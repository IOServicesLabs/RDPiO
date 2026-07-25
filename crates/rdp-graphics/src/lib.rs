//! Graphics decoding: the codecs the RDP graphics pipeline carries.
//!
//! The EGFX command stream itself is parsed in [`rdp_pdu::gfx`]; this crate
//! turns the bytes those commands reference into pixels — legacy bitmap updates
//! ([`bitmap`]), ZGFX bulk decompression ([`zgfx`]), the EGFX-over-DVC pipeline
//! ([`egfx`]), AVC420/444 framing ([`avc`]), and NV12→RGBA ([`yuv`]) — and tracks
//! where each server surface maps onto the desktop ([`surface`]) so decoded
//! updates land at the right screen coordinates. It stays platform-independent
//! and does no GPU work; the renderer (`rdp-gpu`) presents.
#![forbid(unsafe_code)]

pub mod channel;

pub mod redirect;

pub mod egfx;

pub mod avc;

pub mod bitmap;

pub mod rfx;

pub mod progressive;

pub mod clearcodec;

pub mod yuv;

pub mod zgfx;

pub mod surface;

pub mod pointer;

pub mod pool;
