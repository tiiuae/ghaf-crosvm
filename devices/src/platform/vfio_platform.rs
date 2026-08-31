// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fs::File;
use std::sync::Arc;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base::error;
use base::pagesize;
use base::AsRawDescriptor;
use base::AsRawDescriptors;
use base::Event;
use base::MappedRegion;
use base::MemoryMapping;
use base::MemoryMappingBuilder;
use base::Protection;
use base::RawDescriptor;
use hypervisor::MemCacheType;
use hypervisor::Vm;
use resources::AddressRange;
use resources::SystemAllocator;
use vfio_sys::*;
use vm_control::api::VmMemoryClient;
use vm_control::DeviceId;
use vm_control::PlatformDeviceId;
use vm_control::VmMemoryDestination;
use vm_control::VmMemorySource;
use vm_memory::GuestAddress;

use crate::vfio::VfioDevice;
use crate::vfio::VfioError;
use crate::vfio::VfioIrq;
use crate::BusAccessInfo;
use crate::BusDevice;
use crate::BusDeviceObj;
use crate::IommuDevType;
use crate::IrqEdgeEvent;
use crate::IrqLevelEvent;
use crate::Suspendable;

struct MmioInfo {
    index: usize,
    start: u64,
    length: u64,
}

fn platform_region_alignment(size: u64) -> Result<u64, resources::Error> {
    size.checked_next_power_of_two()
        .ok_or(resources::Error::OutOfSpace)
        .map(|alignment| alignment.max(pagesize() as u64))
}

fn validate_required_early_mapping(
    map_early: bool,
    flags: u32,
    index: usize,
    start: u64,
) -> Result<()> {
    if map_early && flags & VFIO_REGION_INFO_FLAG_MMAP == 0 {
        bail!("VFIO region {index} at {start:#x} does not support the required early mapping");
    }
    Ok(())
}

fn needs_early_dma_map(iommu: IommuDevType) -> bool {
    matches!(iommu, IommuDevType::NoIommu)
}

pub struct VfioPlatformDevice {
    device: Arc<VfioDevice>,
    interrupt_edge_evt: Vec<IrqEdgeEvent>,
    interrupt_level_evt: Vec<IrqLevelEvent>,
    mmio_regions: Vec<MmioInfo>,
    vm_memory_client: VmMemoryClient,
    iommu: IommuDevType,
    mmio_base: Option<u64>,
    map_early: bool,
    // scratch MemoryMapping to avoid unmap beform vm exit
    mem: Vec<MemoryMapping>,
}

impl BusDevice for VfioPlatformDevice {
    fn device_id(&self) -> DeviceId {
        PlatformDeviceId::VfioPlatformDevice.into()
    }

    fn debug_label(&self) -> String {
        format!("vfio {} device", self.device.device_name())
    }

    fn read(&mut self, info: BusAccessInfo, data: &mut [u8]) {
        self.read_mmio(info.address, data)
    }

    fn write(&mut self, info: BusAccessInfo, data: &[u8]) {
        self.write_mmio(info.address, data)
    }

    fn supports_power_management(&self) -> anyhow::Result<bool> {
        Ok(self.device.supports_pm_low_power())
    }

    fn power_on(&mut self) -> anyhow::Result<()> {
        self.device.pm_low_power_exit()?;
        Ok(())
    }

    fn power_off(&mut self) -> anyhow::Result<()> {
        self.device.pm_low_power_enter()?;
        Ok(())
    }
}

impl Suspendable for VfioPlatformDevice {}

impl BusDeviceObj for VfioPlatformDevice {
    fn as_platform_device(&self) -> Option<&VfioPlatformDevice> {
        Some(self)
    }
    fn as_platform_device_mut(&mut self) -> Option<&mut VfioPlatformDevice> {
        Some(self)
    }
    fn into_platform_device(self: Box<Self>) -> Option<Box<VfioPlatformDevice>> {
        Some(self)
    }
}

impl VfioPlatformDevice {
    /// Constructs a new Vfio Platform device for the given Vfio device
    pub fn new(
        device: VfioDevice,
        vm_memory_client: VmMemoryClient,
        iommu: IommuDevType,
        mmio_base: Option<u64>,
        map_early: bool,
    ) -> Self {
        let dev = Arc::new(device);
        VfioPlatformDevice {
            device: dev,
            interrupt_edge_evt: Vec::new(),
            interrupt_level_evt: Vec::new(),
            mmio_regions: Vec::new(),
            vm_memory_client,
            iommu,
            mmio_base,
            map_early,
            mem: Vec::new(),
        }
    }

    pub fn get_platform_irqs(&self) -> Result<Vec<VfioIrq>, VfioError> {
        self.device.get_irqs()
    }

    pub fn iommu_group(&self) -> u32 {
        self.device.iommu_group()
    }

    pub fn irq_is_automask(&self, irq: &VfioIrq) -> bool {
        irq.flags & VFIO_IRQ_INFO_AUTOMASKED != 0
    }

    fn setup_irq_resample(&mut self, resample_evt: &Event, index: u32) -> Result<()> {
        self.device.irq_mask(index).context("Intx mask failed")?;
        self.device
            .resample_virq_enable(resample_evt, index)
            .context("resample enable failed")?;
        self.device
            .irq_unmask(index)
            .context("Intx unmask failed")?;
        Ok(())
    }

    pub fn assign_edge_platform_irq(&mut self, irq_evt: &IrqEdgeEvent, index: u32) -> Result<()> {
        let interrupt_evt = irq_evt.try_clone().context("failed to clone irq event")?;
        self.device
            .irq_enable(&[Some(interrupt_evt.get_trigger())], index, 0)
            .context("platform irq enable failed")?;
        self.interrupt_edge_evt.push(interrupt_evt);
        Ok(())
    }

    pub fn assign_level_platform_irq(&mut self, irq_evt: &IrqLevelEvent, index: u32) -> Result<()> {
        let interrupt_evt = irq_evt.try_clone().context("failed to clone irq event")?;
        self.device
            .irq_enable(&[Some(interrupt_evt.get_trigger())], index, 0)
            .context("platform irq enable failed")?;
        if let Err(e) = self.setup_irq_resample(interrupt_evt.get_resample(), index) {
            self.disable_irqs(index);
            bail!("failed to set up irq resampling: {}", e);
        }
        self.interrupt_level_evt.push(interrupt_evt);
        Ok(())
    }

    fn find_region(&self, addr: u64) -> Option<MmioInfo> {
        for mmio_info in self.mmio_regions.iter() {
            if addr >= mmio_info.start && addr < mmio_info.start + mmio_info.length {
                return Some(MmioInfo {
                    index: mmio_info.index,
                    start: mmio_info.start,
                    length: mmio_info.length,
                });
            }
        }
        None
    }

    pub fn allocate_regions(
        &mut self,
        resources: &mut SystemAllocator,
    ) -> Result<Vec<(u64, u64)>, resources::Error> {
        if self.mmio_base.is_some() && self.device.get_region_count() != 1 {
            return Err(resources::Error::OutOfBounds);
        }

        let mut ranges = Vec::new();
        for i in 0..self.device.get_region_count() {
            let size = self.device.get_region_size(i);
            let alloc_id = resources.get_anon_alloc();
            let allocator = resources
                .mmio_platform_allocator()
                .ok_or(resources::Error::MissingPlatformMMIOAddresses)?;
            let start_addr = if let Some(base) = self.mmio_base {
                if base % pagesize() as u64 != 0 {
                    return Err(resources::Error::BadAlignment);
                }
                let range = AddressRange::from_start_and_size(base, size)
                    .ok_or(resources::Error::PoolOverflow { base, size })?;
                allocator.allocate_at(range, alloc_id, "vfio_mmio".to_string())?;
                base
            } else {
                let alignment = platform_region_alignment(size)?;
                allocator.allocate_with_align(size, alloc_id, "vfio_mmio".to_string(), alignment)?
            };
            ranges.push((start_addr, size));

            self.mmio_regions.push(MmioInfo {
                index: i,
                start: start_addr,
                length: size,
            });
        }
        Ok(ranges)
    }

    fn region_mmap_early(&self, vm: &dyn Vm, index: usize, start_addr: u64) -> Result<()> {
        if self.device.get_region_flags(index) & VFIO_REGION_INFO_FLAG_MMAP == 0 {
            return Ok(());
        }

        for mmap in &self.device.get_region_mmap(index) {
            let mmap_offset = mmap.offset;
            let mmap_size = mmap.size;
            let guest_map_start = start_addr + mmap_offset;
            let region_offset = self.device.get_region_offset(index);
            let offset = region_offset + mmap_offset;

            let mmap = MemoryMappingBuilder::new(mmap_size as usize)
                .from_file(self.device.device_file())
                .offset(offset)
                .build()
                .with_context(|| {
                    format!(
                        "failed to map VFIO region {index} at {start_addr:#x}, offset {offset:#x}"
                    )
                })?;

            let host = mmap.as_ptr();
            let guest_addr = GuestAddress(guest_map_start);

            // The early path adds a KVM memslot outside GuestMemory after the
            // NoIommu container's one-time static mapping. Map the same range
            // into VFIO explicitly so the passthrough engine can DMA only to
            // this fixed GPA/HPA carveout.
            //
            // SAFETY:
            // mmap owns a valid host mapping of mmap_size bytes and
            // guest_map_start is the non-overlapping GPA allocated for this
            // VFIO region.
            if needs_early_dma_map(self.iommu) {
                unsafe {
                    self.device
                        .vfio_dma_map(guest_map_start, mmap_size, host as u64, true)
                }
                .with_context(|| {
                    format!(
                        "failed to map VFIO DMA for region {index} at {guest_addr}, host {host:?}"
                    )
                })?;
            }

            vm.add_memory_region(
                guest_addr,
                Box::new(mmap),
                false,
                false,
                MemCacheType::CacheCoherent,
            )
            .with_context(|| {
                format!("failed to add VFIO region {index} at {guest_addr}, host {host:?}")
            })?;
        }
        Ok(())
    }

    /// Force adding the MMIO regions to the guest memory space.
    ///
    /// By default, MMIO regions are mapped lazily when the guest first accesses them. Instead,
    /// this function maps them, even if the guest might end up not accessing them. It only runs in
    /// the current thread and can therefore be called before the VM is started.
    pub fn regions_mmap_early(&mut self, vm: &dyn Vm) -> Result<()> {
        for mmio_info in self.mmio_regions.iter() {
            validate_required_early_mapping(
                self.map_early,
                self.device.get_region_flags(mmio_info.index),
                mmio_info.index,
                mmio_info.start,
            )?;
            self.region_mmap_early(vm, mmio_info.index, mmio_info.start)?;
        }
        Ok(())
    }

    pub fn map_early(&self) -> bool {
        self.map_early
    }

    fn region_mmap(&self, index: usize, start_addr: u64) -> Vec<MemoryMapping> {
        let mut mem_map: Vec<MemoryMapping> = Vec::new();
        if self.device.get_region_flags(index) & VFIO_REGION_INFO_FLAG_MMAP != 0 {
            let mmaps = self.device.get_region_mmap(index);
            if mmaps.is_empty() {
                return mem_map;
            }

            for mmap in mmaps.iter() {
                let mmap_offset = mmap.offset;
                let mmap_size = mmap.size;
                let guest_map_start = start_addr + mmap_offset;
                let region_offset = self.device.get_region_offset(index);
                let offset = region_offset + mmap_offset;
                let descriptor = match self.device.device_file().try_clone() {
                    Ok(device_file) => device_file.into(),
                    Err(_) => break,
                };
                match self.vm_memory_client.register_memory(
                    VmMemorySource::Descriptor {
                        descriptor,
                        offset,
                        size: mmap_size,
                    },
                    VmMemoryDestination::GuestPhysicalAddress(guest_map_start),
                    Protection::read_write(),
                    MemCacheType::CacheCoherent,
                ) {
                    Ok(_region) => {
                        // Even if vm has mapped this region, but it is in vm main process,
                        // device process doesn't has this mapping, but vfio_dma_map() need it
                        // in device process, so here map it again.
                        let mmap = match MemoryMappingBuilder::new(mmap_size as usize)
                            .from_file(self.device.device_file())
                            .offset(offset)
                            .build()
                        {
                            Ok(v) => v,
                            Err(_e) => break,
                        };
                        let host = mmap.as_ptr() as u64;
                        // SAFETY:
                        // Safe because the given guest_map_start is valid guest bar address. and
                        // the host pointer is correct and valid guaranteed by MemoryMapping
                        // interface.
                        match unsafe {
                            self.device
                                .vfio_dma_map(guest_map_start, mmap_size, host, true)
                        } {
                            Ok(_) => mem_map.push(mmap),
                            Err(e) => {
                                error!(
                                    "{}, index: {}, start_addr:0x{:x}, host:0x{:x}",
                                    e, index, start_addr, host
                                );
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!("register_memory failed: {}", e);
                        break;
                    }
                }
            }
        }

        mem_map
    }

    fn regions_mmap(&mut self) {
        for mmio_info in self.mmio_regions.iter() {
            let mut mem_map = self.region_mmap(mmio_info.index, mmio_info.start);
            self.mem.append(&mut mem_map);
        }
    }

    fn disable_irqs(&mut self, index: u32) {
        if let Err(e) = self.device.irq_disable(index) {
            error!("Platform irq disable failed: {}", e);
        }
    }

    fn read_mmio(&mut self, addr: u64, data: &mut [u8]) {
        if let Some(mmio_info) = self.find_region(addr) {
            let offset = addr - mmio_info.start;
            let index = mmio_info.index;
            self.device.region_read(index, data, offset);
        }
        // We have no other way than wait for 1st access and then do the mmap,
        // so that next accesses are dual-stage MMU accelerated.
        self.regions_mmap();
    }

    fn write_mmio(&mut self, addr: u64, data: &[u8]) {
        if let Some(mmio_info) = self.find_region(addr) {
            let offset = addr - mmio_info.start;
            let index = mmio_info.index;
            self.device.region_write(index, data, offset);
        }
        // We have no other way than wait for 1st access and then do the mmap,
        // so that next accesses are dual-stage MMU accelerated.
        self.regions_mmap();
    }

    pub fn keep_rds(&self) -> Vec<RawDescriptor> {
        let mut rds = self.device.keep_rds();

        for irq_evt in self.interrupt_edge_evt.iter() {
            rds.extend(irq_evt.as_raw_descriptors());
        }

        for irq_evt in self.interrupt_level_evt.iter() {
            rds.extend(irq_evt.as_raw_descriptors());
        }

        rds.push(self.vm_memory_client.as_raw_descriptor());
        rds
    }

    /// Gets the vfio device backing `File`.
    pub fn device_file(&self) -> &File {
        self.device.device_file()
    }

    /// Returns the DT symbol (node label) of the VFIO device.
    pub fn dt_symbol(&self) -> Option<&str> {
        self.device.dt_symbol()
    }

    /// Returns the type and indentifier (if applicable) of the IOMMU used by this VFIO device and
    /// its master IDs.
    pub fn iommu(&self) -> Option<(IommuDevType, Option<u32>, &[u32])> {
        self.device.iommu()
    }
}

#[cfg(test)]
mod tests {
    use resources::address_allocator::AddressAllocator;
    use resources::Alloc;

    use super::*;

    #[test]
    fn natural_alignment_matches_qemu_gpu_layout() {
        let mut allocator = AddressAllocator::new(
            AddressRange {
                start: 0x6000_0000,
                end: 0x67ff_ffff,
            },
            Some(0x1000),
            None,
        )
        .unwrap();
        allocator
            .allocate_at(
                AddressRange {
                    start: 0x6000_0000,
                    end: 0x63ff_ffff,
                },
                Alloc::Anon(0),
                "vm_hs".to_string(),
            )
            .unwrap();

        let sizes = [
            0x0100_0000,
            0x0100_0000,
            0x0001_0000,
            0x0001_0000,
            0x0001_0000,
            0x0001_0000,
            0x0008_0000,
            0x0004_0000,
            0x0004_0000,
            0x0004_0000,
        ];
        let addresses: Vec<u64> = sizes
            .into_iter()
            .enumerate()
            .map(|(index, size)| {
                allocator
                    .allocate_with_align(
                        size,
                        Alloc::Anon(index + 1),
                        "vfio_mmio".to_string(),
                        platform_region_alignment(size).unwrap(),
                    )
                    .unwrap()
            })
            .collect();

        assert_eq!(
            addresses,
            [
                0x6400_0000,
                0x6500_0000,
                0x6600_0000,
                0x6601_0000,
                0x6602_0000,
                0x6603_0000,
                0x6608_0000,
                0x6604_0000,
                0x6610_0000,
                0x6614_0000,
            ]
        );
    }

    #[test]
    fn fixed_mapping_rejects_collisions() {
        let mut allocator = AddressAllocator::new(
            AddressRange {
                start: 0x6000_0000,
                end: 0x6fff_ffff,
            },
            Some(0x1000),
            None,
        )
        .unwrap();
        let fixed = AddressRange {
            start: 0x6623_0000,
            end: 0x6623_ffff,
        };
        allocator
            .allocate_at(fixed, Alloc::Anon(0), "display".to_string())
            .unwrap();
        assert!(allocator
            .allocate_at(fixed, Alloc::Anon(1), "collision".to_string())
            .is_err());
    }

    #[test]
    fn required_early_mapping_rejects_non_mappable_region() {
        assert!(validate_required_early_mapping(true, 0, 2, 0x6000_0000).is_err());
        assert!(validate_required_early_mapping(false, 0, 2, 0x6000_0000).is_ok());
        assert!(
            validate_required_early_mapping(true, VFIO_REGION_INFO_FLAG_MMAP, 2, 0x6000_0000,)
                .is_ok()
        );
    }

    #[test]
    fn early_dma_map_is_only_for_no_iommu() {
        assert!(needs_early_dma_map(IommuDevType::NoIommu));
        assert!(!needs_early_dma_map(IommuDevType::VirtioIommu));
        assert!(!needs_early_dma_map(IommuDevType::CoIommu));
        assert!(!needs_early_dma_map(IommuDevType::PkvmPviommu));
    }
}
