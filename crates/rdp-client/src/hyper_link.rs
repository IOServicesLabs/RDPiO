//! The reliable channel's endpoint state, shared by the Hyper host and client.
//!
//! Two independent lanes ride `Kind::Reliable` (a lane byte starts every
//! body): urgent for small, latency-sensitive messages (the pointer), bulk for
//! clipboard contents and files — paced, so a big copy can't crowd out the
//! stream. A pump thread sends, resends and flushes batched acks; it blocks
//! when there is nothing to do, so it costs nothing at rest.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rdp_hyper::messages::{Lane, Message};
use rdp_hyper::reliable::{is_ack, Receiver, Sender};

/// How often the pump looks again while chunks are in flight (resend timers,
/// pacing tokens).
const TICK: Duration = Duration::from_millis(2);
/// Longest idle wait (only `stop` needs noticing then).
const IDLE: Duration = Duration::from_millis(250);
/// The bulk lane backs off once the round trip grows this much: it shares
/// the uplink with game input, which mustn't sit behind a copy's queue.
pub const BULK_QUEUE_LIMIT: Duration = Duration::from_millis(10);

struct Lanes {
    tx: [Sender; 2],
    rx: [Receiver; 2],
}

pub struct ReliableLink {
    state: Mutex<Lanes>,
    wake: Condvar,
}

fn framed(lane: Lane, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(1 + body.len());
    b.push(lane as u8);
    b.extend_from_slice(body);
    b
}

/// When the bulk lane last moved data, either way (ms since [`epoch`]).
static BULK_MOVED_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn bulk_moved() {
    BULK_MOVED_MS.store(epoch().elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
}

/// A copy (files, an image) is going either way over this process's link,
/// or did within the last second: what holds the stream's power settings
/// (Wi-Fi streaming mode, the 1 ms timer, thread priorities) while its
/// window is hidden — pasting is done with the stream's window under
/// Explorer's, and a copy then went with the radio dozing and coarse timers
/// (100 MB: 62 Mbit/s, 273 with the stream in view).
pub fn copying() -> bool {
    let at = BULK_MOVED_MS.load(Ordering::Relaxed);
    at > 0 && (epoch().elapsed().as_millis() as u64 + 1).saturating_sub(at) < 1000
}

impl ReliableLink {
    pub fn new() -> Arc<Self> {
        let mut bulk = Sender::new();
        bulk.set_queue_limit(BULK_QUEUE_LIMIT);
        Arc::new(Self {
            state: Mutex::new(Lanes { tx: [Sender::new(), bulk], rx: [Receiver::new(), Receiver::new()] }),
            wake: Condvar::new(),
        })
    }

    pub fn send(&self, m: &Message) {
        let bytes = m.to_bytes();
        if m.lane() == Lane::Bulk {
            bulk_moved();
        }
        self.state.lock().unwrap().tx[m.lane() as usize].send(&bytes);
        self.wake.notify_one();
    }

    /// Pace the bulk lane (bits per second; `None` = unpaced). The urgent
    /// lane is never paced: it only carries a few small messages.
    pub fn set_bulk_rate(&self, bps: Option<u64>) {
        self.state.lock().unwrap().tx[Lane::Bulk as usize].set_rate(bps);
    }

    /// How far over the path's least round trip the bulk lane lets its
    /// round trip grow before it backs off.
    pub fn set_bulk_queue_limit(&self, limit: Duration) {
        self.state.lock().unwrap().tx[Lane::Bulk as usize].set_queue_limit(limit);
    }

    /// An incoming reliable-channel body: returns an ack to send back now (if
    /// one is due — otherwise the pump sends it shortly) and the messages it
    /// completed.
    pub fn on_packet(&self, body: &[u8]) -> (Option<Vec<u8>>, Vec<Message>) {
        let Some((lane, body)) = body.split_first().and_then(|(l, b)| Some((Lane::from_u8(*l)?, b))) else {
            return (None, Vec::new());
        };
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        if is_ack(body) {
            st.tx[lane as usize].on_ack(body, now);
            drop(st);
            self.wake.notify_one(); // the window may have opened
            return (None, Vec::new());
        }
        if lane == Lane::Bulk {
            bulk_moved();
        }
        let rx = &mut st.rx[lane as usize];
        let raw = rx.on_data(body, now);
        let ack = rx.ack(now).map(|a| framed(lane, &a));
        let pending = rx.ack_deadline().is_some();
        drop(st);
        if pending {
            self.wake.notify_one(); // a batched ack for the pump to flush
        }
        (ack, raw.iter().filter_map(|m| Message::parse(m)).collect())
    }

    /// Acks due now (batched ones whose delay passed). The pump calls this;
    /// a caller without a pump (the self-test) can too.
    pub fn due_acks(&self) -> Vec<Vec<u8>> {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        Lane::ALL.iter().filter_map(|&l| st.rx[l as usize].ack(now).map(|a| framed(l, &a))).collect()
    }

    /// Send (and resend) queued chunks and flush acks via `send` until `stop`.
    pub fn spawn_pump(
        self: &Arc<Self>,
        send: impl Fn(&[u8]) + Send + 'static,
        stop: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        let link = self.clone();
        std::thread::Builder::new()
            .name("hyper-reliable".into())
            .spawn(move || {
                let mut out = Vec::new();
                let mut st = link.state.lock().unwrap();
                while !stop.load(Ordering::Relaxed) {
                    let now = Instant::now();
                    let mut wait = IDLE;
                    // Urgent first: its chunks go ahead of bulk ones.
                    for lane in Lane::ALL {
                        let lanes = &mut *st;
                        let (tx, rx) = (&mut lanes.tx[lane as usize], &mut lanes.rx[lane as usize]);
                        if let Some(a) = rx.ack(now) {
                            out.push(framed(lane, &a));
                        }
                        if let Some(at) = rx.ack_deadline() {
                            wait = wait.min(at.saturating_duration_since(now));
                        }
                        out.extend(tx.poll(now).iter().map(|c| framed(lane, c)));
                        if tx.backlog() > 0 {
                            wait = wait.min(TICK);
                            if lane == Lane::Bulk {
                                bulk_moved();
                            }
                        }
                    }
                    if out.is_empty() {
                        st = link.wake.wait_timeout(st, wait.max(Duration::from_micros(500))).unwrap().0;
                        continue;
                    }
                    drop(st);
                    for b in out.drain(..) {
                        send(&b);
                    }
                    st = link.state.lock().unwrap();
                }
            })
            .expect("spawn reliable pump")
    }

    /// Bytes queued but not yet acknowledged, both lanes.
    #[cfg(test)]
    pub fn backlog(&self) -> usize {
        let st = self.state.lock().unwrap();
        st.tx.iter().map(Sender::backlog).sum()
    }

    /// Bytes waiting in the bulk lane.
    pub fn bulk_backlog(&self) -> usize {
        self.state.lock().unwrap().tx[Lane::Bulk as usize].backlog()
    }

    /// How the bulk lane's sender reads the path, and what it has done.
    pub fn bulk_stats(&self) -> rdp_hyper::reliable::SenderStats {
        self.state.lock().unwrap().tx[Lane::Bulk as usize].stats()
    }

    /// Read a spiky (Wi-Fi) path's round trip as TCP does (see
    /// `Sender::set_jitter_tolerant`).
    pub fn set_bulk_jitter_tolerant(&self, on: bool) {
        self.state.lock().unwrap().tx[Lane::Bulk as usize].set_jitter_tolerant(on);
    }

    /// Wake the pump (e.g. at shutdown so it notices `stop`).
    pub fn nudge(&self) {
        self.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Lab: 100 MB of file chunks between two links over UDP on loopback,
    /// with the host's own pump, unpaced as on the LAN: what the channel
    /// itself can carry, with no radio in the way.
    /// `cargo test --release -p rdp-client loopback_copy -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn loopback_copy_speed() {
        use std::net::UdpSocket;
        for tolerant in [false, true] {
            let (a, b) = (ReliableLink::new(), ReliableLink::new());
            a.set_bulk_rate(None);
            a.set_bulk_queue_limit(Duration::from_millis(25));
            a.set_bulk_jitter_tolerant(tolerant);
            let (sa, sb) = (UdpSocket::bind("127.0.0.1:0").unwrap(), UdpSocket::bind("127.0.0.1:0").unwrap());
            crate::hyper_client::set_socket_buffers(&sa);
            crate::hyper_client::set_socket_buffers(&sb);
            sa.connect(sb.local_addr().unwrap()).unwrap();
            sb.connect(sa.local_addr().unwrap()).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let pa = {
                let s = sa.try_clone().unwrap();
                a.spawn_pump(move |p| drop(s.send(p)), stop.clone())
            };
            let pb = {
                let s = sb.try_clone().unwrap();
                b.spawn_pump(move |p| drop(s.send(p)), stop.clone())
            };
            // The host's receive thread: acks in.
            sa.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
            sb.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
            let ra = {
                let (s, a, stop) = (sa.try_clone().unwrap(), a.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut buf = [0u8; 2048];
                    while !stop.load(Ordering::Relaxed) {
                        if let Ok(n) = s.recv(&mut buf) {
                            a.on_packet(&buf[..n]);
                        }
                    }
                })
            };
            // The client's: data in, acks straight back.
            const MB: usize = 100;
            let got = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let rb = {
                let (s, b, stop, got) = (sb.try_clone().unwrap(), b.clone(), stop.clone(), got.clone());
                std::thread::spawn(move || {
                    let mut buf = [0u8; 2048];
                    while !stop.load(Ordering::Relaxed) {
                        if let Ok(n) = s.recv(&mut buf) {
                            let (ack, msgs) = b.on_packet(&buf[..n]);
                            if let Some(ack) = ack {
                                let _ = s.send(&ack);
                            }
                            got.fetch_add(msgs.len(), Ordering::Relaxed);
                        }
                    }
                })
            };
            let start = Instant::now();
            for i in 0..MB {
                while a.bulk_backlog() > 16 * 1024 * 1024 {
                    std::thread::sleep(Duration::from_millis(5));
                }
                a.send(&Message::FileChunk { transfer: 1, index: 0, offset: (i << 20) as u64, data: vec![i as u8; 1 << 20] });
            }
            while got.load(Ordering::Relaxed) < MB && start.elapsed() < Duration::from_secs(120) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let secs = start.elapsed().as_secs_f64();
            eprintln!(
                "loopback, tolerant={tolerant}: {} MB in {secs:.2} s = {:.0} Mbit/s {:?}",
                got.load(Ordering::Relaxed),
                (MB << 20) as f64 * 8.0 / secs / 1e6,
                a.bulk_stats()
            );
            stop.store(true, Ordering::Relaxed);
            a.nudge();
            b.nudge();
            for h in [pa, pb, ra, rb] {
                h.join().unwrap();
            }
        }
    }

    /// Two links joined by channels (lossless), each with its pump.
    #[test]
    fn lanes_deliver_independently_and_acks_flush() {
        let (a, b) = (ReliableLink::new(), ReliableLink::new());
        let stop = Arc::new(AtomicBool::new(false));
        let (to_b, from_a) = mpsc::channel::<Vec<u8>>();
        let (to_a, from_b) = mpsc::channel::<Vec<u8>>();
        let pa = a.spawn_pump(move |p| drop(to_b.send(p.to_vec())), stop.clone());
        let pb = b.spawn_pump(move |p| drop(to_a.send(p.to_vec())), stop.clone());
        // A big bulk message, then a cursor: the cursor must not wait for it.
        a.set_bulk_rate(Some(2_000_000));
        a.send(&Message::ClipboardImage(vec![7; 400_000]));
        a.send(&Message::CursorVisible(false));
        let start = Instant::now();
        let (mut cursor_at, mut image_at) = (None, None);
        while start.elapsed() < Duration::from_secs(5) && image_at.is_none() {
            while let Ok(p) = from_a.try_recv() {
                let (ack, msgs) = b.on_packet(&p);
                if let Some(ack) = ack {
                    a.on_packet(&ack);
                }
                for m in msgs {
                    match m {
                        Message::CursorVisible(_) => cursor_at = Some(start.elapsed()),
                        Message::ClipboardImage(_) => image_at = Some(start.elapsed()),
                        _ => {}
                    }
                }
            }
            while let Ok(p) = from_b.try_recv() {
                a.on_packet(&p);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let (cursor_at, image_at) = (cursor_at.unwrap(), image_at.unwrap());
        assert!(cursor_at < Duration::from_millis(100), "cursor took {cursor_at:?}");
        assert!(image_at > Duration::from_millis(1000), "bulk wasn't paced: {image_at:?}");
        // Everything acknowledged in the end (the pump flushed batched acks).
        let t = Instant::now();
        while a.backlog() > 0 && t.elapsed() < Duration::from_secs(2) {
            while let Ok(p) = from_b.try_recv() {
                a.on_packet(&p);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(a.backlog(), 0);
        stop.store(true, Ordering::Relaxed);
        a.nudge();
        b.nudge();
        pa.join().unwrap();
        pb.join().unwrap();
    }
}
