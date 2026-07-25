//! Serial port redirection (MS-RDPESP) over the static `serial` virtual channel.
//!
//! This is a simplified implementation of the RDP serial-port redirection
//! protocol. It brings the channel up through the announce/device-completion
//! handshake and then forwards raw read/write/control requests between the
//! server and a local serial-port backend supplied by the platform.
//!
//! The protocol machine is sans-I/O: [`SerialChannel::process`] consumes one
//! complete channel PDU and returns any responses to send. The platform backend
//! implements [`SerialPort`] and is called for `IO_REQUEST` PDUs.

use std::collections::HashMap;

/// A local serial-port backend. The platform supplies this to bridge the RDP
/// channel to a real COM port.
pub trait SerialPort {
    /// Open the port. Returns `true` on success.
    fn open(&mut self) -> bool;
    /// Read up to `len` bytes. Returns the bytes read, or an empty vec on EOF/error.
    fn read(&mut self, len: usize) -> Vec<u8>;
    /// Write bytes. Returns the count written (may be 0).
    fn write(&mut self, data: &[u8]) -> u32;
    /// Close the port.
    fn close(&mut self);
}

/// A no-op backend used in tests and when no COM port is configured.
pub struct NullSerialPort;
impl SerialPort for NullSerialPort {
    fn open(&mut self) -> bool { false }
    fn read(&mut self, _len: usize) -> Vec<u8> { Vec::new() }
    fn write(&mut self, _data: &[u8]) -> u32 { 0 }
    fn close(&mut self) {}
}

/// Channel state machine.
pub struct SerialChannel {
    state: State,
    /// Server-assigned device ID → local port backend.
    ports: HashMap<u32, Box<dyn SerialPort + Send>>,
    /// The platform-supplied port to associate with the first announced device.
    /// Taken on the first `DEVICE_ANNOUNCE`.
    default_port: Option<Box<dyn SerialPort + Send>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting to send our client announce.
    Idle,
    /// Sent client announce, waiting for server announce.
    AwaitingServerAnnounce,
    /// Handshake complete; processing device announces and I/O.
    Ready,
}

/// Major PDU type identifiers.
const CLIENT_ANNOUNCE_REQUEST: u16 = 0x0001;
const SERVER_ANNOUNCE_REQUEST: u16 = 0x0002;
const CLIENT_ANNOUNCE_REPLY: u16 = 0x0003;
const DEVICE_ANNOUNCE: u16 = 0x0004;
const DEVICE_COMPLETION: u16 = 0x0005;
const IO_REQUEST: u16 = 0x0006;
const IO_RESPONSE: u16 = 0x0007;

/// Common RDPESP version.
const RDPESP_VERSION: u32 = 0x0000_0001;

impl SerialChannel {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            ports: HashMap::new(),
            default_port: None,
        }
    }

    /// Install the local serial port backend. Call before the channel handshake
    /// runs; the first `DEVICE_ANNOUNCE` takes ownership of this port.
    pub fn set_port(&mut self, port: Box<dyn SerialPort + Send>) {
        self.default_port = Some(port);
    }

    /// Initial client announce PDU to send when the channel is opened.
    pub fn initial_announce() -> Vec<u8> {
        let mut v = Vec::with_capacity(6);
        v.extend_from_slice(&CLIENT_ANNOUNCE_REQUEST.to_le_bytes());
        v.extend_from_slice(&RDPESP_VERSION.to_le_bytes());
        v
    }

    /// Process one complete inbound PDU, returning any outbound PDUs.
    pub fn process(&mut self,
        pdu: &[u8],
    ) -> Vec<Vec<u8>> {
        if pdu.len() < 2 {
            return Vec::new();
        }
        let msg_type = u16::from_le_bytes([pdu[0], pdu[1]]);
        match msg_type {
            SERVER_ANNOUNCE_REQUEST => {
                tracing::debug!("serial: server announce received");
                self.state = State::Ready;
                vec![Self::client_announce_reply()]
            }
            DEVICE_ANNOUNCE => {
                if pdu.len() < 10 {
                    return Vec::new();
                }
                let device_id = u32::from_le_bytes([pdu[2], pdu[3], pdu[4], pdu[5]]);
                let _port_type = u32::from_le_bytes([pdu[6], pdu[7], pdu[8], pdu[9]]);
                let name = String::from_utf8_lossy(&pdu.get(10..).unwrap_or(&[]),
                );
                let name = name.trim_end_matches('\0');
                tracing::info!(device_id, name, "serial: device announced");
                // Attach the default port (the platform supplies one per
                // configured `--serial` path). A production client would map
                // device names to distinct OS handles.
                let mut port = self.default_port.take().unwrap_or_else(|| Box::new(NullSerialPort));
                let opened = port.open();
                self.ports.insert(device_id, port);
                vec![Self::device_completion(device_id, opened)]
            }
            IO_REQUEST => {
                if pdu.len() < 12 {
                    return Vec::new();
                }
                let device_id = u32::from_le_bytes([pdu[2], pdu[3], pdu[4], pdu[5]]);
                let major_func = u32::from_le_bytes([pdu[6], pdu[7], pdu[8], pdu[9]]);
                let _minor_func = u32::from_le_bytes([pdu[10], pdu[11], pdu[12], pdu[13]]);
                let data = pdu.get(14..).unwrap_or(&[]);
                self.handle_io_request(device_id, major_func, data)
            }
            _ => {
                tracing::debug!(msg_type, "serial: unknown PDU type");
                Vec::new()
            }
        }
    }

    fn client_announce_reply() -> Vec<u8> {
        let mut v = Vec::with_capacity(6);
        v.extend_from_slice(&CLIENT_ANNOUNCE_REPLY.to_le_bytes());
        v.extend_from_slice(&RDPESP_VERSION.to_le_bytes());
        v
    }

    fn device_completion(device_id: u32, success: bool) -> Vec<u8> {
        let mut v = Vec::with_capacity(12);
        v.extend_from_slice(&DEVICE_COMPLETION.to_le_bytes());
        v.extend_from_slice(&device_id.to_le_bytes());
        v.extend_from_slice(&if success { 0u32 } else { 1u32 }.to_le_bytes());
        v
    }

    fn handle_io_request(
        &mut self,
        device_id: u32,
        major_func: u32,
        data: &[u8],
    ) -> Vec<Vec<u8>> {
        // Major function codes mirror the IRP_MJ_* constants. We support read,
        // write, and cleanup/close.
        const IRP_MJ_READ: u32 = 0x04;
        const IRP_MJ_WRITE: u32 = 0x08;
        const IRP_MJ_DEVICE_CONTROL: u32 = 0x0E;
        const IRP_MJ_CLOSE: u32 = 0x02;

        let mut response = Vec::new();
        let result = match major_func {
            IRP_MJ_READ => {
                let len = u32::from_le_bytes([
                    *data.get(0).unwrap_or(&0),
                    *data.get(1).unwrap_or(&0),
                    *data.get(2).unwrap_or(&0),
                    *data.get(3).unwrap_or(&0),
                ]) as usize;
                if let Some(port) = self.ports.get_mut(&device_id) {
                    response = port.read(len);
                    0
                } else {
                    1
                }
            }
            IRP_MJ_WRITE => {
                if let Some(port) = self.ports.get_mut(&device_id) {
                    port.write(data) as u32
                } else {
                    0
                }
            }
            IRP_MJ_DEVICE_CONTROL => {
                // IOCTLs are acknowledged without action in this version.
                0
            }
            IRP_MJ_CLOSE => {
                if let Some(port) = self.ports.remove(&device_id) {
                    let _ = port;
                }
                0
            }
            _ => 1,
        };

        let mut v = Vec::with_capacity(16 + response.len());
        v.extend_from_slice(&IO_RESPONSE.to_le_bytes());
        v.extend_from_slice(&device_id.to_le_bytes());
        v.extend_from_slice(&result.to_le_bytes());
        v.extend_from_slice(&response.len().to_le_bytes());
        v.extend_from_slice(&response);
        vec![v]
    }
}

impl Default for SerialChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct EchoPort {
        buf: Vec<u8>,
    }
    impl SerialPort for EchoPort {
        fn open(&mut self) -> bool { true }
        fn read(&mut self, len: usize) -> Vec<u8> {
            let n = len.min(self.buf.len());
            self.buf.drain(..n).collect()
        }
        fn write(&mut self, data: &[u8]) -> u32 {
            self.buf.extend_from_slice(data);
            data.len() as u32
        }
        fn close(&mut self) {}
    }

    #[test]
    fn initial_announce_has_version() {
        let pdu = SerialChannel::initial_announce();
        assert_eq!(u16::from_le_bytes([pdu[0], pdu[1]]), CLIENT_ANNOUNCE_REQUEST);
        assert_eq!(u32::from_le_bytes([pdu[2], pdu[3], pdu[4], pdu[5]]), RDPESP_VERSION);
    }

    #[test]
    fn server_announce_yields_reply() {
        let mut ch = SerialChannel::new();
        ch.set_port(Box::new(NullSerialPort));
        let server = [
            SERVER_ANNOUNCE_REQUEST as u8,
            (SERVER_ANNOUNCE_REQUEST >> 8) as u8,
            0x01, 0x00, 0x00, 0x00,
        ];
        let out = ch.process(&server);
        assert_eq!(out.len(), 1);
        assert_eq!(u16::from_le_bytes([out[0][0], out[0][1]]), CLIENT_ANNOUNCE_REPLY);
    }

    #[test]
    fn device_announce_opens_port_and_completes() {
        let mut ch = SerialChannel::new();
        ch.set_port(Box::new(EchoPort::default()));
        let mut announce = Vec::new();
        announce.extend_from_slice(&DEVICE_ANNOUNCE.to_le_bytes());
        announce.extend_from_slice(&1u32.to_le_bytes()); // device id
        announce.extend_from_slice(&0u32.to_le_bytes()); // type
        announce.extend_from_slice(b"COM1\0");
        let out = ch.process(&announce);
        assert_eq!(out.len(), 1);
        assert_eq!(u16::from_le_bytes([out[0][0], out[0][1]]), DEVICE_COMPLETION);
    }

    #[test]
    fn write_then_read_echoes() {
        let mut ch = SerialChannel::new();
        ch.set_port(Box::new(EchoPort::default()));
        // First bring the channel up and announce the device.
        let server = [
            SERVER_ANNOUNCE_REQUEST as u8,
            (SERVER_ANNOUNCE_REQUEST >> 8) as u8,
            0x01, 0x00, 0x00, 0x00,
        ];
        ch.process(&server);
        let mut announce = Vec::new();
        announce.extend_from_slice(&DEVICE_ANNOUNCE.to_le_bytes());
        announce.extend_from_slice(&1u32.to_le_bytes());
        announce.extend_from_slice(&0u32.to_le_bytes());
        announce.extend_from_slice(b"COM1\0");
        ch.process(&announce);

        // Write.
        let mut write = Vec::new();
        write.extend_from_slice(&IO_REQUEST.to_le_bytes());
        write.extend_from_slice(&1u32.to_le_bytes()); // device id
        write.extend_from_slice(&0x08u32.to_le_bytes()); // IRP_MJ_WRITE
        write.extend_from_slice(&0u32.to_le_bytes()); // minor
        write.extend_from_slice(&[1, 2, 3, 4]);
        let out = ch.process(&write);
        assert_eq!(out.len(), 1);
        assert_eq!(u16::from_le_bytes([out[0][0], out[0][1]]), IO_RESPONSE);

        // Read.
        let mut read = Vec::new();
        read.extend_from_slice(&IO_REQUEST.to_le_bytes());
        read.extend_from_slice(&1u32.to_le_bytes());
        read.extend_from_slice(&0x04u32.to_le_bytes()); // IRP_MJ_READ
        read.extend_from_slice(&0u32.to_le_bytes());
        read.extend_from_slice(&4u32.to_le_bytes()); // length
        let out = ch.process(&read);
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0][out[0].len() - 4..],
            &[1, 2, 3, 4]
        );
    }
}
