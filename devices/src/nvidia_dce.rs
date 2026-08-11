// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! NVIDIA DCE guest bridge.

use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

use base::error;
use base::EventWaitResult;
use base::MemoryMapping;
use vm_control::DeviceId;
use vm_control::PlatformDeviceId;

use crate::BusAccessInfo;
use crate::BusDevice;
use crate::IrqLevelEvent;
use crate::Suspendable;

pub const NVIDIA_DCE_MMIO_BASE: u64 = 0x090e_0000;
pub const NVIDIA_DCE_MMIO_SIZE: u64 = 0x1_0000;
pub const NVIDIA_DCE_EVENT_PAYLOAD_BASE: u64 = NVIDIA_DCE_MMIO_BASE + EVENT_BUFFER as u64;
pub const NVIDIA_DCE_EVENT_PAYLOAD_SIZE: u64 = EVENT_MAX as u64;

const TX_BUFFER: usize = 0x0000;
const RX_BUFFER: usize = 0x1000;
const TX_SIZE: usize = 0x2000;
const RX_SIZE: usize = 0x2008;
const RETURN_CODE: usize = 0x2010;
const INTERFACE: usize = 0x2018;
const DOORBELL: usize = 0x2100;
const EVENT_SEQUENCE: usize = 0x3000;
const EVENT_INTERFACE: usize = 0x3004;
const EVENT_SIZE: usize = 0x3008;
const EVENT_ACK: usize = 0x300c;
const EVENT_BUFFER: usize = 0x4000;
const EVENT_MAX: usize = 0x1000;
const PROTOCOL_SIZE: usize = 0x5000;
const MAX_PAYLOAD: usize = 0x1000;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct BufferDescriptor {
    data: u64,
    size: u64,
}

/// Stable userspace ABI shared with the Ghaf `dce-host-proxy` driver.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct DceProxyWireMessage {
    interface: u32,
    reserved: u32,
    tx: BufferDescriptor,
    rx: BufferDescriptor,
    ret: i32,
    reserved2: u32,
}

static_assertions::const_assert_eq!(std::mem::size_of::<DceProxyWireMessage>(), 48);

#[repr(C)]
#[derive(Debug)]
struct DceHostEvent {
    interface: u32,
    size: u32,
    data: [u8; EVENT_MAX],
}

static_assertions::const_assert_eq!(std::mem::size_of::<DceHostEvent>(), 4104);

trait DceBackend: Send {
    fn transfer(&mut self, message: &mut DceProxyWireMessage) -> io::Result<()>;
}

struct HostDevice(File);

impl DceBackend for HostDevice {
    fn transfer(&mut self, message: &mut DceProxyWireMessage) -> io::Result<()> {
        // SAFETY: The file remains open for the call and `message` points to the native,
        // writable 48-byte ABI expected by the host driver. The write is bidirectional.
        let written = unsafe {
            libc::write(
                self.0.as_raw_fd(),
                message as *mut DceProxyWireMessage as *const libc::c_void,
                std::mem::size_of::<DceProxyWireMessage>(),
            )
        };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        if written as usize != std::mem::size_of::<DceProxyWireMessage>() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short DCE request write: {written} bytes"),
            ));
        }
        Ok(())
    }
}

struct DceState {
    control: [u8; EVENT_BUFFER],
    sequence: u32,
    acknowledged: u32,
}

struct SharedState {
    state: Mutex<DceState>,
    ack: Condvar,
    event_payload: Arc<MemoryMapping>,
    irq: IrqLevelEvent,
    stopping: AtomicBool,
}

/// MMIO device that forwards DCE IPC to the host and publishes asynchronous events.
pub struct NvidiaDceHost {
    backend: Box<dyn DceBackend>,
    event_file: Option<File>,
    event_thread: Option<JoinHandle<()>>,
    shared: Arc<SharedState>,
}

impl NvidiaDceHost {
    pub fn new(file: File, event_payload: MemoryMapping, irq: IrqLevelEvent) -> io::Result<Self> {
        let event_file = file.try_clone()?;
        Ok(Self::new_with_backend(
            Box::new(HostDevice(file)),
            Some(event_file),
            event_payload,
            irq,
        ))
    }

    fn new_with_backend(
        backend: Box<dyn DceBackend>,
        event_file: Option<File>,
        event_payload: MemoryMapping,
        irq: IrqLevelEvent,
    ) -> Self {
        Self {
            backend,
            event_file,
            event_thread: None,
            shared: Arc::new(SharedState {
                state: Mutex::new(DceState {
                    control: [0; EVENT_BUFFER],
                    sequence: 0,
                    acknowledged: 0,
                }),
                ack: Condvar::new(),
                event_payload: Arc::new(event_payload),
                irq,
                stopping: AtomicBool::new(false),
            }),
        }
    }

    pub fn start_event_thread(&mut self) -> io::Result<()> {
        if self.event_thread.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "DCE event thread is already running",
            ));
        }
        let event_file = self.event_file.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "DCE event backend is unavailable")
        })?;
        let shared = self.shared.clone();
        self.event_thread = Some(
            std::thread::Builder::new()
                .name("dce-event".to_string())
                .spawn(move || event_loop(event_file, shared))?,
        );
        Ok(())
    }

    fn range(offset: u64, len: usize, limit: usize) -> Option<std::ops::Range<usize>> {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(len)?;
        (end <= limit).then_some(start..end)
    }

    fn read_u64(state: &DceState, offset: usize) -> u64 {
        u64::from_le_bytes(state.control[offset..offset + 8].try_into().unwrap())
    }

    fn complete_with_error(state: &mut DceState, err: io::Error) {
        error!("NVIDIA DCE host transfer failed: {err}");
        state.control[RETURN_CODE..RETURN_CODE + 4].copy_from_slice(&(-libc::EIO).to_le_bytes());
        state.control[RX_SIZE..RX_SIZE + 8].copy_from_slice(&0u64.to_le_bytes());
    }

    fn transfer(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        let tx_size = Self::read_u64(&state, TX_SIZE);
        let rx_size = Self::read_u64(&state, RX_SIZE);
        if tx_size > MAX_PAYLOAD as u64 || rx_size > MAX_PAYLOAD as u64 {
            Self::complete_with_error(
                &mut state,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("DCE buffer size exceeds {MAX_PAYLOAD}: tx={tx_size}, rx={rx_size}"),
                ),
            );
            return;
        }

        let interface =
            u32::from_le_bytes(state.control[INTERFACE..INTERFACE + 4].try_into().unwrap());
        let mut message = DceProxyWireMessage {
            interface,
            tx: BufferDescriptor {
                data: state.control[TX_BUFFER..].as_ptr() as u64,
                size: tx_size,
            },
            rx: BufferDescriptor {
                data: state.control[RX_BUFFER..].as_mut_ptr() as u64,
                size: rx_size,
            },
            ..Default::default()
        };

        if let Err(err) = self.backend.transfer(&mut message) {
            Self::complete_with_error(&mut state, err);
            return;
        }

        state.control[RETURN_CODE..RETURN_CODE + 4].copy_from_slice(&message.ret.to_le_bytes());
        state.control[RX_SIZE..RX_SIZE + 8].copy_from_slice(&message.rx.size.to_le_bytes());
    }
}

impl BusDevice for NvidiaDceHost {
    fn debug_label(&self) -> String {
        "nvidia-dce-host".to_owned()
    }

    fn device_id(&self) -> DeviceId {
        PlatformDeviceId::NvidiaDceHost.into()
    }

    fn read(&mut self, info: BusAccessInfo, data: &mut [u8]) {
        if Self::range(info.offset, data.len(), PROTOCOL_SIZE).is_none() {
            data.fill(0);
            return;
        }
        if info.offset >= EVENT_BUFFER as u64 {
            let Some(range) = Self::range(info.offset - EVENT_BUFFER as u64, data.len(), EVENT_MAX)
            else {
                data.fill(0);
                return;
            };
            if self
                .shared
                .event_payload
                .read_slice(data, range.start)
                .is_err()
            {
                data.fill(0);
            }
            return;
        }

        let Some(range) = Self::range(info.offset, data.len(), EVENT_BUFFER) else {
            data.fill(0);
            return;
        };
        data.copy_from_slice(&self.shared.state.lock().unwrap().control[range]);
    }

    fn write(&mut self, info: BusAccessInfo, data: &[u8]) {
        if Self::range(info.offset, data.len(), PROTOCOL_SIZE).is_none() {
            return;
        }
        if info.offset >= EVENT_BUFFER as u64 {
            let Some(range) = Self::range(info.offset - EVENT_BUFFER as u64, data.len(), EVENT_MAX)
            else {
                return;
            };
            let _ = self.shared.event_payload.write_slice(data, range.start);
            return;
        }

        let Some(range) = Self::range(info.offset, data.len(), EVENT_BUFFER) else {
            return;
        };
        if range.start == DOORBELL {
            self.transfer();
            return;
        }

        let mut state = self.shared.state.lock().unwrap();
        state.control[range.clone()].copy_from_slice(data);
        if range.start == EVENT_ACK {
            let ack =
                u32::from_le_bytes(state.control[EVENT_ACK..EVENT_ACK + 4].try_into().unwrap());
            if ack == state.sequence {
                state.acknowledged = ack;
                self.shared.ack.notify_all();
            }
        }
    }
}

impl Suspendable for NvidiaDceHost {}

impl Drop for NvidiaDceHost {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.ack.notify_all();
        if let Some(thread) = self.event_thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_host_event(file: &File) -> io::Result<Option<DceHostEvent>> {
    let mut pollfd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pollfd` is valid for the duration of the call.
    let ready = unsafe { libc::poll(&mut pollfd, 1, 200) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    if ready == 0 || pollfd.revents & libc::POLLIN == 0 {
        return Ok(None);
    }

    let mut event = MaybeUninit::<DceHostEvent>::zeroed();
    // SAFETY: `event` points to writable storage of exactly the native event ABI size.
    let read = unsafe {
        libc::read(
            file.as_raw_fd(),
            event.as_mut_ptr() as *mut libc::c_void,
            std::mem::size_of::<DceHostEvent>(),
        )
    };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    let header_size = std::mem::offset_of!(DceHostEvent, data);
    if (read as usize) < header_size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("short DCE event read: {read} bytes"),
        ));
    }
    // SAFETY: The object was zero-initialized before the host filled its prefix, and every bit
    // pattern is valid for its fields.
    let event = unsafe { event.assume_init() };
    if event.size as usize > EVENT_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("DCE event payload is too large: {}", event.size),
        ));
    }
    let expected = header_size + event.size as usize;
    if read as usize != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("DCE event length mismatch: read {read}, header reports {expected}"),
        ));
    }
    Ok(Some(event))
}

fn publish_event(shared: &SharedState, event: &DceHostEvent) -> io::Result<u32> {
    shared
        .event_payload
        .write_slice(&event.data[..event.size as usize], 0)
        .map_err(io::Error::other)?;
    let mut state = shared.state.lock().unwrap();
    state.control[EVENT_INTERFACE..EVENT_INTERFACE + 4]
        .copy_from_slice(&event.interface.to_le_bytes());
    state.control[EVENT_SIZE..EVENT_SIZE + 4].copy_from_slice(&event.size.to_le_bytes());
    state.sequence = state.sequence.wrapping_add(1).max(1);
    let sequence = state.sequence;
    state.control[EVENT_SEQUENCE..EVENT_SEQUENCE + 4].copy_from_slice(&sequence.to_le_bytes());
    drop(state);
    shared.irq.trigger().map_err(io::Error::other)?;
    Ok(sequence)
}

fn wait_for_ack(shared: &SharedState, sequence: u32) {
    let mut state = shared.state.lock().unwrap();
    while state.acknowledged != sequence && !shared.stopping.load(Ordering::Acquire) {
        let (new_state, _) = shared
            .ack
            .wait_timeout(state, Duration::from_millis(10))
            .unwrap();
        state = new_state;
        if state.acknowledged != sequence
            && shared
                .irq
                .get_resample()
                .wait_timeout(Duration::ZERO)
                .is_ok_and(|result| result == EventWaitResult::Signaled)
        {
            let _ = shared.irq.trigger();
        }
    }
}

fn event_loop(event_file: File, shared: Arc<SharedState>) {
    while !shared.stopping.load(Ordering::Acquire) {
        match read_host_event(&event_file) {
            Ok(Some(event)) => match publish_event(&shared, &event) {
                Ok(sequence) => wait_for_ack(&shared, sequence),
                Err(err) => error!("failed to publish NVIDIA DCE event: {err}"),
            },
            Ok(None) => {}
            Err(err) => error!("failed to read NVIDIA DCE event: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex as StdMutex;

    use base::MemoryMappingBuilder;

    use super::*;

    #[derive(Default)]
    struct BackendState {
        interface: u32,
        tx: Vec<u8>,
        rx_capacity: u64,
        fail: bool,
    }

    struct TestBackend(Arc<StdMutex<BackendState>>);

    impl DceBackend for TestBackend {
        fn transfer(&mut self, message: &mut DceProxyWireMessage) -> io::Result<()> {
            let mut state = self.0.lock().unwrap();
            state.interface = message.interface;
            state.rx_capacity = message.rx.size;
            if state.fail {
                return Err(io::Error::other("backend failure"));
            }
            // SAFETY: The device constructed both pointer and length from its live control buffer.
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
            message.ret = -7;
            Ok(())
        }
    }

    fn access(offset: usize) -> BusAccessInfo {
        BusAccessInfo {
            offset: offset as u64,
            address: NVIDIA_DCE_MMIO_BASE + offset as u64,
            id: 0,
        }
    }

    fn device(state: Arc<StdMutex<BackendState>>) -> NvidiaDceHost {
        NvidiaDceHost::new_with_backend(
            Box::new(TestBackend(state)),
            None,
            MemoryMappingBuilder::new(EVENT_MAX).build().unwrap(),
            IrqLevelEvent::new().unwrap(),
        )
    }

    #[test]
    fn serializes_request_and_returns_response() {
        let state = Arc::new(StdMutex::new(BackendState::default()));
        let mut device = device(state.clone());
        device.write(access(TX_BUFFER), &[1, 2, 3]);
        device.write(access(TX_SIZE), &3u64.to_le_bytes());
        device.write(access(RX_SIZE), &16u64.to_le_bytes());
        device.write(access(INTERFACE), &9u32.to_le_bytes());
        device.write(access(DOORBELL), &1u32.to_le_bytes());

        let state = state.lock().unwrap();
        assert_eq!(state.interface, 9);
        assert_eq!(state.tx, [1, 2, 3]);
        assert_eq!(state.rx_capacity, 16);
        drop(state);

        let mut ret = [0; 4];
        device.read(access(RETURN_CODE), &mut ret);
        assert_eq!(i32::from_le_bytes(ret), -7);
        let mut response = [0; 4];
        device.read(access(RX_BUFFER), &mut response);
        assert_eq!(response, [0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn rejects_bad_sizes_bounds_and_backend_errors() {
        let state = Arc::new(StdMutex::new(BackendState::default()));
        let mut device = device(state.clone());
        device.write(access(TX_SIZE), &((MAX_PAYLOAD + 1) as u64).to_le_bytes());
        device.write(access(DOORBELL), &1u32.to_le_bytes());
        assert!(state.lock().unwrap().tx.is_empty());

        let mut ret = [0; 4];
        device.read(access(RETURN_CODE), &mut ret);
        assert_eq!(i32::from_le_bytes(ret), -libc::EIO);
        let mut outside = [1, 2];
        device.read(access(PROTOCOL_SIZE - 1), &mut outside);
        assert_eq!(outside, [0, 0]);

        state.lock().unwrap().fail = true;
        device.write(access(TX_SIZE), &0u64.to_le_bytes());
        device.write(access(RX_SIZE), &8u64.to_le_bytes());
        device.write(access(DOORBELL), &1u32.to_le_bytes());
        let mut rx_size = [1; 8];
        device.read(access(RX_SIZE), &mut rx_size);
        assert_eq!(u64::from_le_bytes(rx_size), 0);
    }

    #[test]
    fn publishes_event_and_observes_ack() {
        let state = Arc::new(StdMutex::new(BackendState::default()));
        let mut device = device(state);
        let event = DceHostEvent {
            interface: 3,
            size: 4,
            data: {
                let mut data = [0; EVENT_MAX];
                data[..4].copy_from_slice(&[1, 2, 3, 4]);
                data
            },
        };
        let sequence = publish_event(&device.shared, &event).unwrap();
        assert_eq!(
            device
                .shared
                .irq
                .get_trigger()
                .wait_timeout(Duration::ZERO)
                .unwrap(),
            EventWaitResult::Signaled
        );
        let mut payload = [0; 4];
        device.read(access(EVENT_BUFFER), &mut payload);
        assert_eq!(payload, [1, 2, 3, 4]);
        device.write(access(EVENT_ACK), &sequence.to_le_bytes());
        assert_eq!(device.shared.state.lock().unwrap().acknowledged, sequence);
    }

    #[test]
    fn reads_variable_length_host_events() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let mut bytes = Vec::from(3u32.to_ne_bytes());
        bytes.extend_from_slice(&4u32.to_ne_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        sender.write_all(&bytes).unwrap();

        let event = read_host_event(&File::from(OwnedFd::from(receiver)))
            .unwrap()
            .unwrap();
        assert_eq!(event.interface, 3);
        assert_eq!(event.size, 4);
        assert_eq!(&event.data[..4], &[1, 2, 3, 4]);

        let (mut sender, receiver) = UnixStream::pair().unwrap();
        bytes.pop();
        sender.write_all(&bytes).unwrap();
        drop(sender);
        let error = read_host_event(&File::from(OwnedFd::from(receiver))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
