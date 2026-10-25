//! The RDP-UDP side-band's API for the shared session loop. W365 and AVD
//! connections arrive through a gateway WebSocket with no direct address to
//! dial, so the session never dials one here: [`UdpTunnel::connect`] always
//! declines and the graphics stay on the TCP path.

use std::io;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct UdpDial {
    pub server: String,
    pub hostname: String,
    pub accept_invalid_cert: bool,
    pub debug: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct NetSample {
    pub recv_total: u64,
    pub recv_lost: u64,
    pub srtt: Option<Duration>,
    pub jitter: Duration,
}

pub struct UdpTunnel {
    _never: std::convert::Infallible,
}

impl UdpTunnel {
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        _server: &str,
        _hostname: &str,
        _accept_invalid_cert: bool,
        _request_id: u32,
        _cookie: &[u8; 16],
        _lossy: bool,
        _debug: bool,
    ) -> io::Result<Self> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no RDP-UDP on Android"))
    }

    pub fn net_stats(&self) -> NetSample {
        match self._never {}
    }

    pub fn recv(&mut self) -> io::Result<Vec<u8>> {
        match self._never {}
    }

    pub fn raw_socket(&self) -> std::os::unix::io::RawFd {
        match self._never {}
    }

    pub fn awaiting_acks(&self) -> bool {
        match self._never {}
    }

    pub fn send(&mut self, _data: &[u8]) -> io::Result<()> {
        match self._never {}
    }
}
