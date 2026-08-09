// Copyright 2026 TII (SSRC)
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TPM backend forwarding commands to a Linux TPM character device.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;

use base::error;
use base::AsRawDescriptor;
use base::RawDescriptor;
use remain::sorted;
use thiserror::Error;

use crate::TpmBackend;

const TPM_HEADER_SIZE: usize = 10;
const TPM_BUFSIZE: usize = 4096;
const TPM_ST_NO_SESSIONS: u16 = 0x8001;
const TPM_ST_SESSIONS: u16 = 0x8002;

const TPM_RC_FAILURE_RESPONSE: &[u8] = &[
    0x80, 0x01, // TPM_ST_NO_SESSIONS
    0x00, 0x00, 0x00, 0x0A, // Header size = 10
    0x00, 0x00, 0x01, 0x01, // TPM_RC_FAILURE
];

/// A TPM backend connected directly to a Linux TPM resource-manager device.
pub struct HostTpm {
    device: File,
    response: Vec<u8>,
}

impl HostTpm {
    /// Opens a host TPM character device for command forwarding.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let device = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self::from_file(device))
    }

    fn from_file(device: File) -> Self {
        Self {
            device,
            response: vec![0; TPM_BUFSIZE],
        }
    }

    fn validate_header(message: &[u8]) -> Result<usize> {
        if message.len() < TPM_HEADER_SIZE {
            return Err(Error::ShortMessage(message.len()));
        }

        let tag = u16::from_be_bytes(message[..2].try_into().unwrap());
        if !matches!(tag, TPM_ST_NO_SESSIONS | TPM_ST_SESSIONS) {
            return Err(Error::InvalidTag(tag));
        }

        let size = u32::from_be_bytes(message[2..6].try_into().unwrap()) as usize;
        if !(TPM_HEADER_SIZE..=TPM_BUFSIZE).contains(&size) {
            return Err(Error::InvalidMessageSize(size));
        }
        Ok(size)
    }

    fn try_execute_command(&mut self, command: &[u8]) -> Result<()> {
        let command_size = Self::validate_header(command)?;
        if command_size != command.len() {
            return Err(Error::LengthMismatch {
                header: command_size,
                actual: command.len(),
            });
        }

        // A TPM character device treats one write as one complete command.  Do not use
        // write_all(), because retrying a short write would turn the remainder into a second
        // command.
        let written = self.device.write(command).map_err(Error::WriteCommand)?;
        if written != command.len() {
            return Err(Error::ShortWrite {
                expected: command.len(),
                actual: written,
            });
        }

        // Linux TPM character devices return one complete response from a single read.  Reading
        // the header separately can discard the response body on these devices, so use the full
        // TPM buffer and validate the embedded length afterwards.
        self.response.resize(TPM_BUFSIZE, 0);
        let response_size = self
            .device
            .read(&mut self.response)
            .map_err(Error::ReadResponse)?;
        let header_size = Self::validate_header(&self.response[..response_size])?;
        if header_size != response_size {
            return Err(Error::LengthMismatch {
                header: header_size,
                actual: response_size,
            });
        }
        self.response.truncate(response_size);
        Ok(())
    }
}

impl TpmBackend for HostTpm {
    fn execute_command<'a>(&'a mut self, command: &[u8]) -> &'a [u8] {
        match self.try_execute_command(command) {
            Ok(()) => &self.response,
            Err(e) => {
                error!("host TPM command failed: {:#}", e);
                TPM_RC_FAILURE_RESPONSE
            }
        }
    }

    fn keep_rds(&self) -> Vec<RawDescriptor> {
        vec![self.device.as_raw_descriptor()]
    }
}

#[sorted]
#[derive(Debug, Error)]
enum Error {
    #[error("invalid TPM message size: {0}")]
    InvalidMessageSize(usize),
    #[error("invalid TPM message tag: {0:#x}")]
    InvalidTag(u16),
    #[error("TPM message length mismatch: header says {header}, transport returned {actual}")]
    LengthMismatch { header: usize, actual: usize },
    #[error("failed to read TPM response: {0}")]
    ReadResponse(std::io::Error),
    #[error("short TPM message: {0} bytes")]
    ShortMessage(usize),
    #[error("short TPM command write: expected {expected} bytes, wrote {actual}")]
    ShortWrite { expected: usize, actual: usize },
    #[error("failed to write TPM command: {0}")]
    WriteCommand(std::io::Error),
}

type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use std::os::fd::FromRawFd;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;
    use std::thread;

    use super::*;

    const GET_RANDOM: &[u8] = &[
        0x80, 0x01, // TPM_ST_NO_SESSIONS
        0x00, 0x00, 0x00, 0x0C, // command size
        0x00, 0x00, 0x01, 0x7B, // TPM_CC_GetRandom
        0x00, 0x04, // requested bytes
    ];
    const GET_RANDOM_RESPONSE: &[u8] = &[
        0x80, 0x01, // TPM_ST_NO_SESSIONS
        0x00, 0x00, 0x00, 0x10, // response size
        0x00, 0x00, 0x00, 0x00, // TPM_RC_SUCCESS
        0x00, 0x04, 0xDE, 0xAD, 0xBE, 0xEF,
    ];

    fn create_backend() -> (HostTpm, UnixStream) {
        let (client, server) = UnixStream::pair().unwrap();
        // SAFETY: into_raw_fd transfers unique ownership of the descriptor to File.
        let client = unsafe { File::from_raw_fd(client.into_raw_fd()) };
        (HostTpm::from_file(client), server)
    }

    #[test]
    fn forwards_complete_command() {
        let (mut backend, mut server) = create_backend();
        let server_thread = thread::spawn(move || {
            let mut command = [0u8; GET_RANDOM.len()];
            server.read_exact(&mut command).unwrap();
            assert_eq!(command, GET_RANDOM);
            server.write_all(GET_RANDOM_RESPONSE).unwrap();
        });

        assert_eq!(backend.execute_command(GET_RANDOM), GET_RANDOM_RESPONSE);
        assert_eq!(backend.keep_rds().len(), 1);
        server_thread.join().unwrap();
    }

    #[test]
    fn rejects_short_response() {
        let (mut backend, mut server) = create_backend();
        let server_thread = thread::spawn(move || {
            let mut command = [0u8; GET_RANDOM.len()];
            server.read_exact(&mut command).unwrap();
            server.write_all(&GET_RANDOM_RESPONSE[..8]).unwrap();
        });

        assert_eq!(backend.execute_command(GET_RANDOM), TPM_RC_FAILURE_RESPONSE);
        server_thread.join().unwrap();
    }

    #[test]
    fn rejects_malformed_response_length() {
        let (mut backend, mut server) = create_backend();
        let server_thread = thread::spawn(move || {
            let mut command = [0u8; GET_RANDOM.len()];
            server.read_exact(&mut command).unwrap();
            let mut response = GET_RANDOM_RESPONSE.to_vec();
            response[5] = 0x20;
            server.write_all(&response).unwrap();
        });

        assert_eq!(backend.execute_command(GET_RANDOM), TPM_RC_FAILURE_RESPONSE);
        server_thread.join().unwrap();
    }

    #[test]
    fn reports_transport_failure() {
        let (mut backend, server) = create_backend();
        drop(server);

        assert_eq!(backend.execute_command(GET_RANDOM), TPM_RC_FAILURE_RESPONSE);
    }

    #[test]
    fn rejects_malformed_command_length() {
        let (mut backend, _server) = create_backend();
        let mut command = GET_RANDOM.to_vec();
        command[5] = 0x10;

        assert_eq!(backend.execute_command(&command), TPM_RC_FAILURE_RESPONSE);
    }
}
