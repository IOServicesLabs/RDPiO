//! Windows IOCP-based TCP socket wrapper (compile-tested skeleton).
//!
//! This module creates an overlapped Winsock socket, associates it with an I/O
//! Completion Port, and exposes synchronous [`Read`]/[`Write`] methods backed
//! by overlapped [`WSARecv`](windows::Win32::Networking::WinSock::WSARecv) /
//! [`WSASend`](windows::Win32::Networking::WinSock::WSASend). It is not yet
//! wired into [`crate::transport`]; once benchmarked it can replace the
//! poll-based `std::net::TcpStream` path on Windows.

#![allow(dead_code)]

use std::io;

use windows::core::PSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Networking::WinSock::{
    closesocket, connect, AF_INET, INVALID_SOCKET, SOCKET, SOCKET_ERROR, SOCKADDR, SOCKADDR_IN,
    WSABUF, WSADATA, WSAGetLastError, WSASocketW, WSAStartup, WSA_FLAG_OVERLAPPED,
};
use windows::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
};

/// A TCP stream backed by an overlapped socket and an I/O Completion Port.
pub struct IocpStream {
    socket: SOCKET,
    iocp: HANDLE,
}

impl IocpStream {
    /// Create a new overlapped socket and associate it with a fresh IOCP.
    pub fn new() -> io::Result<Self> {
        init_winsock()?;

        // In windows 0.57 `WSASocketW` returns the `SOCKET` directly (rather than
        // a `Result`), signalling failure with `INVALID_SOCKET`.
        let socket = unsafe {
            WSASocketW(
                AF_INET.0 as i32,
                1, // SOCK_STREAM.0
                0,
                None,
                0,
                WSA_FLAG_OVERLAPPED,
            )
        }
        .unwrap_or(INVALID_SOCKET);
        if socket == INVALID_SOCKET {
            return Err(last_wsa_error());
        }

        // windows 0.62 takes the existing completion port as `Option<HANDLE>`.
        let iocp = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1)? };

        unsafe {
            CreateIoCompletionPort(HANDLE(socket.0 as *mut core::ffi::c_void), Some(iocp), 0, 0)?;
        }

        Ok(Self { socket, iocp })
    }

    /// Connect to `addr`.
    pub fn connect(&self, addr: &std::net::SocketAddr) -> io::Result<()> {
        let sa = socket_addr_to_winsock(addr);
        let len = std::mem::size_of::<SOCKADDR_IN>() as i32;
        let rc = unsafe { connect(self.socket, &sa as *const _ as *const SOCKADDR, len) };
        if rc == SOCKET_ERROR {
            return Err(last_wsa_error());
        }
        Ok(())
    }

    fn wait_completion(&self,
        bytes: &mut u32,
    ) -> io::Result<()> {
        let mut completion_key = 0usize;
        let mut overlapped_ptr = std::ptr::null_mut();
        unsafe {
            GetQueuedCompletionStatus(
                self.iocp,
                bytes,
                &mut completion_key,
                &mut overlapped_ptr,
                u32::MAX,
            )
            .map_err(|_| io::Error::last_os_error())
        }
    }
}

impl io::Read for IocpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut overlapped = OVERLAPPED::default();
        let wsabuf = WSABUF {
            len: buf.len() as u32,
            buf: PSTR(buf.as_mut_ptr()),
        };
        let mut flags = 0u32;
        let mut received = 0u32;

        let rc = unsafe {
            windows::Win32::Networking::WinSock::WSARecv(
                self.socket,
                &[wsabuf],
                Some(&mut received),
                &mut flags,
                Some(&mut overlapped),
                None,
            )
        };

        if rc == SOCKET_ERROR {
            let err = unsafe { WSAGetLastError() };
            if err.0 != windows::Win32::Networking::WinSock::WSA_IO_PENDING.0 {
                return Err(io::Error::from_raw_os_error(err.0 as i32));
            }
        }

        self.wait_completion(&mut received)?;
        Ok(received as usize)
    }
}

impl io::Write for IocpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut overlapped = OVERLAPPED::default();
        let wsabuf = WSABUF {
            len: buf.len() as u32,
            buf: PSTR(buf.as_ptr() as *mut _),
        };
        let mut sent = 0u32;

        let rc = unsafe {
            windows::Win32::Networking::WinSock::WSASend(
                self.socket,
                &[wsabuf],
                Some(&mut sent),
                0,
                Some(&mut overlapped),
                None,
            )
        };

        if rc == SOCKET_ERROR {
            let err = unsafe { WSAGetLastError() };
            if err.0 != windows::Win32::Networking::WinSock::WSA_IO_PENDING.0 {
                return Err(io::Error::from_raw_os_error(err.0 as i32));
            }
        }

        self.wait_completion(&mut sent)?;
        Ok(sent as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for IocpStream {
    fn drop(&mut self) {
        unsafe {
            closesocket(self.socket);
            let _ = CloseHandle(self.iocp);
        }
    }
}

fn init_winsock() -> io::Result<()> {
    static INIT: std::sync::Once = std::sync::Once::new();
    static mut OK: bool = false;
    unsafe {
        INIT.call_once(|| {
            let mut data = WSADATA::default();
            let rc = WSAStartup(0x0202, &mut data);
            OK = rc == 0;
        });
        if OK {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::Other, "WSAStartup failed"))
        }
    }
}

fn last_wsa_error() -> io::Error {
    let err = unsafe { WSAGetLastError() };
    io::Error::from_raw_os_error(err.0 as i32)
}

fn socket_addr_to_winsock(addr: &std::net::SocketAddr) -> SOCKADDR_IN {
    let ip_bytes = match addr.ip() {
        std::net::IpAddr::V4(v4) => v4.octets(),
        std::net::IpAddr::V6(_) => panic!("IPv6 not supported by this skeleton"),
    };
    SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: addr.port().to_be(),
        sin_addr: windows::Win32::Networking::WinSock::IN_ADDR {
            S_un: windows::Win32::Networking::WinSock::IN_ADDR_0 {
                S_addr: u32::from_le_bytes(ip_bytes),
            },
        },
        sin_zero: [0; 8],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn iocp_stream_connects_and_exchanges_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"hello");
            s.write_all(b"world").unwrap();
        });

        let mut stream = IocpStream::new().unwrap();
        stream.connect(&addr).unwrap();
        stream.write_all(b"hello").unwrap();
        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"world");

        server.join().unwrap();
    }

    #[test]
    fn iocp_stream_throughput_baseline() {
        const BYTES: usize = 4 * 1024 * 1024;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut incoming = vec![0u8; BYTES];
            s.read_exact(&mut incoming).unwrap();
            let outgoing: Vec<u8> = (0..BYTES).map(|i| (i % 251) as u8).collect();
            s.write_all(&outgoing).unwrap();
        });

        let mut stream = IocpStream::new().unwrap();
        stream.connect(&addr).unwrap();

        let start = Instant::now();
        let write_payload: Vec<u8> = (0..BYTES).map(|i| (i % 251) as u8).collect();
        stream.write_all(&write_payload).unwrap();
        let mut read_payload = vec![0u8; BYTES];
        stream.read_exact(&mut read_payload).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(read_payload, (0..BYTES).map(|i| (i % 251) as u8).collect::<Vec<_>>());
        // Loopback should move 8 MiB in well under a second; the real value on a
        // modern desktop is usually tens of GB/s, but be lenient on CI/sandbox.
        let seconds = elapsed.as_secs_f64();
        let mb = ((BYTES * 2) as f64) / (1024.0 * 1024.0);
        let mbps = mb / seconds.max(0.001);
        assert!(
            mbps > 50.0,
            "IOCP loopback throughput too low: {:.1} MB/s over {:.3}s",
            mbps,
            seconds
        );
        tracing::info!(
            "IOCP loopback throughput: {:.1} MB/s ({:.3}s for {} MiB)",
            mbps,
            seconds,
            mb
        );

        server.join().unwrap();
    }
}
