// Copyright 2026 TII (SSRC)
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TPM backend using the swtpm Unix control socket.

use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;

use base::error;
use base::AsRawDescriptor;
use base::RawDescriptor;
use base::ScmSocket;
use remain::sorted;
use thiserror::Error;

use crate::TpmBackend;

const TPM_HEADER_SIZE: usize = 10;
const TPM_BUFSIZE: usize = 4096;
const SWTPM_CMD_INIT: u32 = 2;
const SWTPM_CMD_SET_DATAFD: u32 = 0x10;
const SWTPM_SUCCESS: u32 = 0;

const TPM_RC_FAILURE_RESPONSE: &[u8] = &[
    0x80, 0x01, // TPM_ST_NO_SESSIONS
    0x00, 0x00, 0x00, 0x0A, // Header size = 10
    0x00, 0x00, 0x01, 0x01, // TPM_RC_FAILURE
];

/// A TPM backend connected to an swtpm `--ctrl type=unixio` socket.
pub struct Swtpm {
    control: ScmSocket<UnixStream>,
    data: UnixStream,
    response: Vec<u8>,
}

impl Swtpm {
    /// Connects to an swtpm Unix control socket and initializes its data channel.
    pub fn connect(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::from_control_stream(UnixStream::connect(path)?)
    }

    fn from_control_stream(control_stream: UnixStream) -> std::io::Result<Self> {
        let mut control = ScmSocket::try_from(control_stream)?;
        let (data, swtpm_data) = UnixStream::pair()?;

        let request = SWTPM_CMD_SET_DATAFD.to_be_bytes();
        let sent = control.send_with_fds(&request, &[swtpm_data.as_raw_descriptor()])?;
        if sent != request.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "short write sending swtpm SET_DATAFD",
            ));
        }
        Self::read_control_result(&mut control)?;
        drop(swtpm_data);

        let mut init_request = Vec::with_capacity(8);
        init_request.extend_from_slice(&SWTPM_CMD_INIT.to_be_bytes());
        init_request.extend_from_slice(&0u32.to_be_bytes());
        control.inner_mut().write_all(&init_request)?;
        Self::read_control_result(&mut control)?;

        Ok(Self {
            control,
            data,
            response: Vec::new(),
        })
    }

    fn read_control_result(control: &mut ScmSocket<UnixStream>) -> std::io::Result<()> {
        let mut response = [0u8; 4];
        control.inner_mut().read_exact(&mut response)?;
        let result = u32::from_be_bytes(response);
        if result == SWTPM_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "swtpm control command failed with result {result:#x}"
            )))
        }
    }

    fn try_execute_command(&mut self, command: &[u8]) -> Result<()> {
        self.data.write_all(command).map_err(Error::WriteCommand)?;

        let mut header = [0u8; TPM_HEADER_SIZE];
        self.data
            .read_exact(&mut header)
            .map_err(Error::ReadResponse)?;

        let response_size = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
        if !(TPM_HEADER_SIZE..=TPM_BUFSIZE).contains(&response_size) {
            return Err(Error::InvalidResponseSize(response_size));
        }

        self.response.resize(response_size, 0);
        self.response[..TPM_HEADER_SIZE].copy_from_slice(&header);
        self.data
            .read_exact(&mut self.response[TPM_HEADER_SIZE..])
            .map_err(Error::ReadResponse)?;
        Ok(())
    }
}

impl TpmBackend for Swtpm {
    fn execute_command<'a>(&'a mut self, command: &[u8]) -> &'a [u8] {
        match self.try_execute_command(command) {
            Ok(()) => &self.response,
            Err(e) => {
                error!("swtpm command failed: {:#}", e);
                TPM_RC_FAILURE_RESPONSE
            }
        }
    }

    fn keep_rds(&self) -> Vec<RawDescriptor> {
        vec![
            self.control.as_raw_descriptor(),
            self.data.as_raw_descriptor(),
        ]
    }
}

#[sorted]
#[derive(Debug, Error)]
enum Error {
    #[error("invalid swtpm response size: {0}")]
    InvalidResponseSize(usize),
    #[error("failed to read swtpm response: {0}")]
    ReadResponse(std::io::Error),
    #[error("failed to write swtpm command: {0}")]
    WriteCommand(std::io::Error),
}

type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
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

    fn create_backend() -> (Swtpm, UnixStream) {
        let (control_client, control_server) = UnixStream::pair().unwrap();
        let control_server = ScmSocket::try_from(control_server).unwrap();
        let server_thread = thread::spawn(move || {
            let mut set_datafd = [0u8; 4];
            let (size, mut fds) = control_server.recv_with_fds(&mut set_datafd, 1).unwrap();
            assert_eq!(size, set_datafd.len());
            assert_eq!(u32::from_be_bytes(set_datafd), SWTPM_CMD_SET_DATAFD);
            assert_eq!(fds.len(), 1);
            control_server
                .inner()
                .write_all(&0u32.to_be_bytes())
                .unwrap();

            let mut init = [0u8; 8];
            control_server.inner().read_exact(&mut init).unwrap();
            assert_eq!(
                u32::from_be_bytes(init[..4].try_into().unwrap()),
                SWTPM_CMD_INIT
            );
            assert_eq!(u32::from_be_bytes(init[4..].try_into().unwrap()), 0);
            control_server
                .inner()
                .write_all(&0u32.to_be_bytes())
                .unwrap();

            UnixStream::from(fds.remove(0))
        });

        let backend = Swtpm::from_control_stream(control_client).unwrap();
        let data_server = server_thread.join().unwrap();
        (backend, data_server)
    }

    #[test]
    fn executes_command() {
        let (mut backend, mut server) = create_backend();
        let server_thread = thread::spawn(move || {
            let mut command = [0u8; GET_RANDOM.len()];
            server.read_exact(&mut command).unwrap();
            assert_eq!(command, GET_RANDOM);
            server.write_all(&GET_RANDOM_RESPONSE[..7]).unwrap();
            server.write_all(&GET_RANDOM_RESPONSE[7..]).unwrap();
        });

        assert_eq!(backend.execute_command(GET_RANDOM), GET_RANDOM_RESPONSE);
        assert_eq!(backend.keep_rds().len(), 2);
        server_thread.join().unwrap();
    }

    #[test]
    fn rejects_invalid_response_size() {
        let (mut backend, mut server) = create_backend();
        let server_thread = thread::spawn(move || {
            let mut command = [0u8; GET_RANDOM.len()];
            server.read_exact(&mut command).unwrap();
            server
                .write_all(&[0x80, 0x01, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00])
                .unwrap();
        });

        assert_eq!(backend.execute_command(GET_RANDOM), TPM_RC_FAILURE_RESPONSE);
        server_thread.join().unwrap();
    }

    #[test]
    fn rejects_failed_control_command() {
        let (control_client, control_server) = UnixStream::pair().unwrap();
        let control_server = ScmSocket::try_from(control_server).unwrap();
        let server_thread = thread::spawn(move || {
            let mut set_datafd = [0u8; 4];
            let (_, _fds) = control_server.recv_with_fds(&mut set_datafd, 1).unwrap();
            control_server
                .inner()
                .write_all(&9u32.to_be_bytes())
                .unwrap();
        });

        let error = match Swtpm::from_control_stream(control_client) {
            Ok(_) => panic!("accepted failed swtpm control command"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("result 0x9"));
        server_thread.join().unwrap();
    }
}
