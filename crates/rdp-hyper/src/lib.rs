//! Hyper Accelerator: rdpio's low-latency game-streaming protocol.
//!
//! RDP's own graphics path tops out at 60 fps (the session's virtual display)
//! and buffers more than a game wants. With `--hyper`, rdpio uses RDP only to
//! sign in and to exchange a session key over a private RDP channel
//! ([`control::Handshake`]); the host agent then moves the session onto the
//! physical console, captures the real display at its full refresh rate,
//! encodes on the GPU and streams over this protocol:
//!
//! - [`wire`]: AES-256-GCM sealed UDP datagrams of at most 1200 bytes (no IP
//!   fragmentation over Tailscale/ZeroTier), with replay protection.
//! - [`video`] + [`fec`]: frames cut into packets with Reed-Solomon parity, so
//!   lost packets are rebuilt without a resend round trip.
//! - [`input`]: client input that survives loss without stuck keys.
//! - [`rate`]: delay- and loss-driven bitrate and FEC control.
//!
//! Platform-neutral; the capture/encode/decode ends live in `rdp-gpu` and
//! `rdp-client`.

pub mod audio;
pub mod auth;
pub mod control;
pub mod fec;
pub mod input;
pub mod messages;
pub mod rate;
pub mod reliable;
pub mod update;
pub mod video;
pub mod wire;

/// Default UDP port the host agent listens on.
pub const DEFAULT_PORT: u16 = 47998;
