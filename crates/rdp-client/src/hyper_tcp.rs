//! Copies on the LAN over a TCP stream of their own (see
//! [`rdp_hyper::bulk_tcp`]): the host listens on the Hyper port's number and
//! offers it over the link once a client on its LAN is in; the client
//! connects. Copied files then go over it both ways, at what the kernel's TCP
//! gets out of the path; without it (an older peer, another network, a
//! firewall), over the link as before.

use std::io::Write;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rdp_hyper::bulk_tcp::{new_key, Dir, Opener, Sealer, HELLO, KEY_LEN};
use rdp_hyper::messages::Message;

use crate::hyper_clipboard::ClipboardSync;
use crate::hyper_link::ReliableLink;

/// The stream of the session now going, if it's up.
static CURRENT: Mutex<Option<Arc<BulkTcp>>> = Mutex::new(None);
/// Bumped as a session ends: a listener still waiting for its client stops.
static SESSION: AtomicU64 = AtomicU64::new(0);

/// How long the host waits for the client to connect.
const ACCEPT_WITHIN: Duration = Duration::from_secs(15);

pub struct BulkTcp {
    stream: TcpStream,
    writer: Mutex<(TcpStream, Sealer)>,
    dead: AtomicBool,
}

impl BulkTcp {
    /// Send `m`; blocks while the kernel's buffers are full (the stream's own
    /// back-pressure). An error ends the stream: copies go over the link
    /// again.
    pub fn send(&self, m: &Message) -> std::io::Result<()> {
        if self.dead.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "copy stream ended"));
        }
        crate::hyper_link::bulk_moved();
        let mut w = self.writer.lock().unwrap();
        let (stream, sealer) = &mut *w;
        let r = sealer.write(stream, &m.to_bytes()).and_then(|()| stream.flush());
        if let Err(e) = &r {
            tracing::warn!(error = %e, "the copy stream broke: copies go over the link");
            self.end();
        }
        r
    }

    fn end(&self) {
        self.dead.store(true, Ordering::Relaxed);
        let _ = self.stream.shutdown(Shutdown::Both);
        let mut cur = CURRENT.lock().unwrap();
        if cur.as_ref().is_some_and(|c| std::ptr::eq(&**c, self)) {
            *cur = None;
        }
    }
}

/// The copy stream, when one is up.
pub fn current() -> Option<Arc<BulkTcp>> {
    CURRENT.lock().unwrap().clone().filter(|c| !c.dead.load(Ordering::Relaxed))
}

/// The session ended: its stream (or a listener still waiting) goes.
pub fn end_session() {
    SESSION.fetch_add(1, Ordering::SeqCst);
    let cur = CURRENT.lock().unwrap().take();
    if let Some(c) = cur {
        c.dead.store(true, Ordering::Relaxed);
        let _ = c.stream.shutdown(Shutdown::Both);
    }
}

fn tune(s: &TcpStream) {
    let _ = s.set_nodelay(true);
}

/// Host: listen on `port` (TCP) for this session's client at `client`, and
/// offer it over `link`. The SYSTEM agent opens the port in the firewall
/// first.
pub fn host_offer(port: u16, client: IpAddr, link: &ReliableLink, clipboard: Option<Arc<ClipboardSync>>) {
    let session = SESSION.load(Ordering::SeqCst);
    open_firewall(port);
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port, error = %e, "couldn't listen for a copy stream: copies go over the link");
            return;
        }
    };
    let key = new_key();
    link.send(&Message::BulkTcp { port, key });
    let spawned = std::thread::Builder::new().name("hyper-copy-listen".into()).spawn(move || {
        let _ = listener.set_nonblocking(true);
        let until = Instant::now() + ACCEPT_WITHIN;
        while Instant::now() < until && SESSION.load(Ordering::SeqCst) == session {
            match listener.accept() {
                Ok((s, from)) if from.ip() == client => {
                    let _ = s.set_nonblocking(false);
                    tune(&s);
                    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut r = &s;
                    let mut opener = Opener::new(&key, Dir::ClientToHost);
                    match opener.read(&mut r) {
                        Ok(h) if h == HELLO => {}
                        _ => {
                            tracing::warn!(%from, "a copy stream didn't say hello under the session's key: closed");
                            continue;
                        }
                    }
                    match start_after_hello(s, key, opener, clipboard.clone()) {
                        Ok(()) => tracing::info!(%from, "copy stream up: copies go over TCP"),
                        Err(e) => tracing::warn!(error = %e, "couldn't start the copy stream"),
                    }
                    return;
                }
                Ok((_, from)) => tracing::warn!(%from, "a copy stream from another address: refused"),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => {
                    tracing::warn!(error = %e, "copy stream accept failed");
                    return;
                }
            }
        }
        tracing::info!("no copy stream from the client: copies go over the link");
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "couldn't wait for a copy stream");
    }
}

/// The host's side once the client's hello opened (`opener` is past it).
fn start_after_hello(stream: TcpStream, key: [u8; KEY_LEN], opener: Opener, clipboard: Option<Arc<ClipboardSync>>) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(None);
    let tcp = Arc::new(BulkTcp {
        writer: Mutex::new((stream.try_clone()?, Sealer::new(&key, Dir::HostToClient))),
        stream: stream.try_clone()?,
        dead: AtomicBool::new(false),
    });
    *CURRENT.lock().unwrap() = Some(tcp.clone());
    std::thread::Builder::new().name("hyper-copy-tcp".into()).spawn(move || read_loop(stream, opener, tcp, clipboard))?;
    Ok(())
}

fn read_loop(stream: TcpStream, mut opener: Opener, tcp: Arc<BulkTcp>, clipboard: Option<Arc<ClipboardSync>>) {
    let mut r = std::io::BufReader::with_capacity(1 << 20, stream);
    loop {
        match opener.read(&mut r) {
            Ok(b) => {
                crate::hyper_link::bulk_moved();
                if let (Some(m), Some(c)) = (Message::parse(&b), &clipboard) {
                    c.apply(m);
                }
            }
            Err(e) => {
                if !tcp.dead.load(Ordering::Relaxed) {
                    tracing::info!(error = %e, "the copy stream ended");
                }
                tcp.end();
                return;
            }
        }
    }
}

/// Client: the host offered a copy stream at `host:port`.
pub fn client_connect(host: SocketAddr, port: u16, key: [u8; KEY_LEN], clipboard: Option<Arc<ClipboardSync>>) {
    let session = SESSION.load(Ordering::SeqCst);
    let spawned = std::thread::Builder::new().name("hyper-copy-connect".into()).spawn(move || {
        let to = SocketAddr::new(host.ip(), port);
        let s = match TcpStream::connect_timeout(&to, Duration::from_secs(3)) {
            Ok(s) => s,
            Err(e) => {
                tracing::info!(%to, error = %e, "couldn't reach the host's copy stream: copies go over the link");
                return;
            }
        };
        if SESSION.load(Ordering::SeqCst) != session {
            return;
        }
        tune(&s);
        let mut w = &s;
        let mut sealer = Sealer::new(&key, Dir::ClientToHost);
        if let Err(e) = sealer.write(&mut w, HELLO) {
            tracing::info!(error = %e, "the copy stream broke at hello");
            return;
        }
        let tcp = match (s.try_clone(), s.try_clone()) {
            (Ok(a), Ok(b)) => Arc::new(BulkTcp { writer: Mutex::new((a, sealer)), stream: b, dead: AtomicBool::new(false) }),
            _ => return,
        };
        *CURRENT.lock().unwrap() = Some(tcp.clone());
        tracing::info!(%to, "copy stream up: copies go over TCP");
        read_loop(s, Opener::new(&key, Dir::HostToClient), tcp, clipboard);
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "couldn't connect a copy stream");
    }
}

/// The agent runs as SYSTEM: let the client reach `port` over TCP (once a
/// process; a rule already there is left as it is).
fn open_firewall(port: u16) {
    static DONE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    DONE.get_or_init(|| {
        let netsh = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into())).join("System32\\netsh.exe");
        let rule = "rdpio Hyper copies (TCP)";
        let shown = std::process::Command::new(&netsh)
            .args(["advfirewall", "firewall", "show", "rule", &format!("name={rule}")])
            .output()
            .is_ok_and(|o| o.status.success());
        if shown {
            return;
        }
        let added = std::process::Command::new(&netsh)
            .args(["advfirewall", "firewall", "add", "rule", &format!("name={rule}"), "dir=in", "action=allow", "protocol=TCP", &format!("localport={port}"), "remoteip=localsubnet"])
            .output();
        match added {
            Ok(o) if o.status.success() => tracing::info!(port, "TCP {port} allowed through the firewall for copies"),
            _ => tracing::info!(port, "couldn't open TCP {port} in the firewall (not SYSTEM?): a copy stream may not get through"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 100 MB of sealed file chunks over TCP on loopback: what the stream's
    /// sealing and framing cost (the network aside).
    #[test]
    fn loopback_copy_stream_speed() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = l.local_addr().unwrap();
        let key = new_key();
        let reader = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = std::io::BufReader::with_capacity(1 << 20, s);
            let mut o = Opener::new(&key, Dir::ClientToHost);
            let mut n = 0;
            while let Ok(b) = o.read(&mut r) {
                assert!(matches!(Message::parse(&b), Some(Message::FileChunk { .. })));
                n += 1;
            }
            n
        });
        let s = TcpStream::connect(at).unwrap();
        tune(&s);
        let mut w = &s;
        let mut sealer = Sealer::new(&key, Dir::ClientToHost);
        let t = Instant::now();
        for i in 0..100u64 {
            let m = Message::FileChunk { transfer: 1, index: 0, offset: i << 20, data: vec![i as u8; 1 << 20] };
            sealer.write(&mut w, &m.to_bytes()).unwrap();
        }
        s.shutdown(Shutdown::Write).unwrap();
        assert_eq!(reader.join().unwrap(), 100);
        let secs = t.elapsed().as_secs_f64();
        eprintln!("100 MB sealed over loopback TCP: {secs:.2} s = {:.0} Mbit/s", 100.0 * 8.388_608 / secs);
    }
}
