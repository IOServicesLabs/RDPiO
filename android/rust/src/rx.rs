//! The Hyper stream's datagrams, received in batches: one `recvmmsg` takes
//! every datagram waiting (up to [BATCH]) where `recv` takes one. At 144 fps
//! and tens of Mbit/s that's thousands of system calls a second fewer, and
//! the receive thread wakes once per burst instead of once per datagram.
//!
//! With UDP GRO on, the kernel also hands over a run of same-size datagrams
//! from one sender (a frame's shards are all one size) as one buffer, cut
//! here at the size it reports: the network stack runs once per run rather
//! than per datagram. (`debug.rdpio.udpgro=0`: off, switched live.)

use std::net::UdpSocket;
use std::os::fd::AsRawFd;

/// Buffers per call without GRO (one datagram each).
const BATCH: usize = 32;
const SIZE: usize = 2048;
/// With GRO: fewer, each big enough for a coalesced run (64 KB at most).
const GRO_BATCH: usize = 8;
const GRO_SIZE: usize = 65_536;

pub struct Rx {
    /// One allocation, cut into BATCH x SIZE or GRO_BATCH x GRO_SIZE.
    buf: Vec<u8>,
    /// Datagrams of the last call not yet handed out: (start, length).
    segments: Vec<(usize, usize)>,
    next: usize,
    gro: Option<bool>,
    /// Receive calls and datagrams since [take_counts] (for the stats line).
    calls: u64,
    packets: u64,
}

impl Rx {
    pub fn new() -> Self {
        Self { buf: vec![0; GRO_BATCH * GRO_SIZE], segments: Vec::with_capacity(256), next: 0, gro: None, calls: 0, packets: 0 }
    }

    /// The next datagram (to decrypt in place), waiting up to the socket's read timeout.
    pub fn recv(&mut self, socket: &UdpSocket) -> Option<&mut [u8]> {
        if self.next >= self.segments.len() {
            self.next = 0;
            self.segments.clear();
            self.calls += 1;
            self.follow_gro(socket);
            self.recv_many(socket);
            self.packets += self.segments.len() as u64;
            if self.segments.is_empty() {
                return None;
            }
        }
        let (start, len) = self.segments[self.next];
        self.next += 1;
        Some(&mut self.buf[start..start + len])
    }

    fn follow_gro(&mut self, socket: &UdpSocket) {
        let want = crate::tune::ab("udpgro", true);
        if self.gro == Some(want) {
            return;
        }
        let on: libc::c_int = want as _;
        let ok = unsafe {
            libc::setsockopt(socket.as_raw_fd(), libc::SOL_UDP, libc::UDP_GRO, &on as *const _ as *const libc::c_void, std::mem::size_of_val(&on) as libc::socklen_t)
        } == 0;
        if want && !ok {
            log::info!("UDP GRO not available: {}", std::io::Error::last_os_error());
        }
        // (Refused: plain datagrams, as with it off.)
        self.gro = Some(want && ok);
    }

    fn recv_many(&mut self, socket: &UdpSocket) {
        let gro = self.gro == Some(true);
        let (batch, size) = if gro { (GRO_BATCH, GRO_SIZE) } else { (BATCH, SIZE) };
        let mut iov: [libc::iovec; BATCH] = unsafe { std::mem::zeroed() };
        let mut msgs: [libc::mmsghdr; BATCH] = unsafe { std::mem::zeroed() };
        // Room for one UDP_GRO control message (an int) per datagram.
        let mut control = [[0u64; 4]; BATCH];
        for i in 0..batch {
            iov[i] = libc::iovec { iov_base: self.buf[i * size..].as_mut_ptr().cast(), iov_len: size };
            msgs[i].msg_hdr.msg_iov = &mut iov[i];
            msgs[i].msg_hdr.msg_iovlen = 1;
            if gro {
                msgs[i].msg_hdr.msg_control = control[i].as_mut_ptr().cast();
                msgs[i].msg_hdr.msg_controllen = std::mem::size_of_val(&control[i]) as _;
            }
        }
        // (MSG_WAITFORONE: block, under SO_RCVTIMEO, for the first; then take
        // only what's already there.)
        let n = unsafe { libc::recvmmsg(socket.as_raw_fd(), msgs.as_mut_ptr(), batch as _, libc::MSG_WAITFORONE, std::ptr::null_mut()) };
        if n <= 0 {
            return;
        }
        for i in 0..n as usize {
            let len = msgs[i].msg_len as usize;
            let segment = if gro { gro_size(&msgs[i].msg_hdr).unwrap_or(len) } else { len };
            let mut start = i * size;
            let end = start + len;
            while start < end {
                let l = segment.min(end - start);
                self.segments.push((start, l));
                start += l;
            }
        }
    }

    /// Datagrams of the last call not yet handed out.
    pub fn pending(&self) -> usize {
        self.segments.len() - self.next
    }

    /// (calls, datagrams) since the last time.
    pub fn take_counts(&mut self) -> (u64, u64) {
        (std::mem::take(&mut self.calls), std::mem::take(&mut self.packets))
    }
}

/// The size a coalesced buffer's datagrams were (the last may be shorter),
/// when the kernel coalesced it.
fn gro_size(hdr: &libc::msghdr) -> Option<usize> {
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(hdr);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_UDP && (*c).cmsg_type == libc::UDP_GRO {
                let size = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const libc::c_int);
                return (size > 0).then_some(size as usize);
            }
            c = libc::CMSG_NXTHDR(hdr, c);
        }
    }
    None
}
