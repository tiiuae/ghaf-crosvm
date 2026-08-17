// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! NVIDIA BPMP guest bridge.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

use base::error;
use vm_control::DeviceId;
use vm_control::PlatformDeviceId;

use crate::BusAccessInfo;
use crate::BusDevice;
use crate::Suspendable;

pub const NVIDIA_BPMP_MMIO_BASE: u64 = 0x090d_0000;
pub const NVIDIA_BPMP_MMIO_SIZE: u64 = 0x1000;

const TX_BUFFER: usize = 0x0000;
const RX_BUFFER: usize = 0x0200;
const TX_SIZE: usize = 0x0400;
const RX_SIZE: usize = 0x0408;
const RETURN_CODE: usize = 0x0410;
const MRQ: usize = 0x0500;
const PROTOCOL_SIZE: usize = 0x0600;
const MESSAGE_SIZE: usize = 0x0200;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct BufferDescriptor {
    data: u64,
    size: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct ResponseDescriptor {
    data: u64,
    size: u64,
    ret: i32,
    reserved: u32,
}

/// Stable userspace ABI shared with the Ghaf `bpmp-host-proxy` driver.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct BpmpProxyWireMessage {
    mrq: u32,
    reserved: u32,
    tx: BufferDescriptor,
    rx: ResponseDescriptor,
}

static_assertions::const_assert_eq!(std::mem::size_of::<BpmpProxyWireMessage>(), 48);

trait BpmpBackend: Send {
    fn transfer(&mut self, message: &mut BpmpProxyWireMessage) -> io::Result<()>;
}

struct HostDevice(File);

impl BpmpBackend for HostDevice {
    fn transfer(&mut self, message: &mut BpmpProxyWireMessage) -> io::Result<()> {
        // SAFETY: The file remains open for the call and `message` points to a writable,
        // correctly sized header. The host driver intentionally uses write(2) bidirectionally.
        let written = unsafe {
            libc::write(
                self.0.as_raw_fd(),
                message as *mut BpmpProxyWireMessage as *const libc::c_void,
                std::mem::size_of::<BpmpProxyWireMessage>(),
            )
        };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        if written as usize != std::mem::size_of::<BpmpProxyWireMessage>() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short BPMP request write: {written} bytes"),
            ));
        }
        Ok(())
    }
}

/// MMIO device that forwards the Ghaf BPMP guest-proxy protocol to a host character device.
pub struct NvidiaBpmpHost {
    backend: Box<dyn BpmpBackend>,
    memory: [u8; PROTOCOL_SIZE],
}

impl NvidiaBpmpHost {
    pub fn new(file: File) -> Self {
        Self::new_with_backend(Box::new(HostDevice(file)))
    }

    fn new_with_backend(backend: Box<dyn BpmpBackend>) -> Self {
        Self {
            backend,
            memory: [0; PROTOCOL_SIZE],
        }
    }

    fn range(offset: u64, len: usize) -> Option<std::ops::Range<usize>> {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(len)?;
        (end <= PROTOCOL_SIZE).then_some(start..end)
    }

    fn read_u64(&self, offset: usize) -> u64 {
        u64::from_le_bytes(self.memory[offset..offset + 8].try_into().unwrap())
    }

    fn complete_with_error(&mut self, err: io::Error) {
        error!("NVIDIA BPMP host transfer failed: {err}");
        self.memory[RETURN_CODE..RETURN_CODE + 4].copy_from_slice(&(-libc::EIO).to_le_bytes());
        self.memory[RX_SIZE..RX_SIZE + 8].copy_from_slice(&0u64.to_le_bytes());
    }

    fn transfer(&mut self, data: &[u8]) {
        let tx_size = self.read_u64(TX_SIZE);
        let rx_size = self.read_u64(RX_SIZE);
        if tx_size > MESSAGE_SIZE as u64 || rx_size > MESSAGE_SIZE as u64 {
            self.complete_with_error(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("BPMP buffer size exceeds {MESSAGE_SIZE}: tx={tx_size}, rx={rx_size}"),
            ));
            return;
        }

        let mut mrq_bytes = [0u8; 4];
        mrq_bytes[..data.len().min(4)].copy_from_slice(&data[..data.len().min(4)]);
        let mut message = BpmpProxyWireMessage {
            mrq: u32::from_le_bytes(mrq_bytes),
            tx: BufferDescriptor {
                data: self.memory[TX_BUFFER..].as_ptr() as u64,
                size: tx_size,
            },
            rx: ResponseDescriptor {
                data: self.memory[RX_BUFFER..].as_mut_ptr() as u64,
                size: rx_size,
                ..Default::default()
            },
            ..Default::default()
        };

        if let Err(err) = self.backend.transfer(&mut message) {
            self.complete_with_error(err);
            return;
        }

        self.memory[RETURN_CODE..RETURN_CODE + 4].copy_from_slice(&message.rx.ret.to_le_bytes());
        self.memory[RX_SIZE..RX_SIZE + 8].copy_from_slice(&message.rx.size.to_le_bytes());
    }
}

impl BusDevice for NvidiaBpmpHost {
    fn debug_label(&self) -> String {
        "nvidia-bpmp-host".to_owned()
    }

    fn device_id(&self) -> DeviceId {
        PlatformDeviceId::NvidiaBpmpHost.into()
    }

    fn read(&mut self, info: BusAccessInfo, data: &mut [u8]) {
        let Some(range) = Self::range(info.offset, data.len()) else {
            error!(
                "NVIDIA BPMP read outside protocol window: {info}, size={}",
                data.len()
            );
            data.fill(0);
            return;
        };
        data.copy_from_slice(&self.memory[range]);
    }

    fn write(&mut self, info: BusAccessInfo, data: &[u8]) {
        let Some(range) = Self::range(info.offset, data.len()) else {
            error!(
                "NVIDIA BPMP write outside protocol window: {info}, size={}",
                data.len()
            );
            return;
        };

        if range.start == MRQ {
            self.transfer(data);
        } else {
            self.memory[range].copy_from_slice(data);
        }
    }
}

impl Suspendable for NvidiaBpmpHost {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sync::Mutex;

    use super::*;
    use crate::Bus;
    use crate::BusType;

    #[derive(Default)]
    struct BackendState {
        mrq: u32,
        tx: Vec<u8>,
        rx_capacity: u64,
        fail: bool,
    }

    struct TestBackend(Arc<Mutex<BackendState>>);

    impl BpmpBackend for TestBackend {
        fn transfer(&mut self, message: &mut BpmpProxyWireMessage) -> io::Result<()> {
            let mut state = self.0.lock();
            state.mrq = message.mrq;
            state.rx_capacity = message.rx.size;
            if state.fail {
                return Err(io::Error::other("backend failure"));
            }
            // SAFETY: The device constructed both pointer and length from its live MMIO buffer.
            let tx = unsafe {
                std::slice::from_raw_parts(message.tx.data as *const u8, message.tx.size as usize)
            };
            state.tx = tx.to_vec();
            let response = [0xde, 0xad, 0xbe, 0xef];
            // SAFETY: The response is smaller than the capacity supplied by this test.
            let rx = unsafe {
                std::slice::from_raw_parts_mut(message.rx.data as *mut u8, response.len())
            };
            rx.copy_from_slice(&response);
            message.rx.size = response.len() as u64;
            message.rx.ret = -7;
            Ok(())
        }
    }

    fn access(offset: usize) -> BusAccessInfo {
        BusAccessInfo {
            offset: offset as u64,
            address: NVIDIA_BPMP_MMIO_BASE + offset as u64,
            id: 0,
        }
    }

    fn device(state: Arc<Mutex<BackendState>>) -> NvidiaBpmpHost {
        NvidiaBpmpHost::new_with_backend(Box::new(TestBackend(state)))
    }

    #[test]
    fn serializes_request_and_returns_response() {
        let state = Arc::new(Mutex::new(BackendState::default()));
        let mut device = device(state.clone());
        device.write(access(TX_BUFFER), &[1, 2, 3]);
        device.write(access(TX_SIZE), &3u64.to_le_bytes());
        device.write(access(RX_SIZE), &16u64.to_le_bytes());
        device.write(access(MRQ), &42u32.to_le_bytes());

        let state = state.lock();
        assert_eq!(state.mrq, 42);
        assert_eq!(state.tx, [1, 2, 3]);
        assert_eq!(state.rx_capacity, 16);
        drop(state);

        let mut ret = [0; 4];
        device.read(access(RETURN_CODE), &mut ret);
        assert_eq!(i32::from_le_bytes(ret), -7);
        let mut rx_size = [0; 8];
        device.read(access(RX_SIZE), &mut rx_size);
        assert_eq!(u64::from_le_bytes(rx_size), 4);
        let mut response = [0; 4];
        device.read(access(RX_BUFFER), &mut response);
        assert_eq!(response, [0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn rejects_out_of_bounds_accesses_and_sizes() {
        let state = Arc::new(Mutex::new(BackendState::default()));
        let mut device = device(state.clone());
        device.write(access(PROTOCOL_SIZE - 1), &[1, 2]);
        let mut outside = [1, 2];
        device.read(access(PROTOCOL_SIZE - 1), &mut outside);
        assert_eq!(outside, [0, 0]);

        device.write(access(TX_SIZE), &((MESSAGE_SIZE + 1) as u64).to_le_bytes());
        device.write(access(MRQ), &1u32.to_le_bytes());
        assert_eq!(state.lock().mrq, 0);

        let mut ret = [0; 4];
        device.read(access(RETURN_CODE), &mut ret);
        assert_eq!(i32::from_le_bytes(ret), -libc::EIO);
    }

    #[test]
    fn reports_backend_errors() {
        let state = Arc::new(Mutex::new(BackendState {
            fail: true,
            ..Default::default()
        }));
        let mut device = device(state);
        device.write(access(RX_SIZE), &8u64.to_le_bytes());
        device.write(access(MRQ), &1u32.to_le_bytes());

        let mut ret = [0; 4];
        device.read(access(RETURN_CODE), &mut ret);
        assert_eq!(i32::from_le_bytes(ret), -libc::EIO);
        let mut rx_size = [1; 8];
        device.read(access(RX_SIZE), &mut rx_size);
        assert_eq!(u64::from_le_bytes(rx_size), 0);
    }

    #[test]
    fn fixed_mmio_range_rejects_collisions() {
        let bus = Bus::new(BusType::Mmio);
        let first = Arc::new(Mutex::new(device(Arc::new(Mutex::new(
            BackendState::default(),
        )))));
        let second = Arc::new(Mutex::new(device(Arc::new(Mutex::new(
            BackendState::default(),
        )))));

        bus.insert(first, NVIDIA_BPMP_MMIO_BASE, NVIDIA_BPMP_MMIO_SIZE)
            .unwrap();
        assert!(bus
            .insert(second, NVIDIA_BPMP_MMIO_BASE, NVIDIA_BPMP_MMIO_SIZE)
            .is_err());
    }
}
