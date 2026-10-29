// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides a function to lanunches a process of creating ext2 filesystem on memory region
//! asynchronously for pmem-ext2 device.
//!
//! The ext2 file system is created in the memory area for pmem by the following three processes:
//! (a). The main process
//! (b). ext2 process launched by the `launch()` below.
//! (c). The virtio-pmem process
//!
//! By executing mkfs in the multiple processes, mkfs won't block other initalization steps. Also,
//! we can use different seccopm poliy for (b) and (c).
//!
//! The overall workflow is like the followings:
//! 1. At (a): `launch()` is called from (a)
//! 2. At (a): (b) is foked from (a) in `launch()`
//! 3. At (b): The given directory is traversed and metadata is constructed.
//! 4. At (b): File descriptors are sent to (a) with `VmMemoryRequest::MmapAndRegisterMemory`.
//! 5. At (a): mmap() for the file descriptors are called. The reply is sent to (b).
//! 6. At (b): memory slot number is sent to (c).
//! 7. At (c): device activation finished.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::ops::Range;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base::error;
use base::info;
use base::warn;
use base::AsRawDescriptor;
use base::Pid;
use base::SharedMemory;
use base::Tube;
use jail::create_base_minijail;
use jail::create_sandbox_minijail;
use jail::fork_process;
use jail::JailConfig;
use jail::RunAsUser;
use jail::SandboxConfig;
use vm_control::api::VmMemoryClient;
use vm_control::VmMemoryFileMapping;
use vm_memory::GuestAddress;

/// Starts a process to create an ext2 filesystem on a given shared memory region.
pub fn launch(
    mapping_address: GuestAddress,
    vm_memory_client: VmMemoryClient,
    device_tube: Tube, // Connects to a virtio device to send a memory slot number.
    ready_tube: Tube,
    path: &Path,
    ugid: &(Option<u32>, Option<u32>),
    ugid_map: (&str, &str),
    mut builder: ext2::Builder,
    cache: Option<&Path>,
    jail_config: Option<&JailConfig>,
) -> Result<Vec<Pid>> {
    let max_open_files = base::linux::max_open_files()
        .context("failed to get max number of open files")?
        .rlim_max;

    let jail = if let Some(jail_config) = jail_config {
        let mut config = SandboxConfig::new(jail_config, "virtual_ext2");
        config.limit_caps = false;
        config.ugid_map = Some(ugid_map);
        // We want bind mounts from the parent namespaces to propagate into the mkfs's
        // namespace.
        config.remount_mode = Some(libc::MS_SLAVE);
        config.run_as = match *ugid {
            (None, None) => RunAsUser::Unspecified,
            (uid_opt, gid_opt) => RunAsUser::Specified(uid_opt.unwrap_or(0), gid_opt.unwrap_or(0)),
        };
        create_sandbox_minijail(path, max_open_files, &config)?
    } else {
        create_base_minijail(path, max_open_files)?
    };

    // Use "/" in the new mount namespace as the root for mkfs.
    builder.root_dir = Some(std::path::PathBuf::from("/"));

    let shm = SharedMemory::new("pmem_ext2_shm", builder.size as u64)
        .context("failed to create shared memory")?;
    let mut keep_rds = vec![
        shm.as_raw_descriptor(),
        vm_memory_client.as_raw_descriptor(),
        device_tube.as_raw_descriptor(),
        ready_tube.as_raw_descriptor(),
    ];
    let mut pids = Vec::new();
    let mut cache_files = None;
    if let Some(cache) = cache {
        let cache_in = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(cache)
        {
            Ok(f) => Some(f),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!("failed to open {}: {}", cache.display(), e);
                }
                None
            }
        };
        let (pipe_r, pipe_w) = base::pipe().context("failed to create a pipe")?;
        pids.push(spawn_cache_writer(pipe_r, cache, max_open_files)?);
        keep_rds.extend(cache_in.as_ref().map(|f| f.as_raw_descriptor()));
        keep_rds.push(pipe_w.as_raw_descriptor());
        cache_files = Some((cache_in, pipe_w));
    }
    base::syslog::push_descriptors(&mut keep_rds);

    let child_process = fork_process(jail, keep_rds, Some(String::from("mkfs process")), || {
        if let Err(e) = mkfs_callback(
            vm_memory_client,
            mapping_address,
            device_tube,
            ready_tube,
            builder,
            shm,
            cache_files,
        ) {
            error!("failed to create file system: {:#}", e);
            // SAFETY: exit() is trivially safe.
            unsafe { libc::exit(1) };
        }
    })
    .context("failed to fork a process for mkfs")?;
    pids.push(child_process.pid);
    Ok(pids)
}

/// Forks a process that writes what the mkfs process sends through `pipe` to `cache`, so the mkfs
/// jail needs no access to the cache directory.
fn spawn_cache_writer(mut pipe: File, cache: &Path, max_open_files: u64) -> Result<Pid> {
    let cache = cache.to_path_buf();
    let jail = create_base_minijail(Path::new("/"), max_open_files)?;
    let mut keep_rds = vec![pipe.as_raw_descriptor()];
    base::syslog::push_descriptors(&mut keep_rds);
    let child = fork_process(
        jail,
        keep_rds,
        Some(String::from("pmem-ext2 cache")),
        move || {
            // An exit status other than 0 would stop the VM, and a missing cache only costs a build.
            if let Err(e) = write_cache_file(&mut pipe, &cache) {
                error!("failed to write {}: {:#}", cache.display(), e);
            }
        },
    )
    .context("failed to fork a process for the pmem-ext2 cache")?;
    Ok(child.pid)
}

fn write_cache_file(pipe: &mut File, cache: &Path) -> Result<()> {
    let mut data = Vec::new();
    pipe.read_to_end(&mut data)?;
    if data.is_empty() {
        return Ok(());
    }
    let mut tmp = cache.as_os_str().to_owned();
    tmp.push(".tmp");
    // create_new (O_EXCL) fails on any existing path, symlinks included, so remove a leftover first.
    if let Err(e) = std::fs::remove_file(&tmp) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(e.into());
        }
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, cache)?;
    info!("wrote the pmem-ext2 cache {}", cache.display());
    Ok(())
}

/// A callback to create a ext2 file system on `shm`.
/// This is supposed to be run in a jailed child process so operations are sandboxed and limited.
fn mkfs_callback(
    mem_client: VmMemoryClient,
    mapping_address: GuestAddress,
    device_tube: Tube, // Connects to a virtio device to send a memory slot number.
    ready_tube: Tube,
    builder: ext2::Builder,
    shm: SharedMemory,
    cache: Option<(Option<File>, File)>, // (existing cache, pipe to the cache writer)
) -> Result<()> {
    let (cache_in, cache_out) = cache.unzip();
    let restored = cache_in.flatten().and_then(|f| {
        match builder
            .clone()
            .build_on_shm(&shm)
            .and_then(|r| r.restore(f))
        {
            Ok(r) => {
                info!("restored the pmem-ext2 image from the cache");
                Some(r)
            }
            Err(e) => {
                warn!("ignoring the pmem-ext2 cache: {:#}", e);
                None
            }
        }
    });
    let cache_out = cache_out.filter(|_| restored.is_none());
    let mut region = match restored {
        Some(r) => r,
        None => builder
            .clone()
            .build_on_shm(&shm)
            .context("failed to build memory region")?
            .build_mmap_info()
            .context("failed to build ext2")?,
    };
    let cache_out = cache_out.map(|w| {
        let mappings: Vec<ext2::CachedMapping> =
            region.mapping_info.iter().map(Into::into).collect();
        (w, mappings, data_ranges(&shm))
    });
    let file_mappings = std::mem::take(&mut region.mapping_info);

    let file_mapping_info: Vec<_> = file_mappings
        .into_iter()
        .map(|info| VmMemoryFileMapping {
            file: info.file,
            length: info.length,
            mem_offset: info.mem_offset,
            file_offset: info.file_offset as u64,
        })
        .collect();

    let slot = mem_client
        .mmap_and_register_memory(mapping_address, shm, file_mapping_info)
        .context("failed to request mmaping and registering memory")?;
    device_tube
        .send(&slot)
        .context("failed to send VmMemoryRequest::RegisterMemory")?;
    ready_tube
        .send(&())
        .context("failed to report the pmem-ext2 image ready")?;

    // The cache is written only after the VM has its image, so it does not delay the boot.
    if let Some((w, mappings, data)) = cache_out {
        if let Err(e) = data.and_then(|data| region.write_cache(w, &builder, &data, &mappings)) {
            warn!("failed to write the pmem-ext2 cache: {:#}", e);
        }
    }
    // Only this process maps most of the image, so only it can page it out.
    if let Err(e) = region.page_out() {
        warn!("{:#}", e);
    }
    Ok(())
}

/// Returns the allocated ranges of `shm`, skipping its holes without faulting them in.
fn data_ranges(shm: &SharedMemory) -> Result<Vec<Range<usize>>> {
    let fd = shm.as_raw_descriptor();
    let size = shm.size() as i64;
    let mut ranges = Vec::new();
    let mut off = 0;
    while off < size {
        // SAFETY: lseek on a valid descriptor; ENXIO past the last data range ends the loop.
        let start = unsafe { libc::lseek64(fd, off, libc::SEEK_DATA) };
        if start < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            bail!("lseek failed: {}", e);
        }
        // SAFETY: as above.
        let end = unsafe { libc::lseek64(fd, start, libc::SEEK_HOLE) };
        if end < 0 {
            bail!("lseek failed: {}", std::io::Error::last_os_error());
        }
        ranges.push(start as usize..end.min(size) as usize);
        off = end;
    }
    Ok(ranges)
}
