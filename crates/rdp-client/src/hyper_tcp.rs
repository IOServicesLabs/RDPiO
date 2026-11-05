//! Copies on the LAN over a TCP stream of their own (see
//! [`rdp_hyper::bulk_tcp`]): the host listens on the Hyper port's number and
//! offers it over the link once a client on its LAN is in; the client
//! connects. Copied files then go over it both ways, at what the kernel's TCP
//! gets out of the path; without it (an older peer, another network, a
//! firewall), over the link as before.
//!
//! The stream may be striped over several connections: each has a writer
//! thread of its own (reading the files, sealing and sending overlap), and
//! records carry their sequence number for the receiver to put them back in
//! order before they reach the clipboard.

use std::collections::BTreeMap;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rdp_hyper::bulk_tcp::{new_key, Dir, Opener, Sealer, HELLO, KEY_LEN, MAX_STREAMS};
use rdp_hyper::messages::Message;

use crate::hyper_clipboard::ClipboardSync;
use crate::hyper_link::ReliableLink;

/// The stream of the session now going, if it's up.
static CURRENT: Mutex<Option<Arc<BulkTcp>>> = Mutex::new(None);
/// Bumped as a session ends: a listener still waiting for its client stops.
static SESSION: AtomicU64 = AtomicU64::new(0);

/// How long the host waits for the client's connections.
const ACCEPT_WITHIN: Duration = Duration::from_secs(15);

/// Records queued per connection ahead of its writer (1 MB each).
const QUEUED_PER_LANE: usize = 4;

pub struct BulkTcp {
    lanes: Vec<(TcpStream, SyncSender<Vec<u8>>)>,
    next_seq: AtomicU64,
    dead: Arc<AtomicBool>,
}

impl BulkTcp {
    /// How many connections the stream has.
    pub fn width(&self) -> usize {
        self.lanes.len()
    }

    /// Send `m` over the first `width` connections, round robin; blocks while
    /// the chosen connection's queue is full (TCP's back-pressure). An error
    /// ends the stream: copies go over the link again.
    pub fn send(&self, m: &Message, width: usize) -> std::io::Result<()> {
        if self.dead.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "copy stream ended"));
        }
        crate::hyper_link::bulk_moved();
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut rec = Vec::with_capacity(8 + 32 + (1 << 20));
        rec.extend_from_slice(&seq.to_le_bytes());
        rec.extend_from_slice(&m.to_bytes());
        let (_, tx) = &self.lanes[seq as usize % width.clamp(1, self.lanes.len())];
        tx.send(rec).map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "copy stream ended"))
    }

    fn end(&self) {
        if !self.dead.swap(true, Ordering::Relaxed) {
            for (s, _) in &self.lanes {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
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
        c.end();
    }
}

fn tune(s: &TcpStream) {
    let _ = s.set_nodelay(true);
}

/// Records from all connections, back in order.
struct Reorder {
    next: u64,
    held: BTreeMap<u64, Vec<u8>>,
}

/// What arrives goes to the clipboard as the link's messages do.
type Sink = Box<dyn Fn(Message) + Send + Sync>;

fn to_clipboard(clipboard: Option<Arc<ClipboardSync>>) -> Sink {
    Box::new(move |m| {
        if let Some(c) = &clipboard {
            c.apply(m);
        }
    })
}

/// Bring the stream up over `conns` (each past its hello): a writer and a
/// reader per connection.
fn start_with(conns: Vec<TcpStream>, key: [u8; KEY_LEN], ours: Dir, theirs: Dir, sink: Sink) -> std::io::Result<Arc<BulkTcp>> {
    let sink: Arc<Sink> = Arc::new(sink);
    let dead = Arc::new(AtomicBool::new(false));
    let mut lanes = Vec::new();
    let mut writers = Vec::new();
    for (i, s) in conns.iter().enumerate() {
        let _ = s.set_read_timeout(None);
        let (tx, rx) = sync_channel::<Vec<u8>>(QUEUED_PER_LANE);
        lanes.push((s.try_clone()?, tx));
        writers.push((s.try_clone()?, rx, i as u8));
    }
    let tcp = Arc::new(BulkTcp { lanes, next_seq: AtomicU64::new(0), dead: dead.clone() });
    for (mut s, rx, lane) in writers {
        // (Record 0 of each connection was its hello.)
        let mut sealer = Sealer::lane(&key, ours, lane);
        if ours == Dir::ClientToHost {
            sealer.skip_one();
        }
        let tcp = tcp.clone();
        std::thread::Builder::new().name("hyper-copy-tx".into()).spawn(move || {
            for rec in rx {
                if let Err(e) = sealer.write(&mut s, &rec) {
                    if !tcp.dead.load(Ordering::Relaxed) {
                        tracing::warn!(error = %e, "the copy stream broke: copies go over the link");
                    }
                    tcp.end();
                    return;
                }
            }
        })?;
    }
    let order = Arc::new(Mutex::new(Reorder { next: 0, held: BTreeMap::new() }));
    for (i, s) in conns.into_iter().enumerate() {
        let mut opener = Opener::lane(&key, theirs, i as u8);
        if theirs == Dir::ClientToHost {
            opener.skip_one();
        }
        let (tcp, order, sink) = (tcp.clone(), order.clone(), sink.clone());
        std::thread::Builder::new().name("hyper-copy-rx".into()).spawn(move || {
            let mut r = std::io::BufReader::with_capacity(1 << 20, s);
            loop {
                let rec = match opener.read(&mut r) {
                    Ok(b) if b.len() >= 8 => b,
                    Ok(_) | Err(_) if tcp.dead.load(Ordering::Relaxed) => return,
                    other => {
                        tracing::info!(error = ?other.err(), "the copy stream ended");
                        tcp.end();
                        return;
                    }
                };
                crate::hyper_link::bulk_moved();
                let seq = u64::from_le_bytes(rec[..8].try_into().unwrap());
                let mut o = order.lock().unwrap();
                o.held.insert(seq, rec);
                while let Some(rec) = { let n = o.next; o.held.remove(&n) } {
                    o.next += 1;
                    if let Some(m) = Message::parse(&rec[8..]) {
                        sink(m);
                    }
                }
            }
        })?;
    }
    *CURRENT.lock().unwrap() = Some(tcp.clone());
    Ok(tcp)
}

/// Host: listen on `port` (TCP) for this session's client at `client`, and
/// offer it over `link` with `streams` connections. The SYSTEM agent opens
/// the port in the firewall first.
pub fn host_offer(port: u16, client: IpAddr, streams: u8, link: &ReliableLink, clipboard: Option<Arc<ClipboardSync>>) {
    let session = SESSION.load(Ordering::SeqCst);
    let streams = streams.clamp(1, MAX_STREAMS);
    open_firewall(port);
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port, error = %e, "couldn't listen for a copy stream: copies go over the link");
            return;
        }
    };
    let key = new_key();
    link.send(&Message::BulkTcp { port, key, streams });
    let spawned = std::thread::Builder::new().name("hyper-copy-listen".into()).spawn(move || {
        let _ = listener.set_nonblocking(true);
        let until = Instant::now() + ACCEPT_WITHIN;
        let mut conns: Vec<Option<TcpStream>> = (0..streams).map(|_| None).collect();
        while Instant::now() < until && SESSION.load(Ordering::SeqCst) == session {
            match listener.accept() {
                Ok((s, from)) if from.ip() == client => {
                    let _ = s.set_nonblocking(false);
                    tune(&s);
                    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                    // Which connection this is: its hello opens under its number.
                    let raw = read_record(&s);
                    let lane = raw.as_ref().and_then(|raw| {
                        (0..streams).find(|&l| {
                            conns[l as usize].is_none()
                                && Opener::lane(&key, Dir::ClientToHost, l).read(&mut &raw[..]).is_ok_and(|h| h == hello(l, streams))
                        })
                    });
                    let Some(lane) = lane else {
                        tracing::warn!(%from, "a copy connection didn't say hello under the session's key: closed");
                        continue;
                    };
                    conns[lane as usize] = Some(s);
                    if conns.iter().all(Option::is_some) {
                        let sink = to_clipboard(clipboard);
                        match start_with(conns.into_iter().flatten().collect(), key, Dir::HostToClient, Dir::ClientToHost, sink) {
                            Ok(_) => tracing::info!(%from, streams, "copy stream up: copies go over TCP"),
                            Err(e) => tracing::warn!(error = %e, "couldn't start the copy stream"),
                        }
                        return;
                    }
                }
                Ok((_, from)) => tracing::warn!(%from, "a copy connection from another address: refused"),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
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

/// One record's bytes as they came (length and all), unopened.
fn read_record(s: &TcpStream) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut r = s;
    let mut len = [0u8; 4];
    r.read_exact(&mut len).ok()?;
    let n = u32::from_le_bytes(len) as usize;
    if n > 1024 {
        return None; // (a hello is a few dozen bytes)
    }
    let mut raw = len.to_vec();
    raw.resize(4 + n, 0);
    r.read_exact(&mut raw[4..]).ok()?;
    Some(raw)
}

/// Connection `lane` of `streams` says this first.
fn hello(lane: u8, streams: u8) -> Vec<u8> {
    let mut h = HELLO.to_vec();
    h.extend_from_slice(&[lane, streams]);
    h
}

/// Client: the host offered a copy stream at `host:port` over `streams`
/// connections.
pub fn client_connect(host: SocketAddr, port: u16, key: [u8; KEY_LEN], streams: u8, clipboard: Option<Arc<ClipboardSync>>) {
    let session = SESSION.load(Ordering::SeqCst);
    let streams = streams.clamp(1, MAX_STREAMS);
    let spawned = std::thread::Builder::new().name("hyper-copy-connect".into()).spawn(move || {
        let to = SocketAddr::new(host.ip(), port);
        let mut conns = Vec::new();
        for lane in 0..streams {
            let s = match TcpStream::connect_timeout(&to, Duration::from_secs(3)) {
                Ok(s) => s,
                Err(e) => {
                    tracing::info!(%to, error = %e, "couldn't reach the host's copy stream: copies go over the link");
                    return;
                }
            };
            tune(&s);
            let mut w = &s;
            if let Err(e) = Sealer::lane(&key, Dir::ClientToHost, lane).write(&mut w, &hello(lane, streams)) {
                tracing::info!(error = %e, "the copy stream broke at hello");
                return;
            }
            conns.push(s);
        }
        if SESSION.load(Ordering::SeqCst) != session {
            return;
        }
        match start_with(conns, key, Dir::ClientToHost, Dir::HostToClient, to_clipboard(clipboard)) {
            Ok(_) => tracing::info!(%to, streams, "copy stream up: copies go over TCP"),
            Err(e) => tracing::warn!(error = %e, "couldn't start the copy stream"),
        }
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

    /// 100 MB of file chunks through a striped stream over loopback: they
    /// arrive whole and in order, and what the sealing and threads cost.
    #[test]
    fn loopback_copy_stream_speed() {
        for streams in [1u8, 2, 4] {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let at = l.local_addr().unwrap();
            let key = new_key();
            let (got_tx, got_rx) = std::sync::mpsc::channel::<Message>();
            let host = std::thread::spawn(move || {
                let mut conns = Vec::new();
                for lane in 0..streams {
                    let (s, _) = l.accept().unwrap();
                    let mut r = &s;
                    assert_eq!(Opener::lane(&key, Dir::ClientToHost, lane).read(&mut r).unwrap(), hello(lane, streams));
                    conns.push(s);
                }
                start_with(conns, key, Dir::HostToClient, Dir::ClientToHost, Box::new(move |m| drop(got_tx.send(m)))).unwrap()
            });
            let mut conns = Vec::new();
            for lane in 0..streams {
                let s = TcpStream::connect(at).unwrap();
                let mut w = &s;
                Sealer::lane(&key, Dir::ClientToHost, lane).write(&mut w, &hello(lane, streams)).unwrap();
                conns.push(s);
            }
            let client = start_with(conns, key, Dir::ClientToHost, Dir::HostToClient, Box::new(|_| {})).unwrap();
            let _host = host.join().unwrap();
            let t = Instant::now();
            for i in 0..100u64 {
                client.send(&Message::FileChunk { transfer: 1, index: 0, offset: i << 20, data: vec![i as u8; 1 << 20] }, streams as usize).unwrap();
            }
            for i in 0..100u64 {
                match got_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    Message::FileChunk { offset, data, .. } => {
                        assert_eq!(offset, i << 20, "in order");
                        assert!(data.iter().all(|&b| b == i as u8));
                    }
                    m => panic!("{m:?}"),
                }
            }
            let secs = t.elapsed().as_secs_f64();
            eprintln!("100 MB over {streams} loopback connection(s): {secs:.2} s = {:.0} Mbit/s", 100.0 * 8.388_608 / secs);
            client.end();
        }
    }
}
