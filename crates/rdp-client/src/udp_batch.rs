//! Fewer system calls for the stream's UDP traffic (Windows 10 2004+):
//!
//! - sending: a run of equal-size datagrams goes to the kernel in one call
//!   (UDP segmentation offload, `UDP_SEND_MSG_SIZE`), which the network
//!   adapter (or the kernel, if it can't) cuts back into datagrams;
//! - receiving: datagrams from the same sender that queued up meanwhile come
//!   back from one call (receive coalescing, `UDP_RECV_MAX_COALESCED_SIZE`),
//!   with the size to cut them at.
//!
//! On the wire nothing changes. Where either isn't supported (older Windows,
//! some adapters and tunnels), the first failure turns it off and every
//! datagram goes on its own, as before.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::GUID;
use windows::Win32::Networking::WinSock::*;

/// Most datagrams per send (USO takes up to 64 KB at a time).
pub const MAX_SEGMENTS: usize = 40;

/// Segmentation offload works here (until a send says otherwise).
static SEND_OFFLOAD: AtomicBool = AtomicBool::new(true);

fn socket_of(s: &UdpSocket) -> SOCKET {
    SOCKET(s.as_raw_socket() as usize)
}

/// A socket address as Windows takes it (`SOCKADDR_IN` / `SOCKADDR_IN6`).
#[repr(C, align(8))]
struct RawAddr([u8; 28]);

fn raw_addr(a: SocketAddr) -> (RawAddr, i32) {
    let mut b = [0u8; 28];
    b[2..4].copy_from_slice(&a.port().to_be_bytes());
    match a {
        SocketAddr::V4(v4) => {
            b[0..2].copy_from_slice(&(AF_INET.0).to_le_bytes());
            b[4..8].copy_from_slice(&v4.ip().octets());
            (RawAddr(b), 16)
        }
        SocketAddr::V6(v6) => {
            b[0..2].copy_from_slice(&(AF_INET6.0).to_le_bytes());
            b[4..8].copy_from_slice(&v6.flowinfo().to_ne_bytes());
            b[8..24].copy_from_slice(&v6.ip().octets());
            b[24..28].copy_from_slice(&v6.scope_id().to_ne_bytes());
            (RawAddr(b), 28)
        }
    }
}

/// `WSACMSGHDR` (len: SIZE_T, level, type) then its data, 8-aligned.
#[repr(C, align(8))]
struct Control([u8; 32]);

const CMSG_HEADER: usize = core::mem::size_of::<usize>() + 8;

/// Send `packets` (`seg` bytes each, the last may be shorter) to `peer` in one
/// call. `Ok(false)`: segmentation offload isn't available; send them one by
/// one instead (nothing was sent).
pub fn send_segments(socket: &UdpSocket, peer: SocketAddr, packets: &[u8], seg: usize) -> io::Result<bool> {
    if packets.len() <= seg {
        socket.send_to(packets, peer)?;
        return Ok(true);
    }
    if !SEND_OFFLOAD.load(Ordering::Relaxed) {
        return Ok(false);
    }
    let (mut addr, addr_len) = raw_addr(peer);
    let mut control = Control([0; 32]);
    control.0[..core::mem::size_of::<usize>()].copy_from_slice(&(CMSG_HEADER + 4).to_ne_bytes());
    let at = core::mem::size_of::<usize>();
    control.0[at..at + 4].copy_from_slice(&(IPPROTO_UDP.0).to_ne_bytes());
    control.0[at + 4..at + 8].copy_from_slice(&(UDP_SEND_MSG_SIZE as i32).to_ne_bytes());
    control.0[CMSG_HEADER..CMSG_HEADER + 4].copy_from_slice(&(seg as u32).to_ne_bytes());
    let mut data = WSABUF { len: packets.len() as u32, buf: windows::core::PSTR(packets.as_ptr() as *mut u8) };
    let msg = WSAMSG {
        name: &mut addr as *mut RawAddr as *mut SOCKADDR,
        namelen: addr_len,
        lpBuffers: &mut data,
        dwBufferCount: 1,
        Control: WSABUF { len: (CMSG_HEADER + 8) as u32, buf: windows::core::PSTR(control.0.as_mut_ptr()) },
        dwFlags: 0,
    };
    let mut sent = 0u32;
    let r = unsafe { WSASendMsg(socket_of(socket), &msg, 0, Some(&mut sent), None, None) };
    if r == 0 {
        return Ok(true);
    }
    let err = unsafe { WSAGetLastError() };
    if matches!(err, WSAEINVAL | WSAEOPNOTSUPP | WSAENOPROTOOPT | WSAEMSGSIZE) {
        if SEND_OFFLOAD.swap(false, Ordering::Relaxed) {
            tracing::info!(code = err.0, "UDP segmentation offload unavailable; one datagram per send");
        }
        return Ok(false);
    }
    Err(io::Error::from_raw_os_error(err.0))
}

/// Equal-size datagrams to one peer, gathered and sent together.
pub struct SendBatch {
    buf: Vec<u8>,
    seg: usize,
    peer: Option<SocketAddr>,
}

impl Default for SendBatch {
    fn default() -> Self {
        Self { buf: Vec::with_capacity(MAX_SEGMENTS * 1500), seg: 0, peer: None }
    }
}

impl SendBatch {
    /// Add one datagram; what's gathered goes first if this one can't join
    /// it (another size or peer, or the batch is full). (The stream seals
    /// into the batch instead: [`push_with`](Self::push_with).)
    #[cfg(test)]
    pub fn push(&mut self, socket: &UdpSocket, peer: SocketAddr, packet: &[u8]) {
        self.push_with(socket, peer, packet.len(), |out| out.extend_from_slice(packet));
    }

    /// Add one datagram of `len` bytes that `write` appends to the batch's
    /// own buffer (sealed straight into it: no packet of its own, no copy);
    /// what's gathered goes first if this one can't join it (another size or
    /// peer, or the batch is full). `write` must append exactly `len` bytes;
    /// one that doesn't goes on its own, so the batch's datagrams stay
    /// equal-sized.
    pub fn push_with(&mut self, socket: &UdpSocket, peer: SocketAddr, len: usize, write: impl FnOnce(&mut Vec<u8>)) {
        if !self.buf.is_empty() && (len != self.seg || self.peer != Some(peer) || self.buf.len() / self.seg >= MAX_SEGMENTS) {
            self.flush(socket);
        }
        if self.buf.is_empty() {
            (self.seg, self.peer) = (len, Some(peer));
        }
        let at = self.buf.len();
        write(&mut self.buf);
        if self.buf.len() - at != len {
            let packet = self.buf.split_off(at);
            self.flush(socket);
            let _ = socket.send_to(&packet, peer);
        }
    }

    /// Send what's gathered.
    pub fn flush(&mut self, socket: &UdpSocket) {
        let Some(peer) = self.peer else { return };
        if self.buf.is_empty() {
            return;
        }
        match send_segments(socket, peer, &self.buf, self.seg) {
            Ok(true) => {}
            _ => {
                for p in self.buf.chunks(self.seg) {
                    let _ = socket.send_to(p, peer);
                }
            }
        }
        self.buf.clear();
    }
}

/// Receives datagrams, several at a time where the kernel coalesces them.
pub struct Receiver {
    buf: Vec<u8>,
    recv_msg: LPFN_WSARECVMSG,
}

impl Receiver {
    /// Turn coalescing on for `socket` where it can be.
    pub fn new(socket: &UdpSocket) -> Self {
        let s = socket_of(socket);
        let mut recv_msg: LPFN_WSARECVMSG = None;
        unsafe {
            let max = 60_000u32.to_ne_bytes();
            let on = setsockopt(s, IPPROTO_UDP.0, UDP_RECV_MAX_COALESCED_SIZE as i32, Some(&max)) == 0;
            if on {
                let guid: GUID = WSAID_WSARECVMSG;
                let mut out = 0u32;
                let r = WSAIoctl(
                    s,
                    SIO_GET_EXTENSION_FUNCTION_POINTER,
                    Some(&guid as *const GUID as *const core::ffi::c_void),
                    core::mem::size_of::<GUID>() as u32,
                    Some(&mut recv_msg as *mut LPFN_WSARECVMSG as *mut core::ffi::c_void),
                    core::mem::size_of::<LPFN_WSARECVMSG>() as u32,
                    &mut out,
                    None,
                    None,
                );
                if r != 0 {
                    recv_msg = None;
                }
                if recv_msg.is_none() {
                    let off = 0u32.to_ne_bytes();
                    setsockopt(s, IPPROTO_UDP.0, UDP_RECV_MAX_COALESCED_SIZE as i32, Some(&off));
                }
            }
            tracing::info!(coalescing = recv_msg.is_some(), "UDP receive");
        }
        Self { buf: vec![0; 65_536], recv_msg }
    }

    /// The next datagrams (as the socket's read timeout allows), one slice
    /// each, writable: a packet can be opened where it lies
    /// (`Session::open_in_place`), not copied out first.
    pub fn recv(&mut self, socket: &UdpSocket) -> io::Result<std::slice::ChunksMut<'_, u8>> {
        let Some(recv_msg) = self.recv_msg else {
            let n = socket.recv(&mut self.buf)?;
            return Ok(self.buf[..n].chunks_mut(n.max(1)));
        };
        let mut control = Control([0; 32]);
        let mut data = WSABUF { len: self.buf.len() as u32, buf: windows::core::PSTR(self.buf.as_mut_ptr()) };
        let mut msg = WSAMSG {
            name: core::ptr::null_mut(),
            namelen: 0,
            lpBuffers: &mut data,
            dwBufferCount: 1,
            Control: WSABUF { len: control.0.len() as u32, buf: windows::core::PSTR(control.0.as_mut_ptr()) },
            dwFlags: 0,
        };
        let mut n = 0u32;
        let r = unsafe { recv_msg(socket_of(socket), &mut msg, &mut n, core::ptr::null_mut(), None) };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }.0));
        }
        let n = n as usize;
        // The segment size, if these are several datagrams.
        let mut seg = n;
        let len = (msg.Control.len as usize).min(control.0.len());
        if len >= CMSG_HEADER + 4 {
            let at = core::mem::size_of::<usize>();
            let level = i32::from_ne_bytes(control.0[at..at + 4].try_into().unwrap());
            let kind = i32::from_ne_bytes(control.0[at + 4..at + 8].try_into().unwrap());
            if level == IPPROTO_UDP.0 && kind == UDP_COALESCED_INFO as i32 {
                let s = u32::from_ne_bytes(control.0[CMSG_HEADER..CMSG_HEADER + 4].try_into().unwrap()) as usize;
                if s > 0 {
                    seg = s;
                }
            }
        }
        Ok(self.buf[..n].chunks_mut(seg.max(1)))
    }

    /// Whether datagrams wait in `socket`'s buffer now (never blocks): a
    /// caller that rests between calls reads on at once while this says so.
    /// (A socket that can't tell reads as empty.)
    pub fn pending(&self, socket: &UdpSocket) -> bool {
        let mut bytes = 0u32;
        unsafe { ioctlsocket(socket_of(socket), FIONREAD, &mut bytes) == 0 && bytes > 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Sending a keyframe's worth (300 datagrams of 1200 bytes): one call
    /// per datagram against batches. `cargo test --release -p rdp-client --bin rdpio send_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn send_cost() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        crate::hyper_client::set_socket_buffers(&rx);
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = rx.local_addr().unwrap();
        let p = vec![7u8; 1200];
        for round in 0..3 {
            let t = std::time::Instant::now();
            for _ in 0..300 {
                tx.send_to(&p, peer).unwrap();
            }
            let single = t.elapsed();
            let mut batch = SendBatch::default();
            let t = std::time::Instant::now();
            for _ in 0..300 {
                batch.push(&tx, peer, &p);
            }
            batch.flush(&tx);
            let batched = t.elapsed();
            println!("round {round}: one by one {:.3} ms, batched {:.3} ms", single.as_secs_f64() * 1e3, batched.as_secs_f64() * 1e3);
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Twelve datagrams of 1200 bytes and a short last one, gathered and sent
    /// as one, arrive as they were, in order, through the coalescing receiver.
    #[test]
    fn batched_datagrams_arrive_as_sent() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.connect(rx.local_addr().unwrap()).unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let peer = rx.local_addr().unwrap();
        let mut batch = SendBatch::default();
        let mut sent = Vec::new();
        for i in 0..12u8 {
            let p = vec![i; 1200];
            batch.push(&tx, peer, &p);
            sent.push(p);
        }
        let last = vec![99u8; 300];
        batch.push(&tx, peer, &last);
        sent.push(last);
        batch.flush(&tx);
        let mut r = Receiver::new(&rx);
        let mut got = Vec::new();
        while got.len() < sent.len() {
            for d in r.recv(&rx).expect("all of them") {
                got.push(d.to_vec());
            }
        }
        println!("segmentation offload: {}, receive coalescing: {}", SEND_OFFLOAD.load(Ordering::Relaxed), r.recv_msg.is_some());
        assert_eq!(got, sent);
    }

    /// Datagrams sealed straight into the batch go out byte for byte as
    /// sealed ones pushed: a frame's equal-sized packets, a short last one,
    /// and a writer that appends the wrong length (sent on its own).
    #[test]
    fn sealed_into_the_batch_is_sealed_and_pushed() {
        use rdp_hyper::wire::{Direction, Kind, Session, OVERHEAD};
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let peer = rx.local_addr().unwrap();
        let key = [5u8; 32];
        let (mut a, mut b) = (Session::new(&key, Direction::HostToClient), Session::new(&key, Direction::HostToClient));
        let bodies: Vec<Vec<u8>> = (0..6u8).map(|i| vec![i; 1100]).chain([vec![42u8; 200]]).collect();
        let mut sent = Vec::new();
        let mut batch = SendBatch::default();
        for body in &bodies {
            sent.push(a.seal(Kind::Video, body));
            batch.push_with(&tx, peer, body.len() + OVERHEAD, |out| b.seal_into(Kind::Video, body, out));
        }
        // (Claims a length it doesn't write.)
        let odd = vec![1u8, 2, 3];
        sent.push(odd.clone());
        batch.push_with(&tx, peer, 1100 + OVERHEAD, |out| out.extend_from_slice(&odd));
        batch.flush(&tx);
        let mut r = Receiver::new(&rx);
        let mut got = Vec::new();
        while got.len() < sent.len() {
            for d in r.recv(&rx).expect("all of them") {
                got.push(d.to_vec());
            }
        }
        // (The odd one goes on its own, right after what was gathered.)
        let odd_at = got.iter().position(|d| d == &odd).expect("the odd one");
        got.remove(odd_at);
        sent.pop();
        assert_eq!(got, sent);
    }

    /// Received datagrams open where they lie in the receive buffer, to the
    /// bodies sealed, batched or one by one.
    #[test]
    fn received_datagrams_open_in_place() {
        use rdp_hyper::wire::{Direction, Kind, Session};
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let peer = rx.local_addr().unwrap();
        let key = [9u8; 32];
        let (mut seal, mut open) = (Session::new(&key, Direction::HostToClient), Session::new(&key, Direction::ClientToHost));
        let bodies: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 1100]).chain([vec![77u8; 90]]).collect();
        let mut batch = SendBatch::default();
        for body in &bodies {
            batch.push(&tx, peer, &seal.seal(Kind::Video, body));
        }
        batch.flush(&tx);
        tx.send_to(&seal.seal(Kind::Control, b"alone"), peer).unwrap();
        let mut r = Receiver::new(&rx);
        let mut got: Vec<(Kind, Vec<u8>)> = Vec::new();
        while got.len() < bodies.len() + 1 {
            for d in r.recv(&rx).expect("all of them") {
                let (kind, body) = open.open_in_place(d).expect("authentic");
                got.push((kind, body.to_vec()));
            }
        }
        let want: Vec<(Kind, Vec<u8>)> = bodies.into_iter().map(|b| (Kind::Video, b)).chain([(Kind::Control, b"alone".to_vec())]).collect();
        assert_eq!(got, want);
    }

    /// A socket says it holds datagrams from when they arrive until the last
    /// is read, and not before or after.
    #[test]
    fn a_socket_says_when_datagrams_wait() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.connect(rx.local_addr().unwrap()).unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut r = Receiver::new(&rx);
        assert!(!r.pending(&rx), "nothing was sent yet");
        for i in 0..3u8 {
            tx.send(&[i; 1200]).unwrap();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !r.pending(&rx) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(r.pending(&rx), "three datagrams wait");
        let mut got = 0;
        while got < 3 {
            got += r.recv(&rx).expect("all of them").count();
        }
        assert!(!r.pending(&rx), "all three were read");
    }
}
