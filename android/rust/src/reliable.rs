//! The reliable channel's endpoint state (as the desktop client's
//! `hyper_link.rs`, which this follows).
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
const BULK_QUEUE_LIMIT: Duration = Duration::from_millis(10);

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
        self.state.lock().unwrap().tx[m.lane() as usize].send(&bytes);
        self.wake.notify_one();
    }

    /// Pace the bulk lane (bits per second; `None` = unpaced). The urgent
    /// lane is never paced: it only carries a few small messages.
    pub fn set_bulk_rate(&self, bps: Option<u64>) {
        self.state.lock().unwrap().tx[Lane::Bulk as usize].set_rate(bps);
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


    /// Bytes waiting in the bulk lane.
    pub fn bulk_backlog(&self) -> usize {
        self.state.lock().unwrap().tx[Lane::Bulk as usize].backlog()
    }

    /// Wake the pump (e.g. at shutdown so it notices `stop`).
    pub fn nudge(&self) {
        self.wake.notify_all();
    }
}

