//! Native Teams "Optimized" WebRTC redirector.
//!
//! Windows reaches Teams "Optimized" by hosting Microsoft's `MsRdcWebRTCAddIn.dll`
//! (see `rdp-client::webrtc_addin`). That DLL is a Windows PE bound to Media
//! Foundation / D3D11 / COM and cannot run anywhere else. But the protocol it
//! speaks on the `com.microsoft.rdc.dvc.webrtc.1` dynamic virtual channel is not
//! Microsoft-proprietary media — it is **plain JSON-RPC that mirrors the W3C
//! WebRTC / MediaDevices JavaScript API**. Teams runs in the Cloud PC and its JS
//! WebRTC calls are proxied over the DVC to the *client*, which executes them
//! against a local WebRTC engine so the media runs peer-to-peer from the client
//! (straight to Teams' relays, never through the Cloud PC — the "optimized" win).
//!
//! This crate re-implements the *client* half of that contract in portable Rust,
//! so the same optimization can run where the DLL can't (Linux, via `webrtc-rs`).
//! It is deliberately **decoupled from the Windows add-in path**: `rdp-client`
//! keeps hosting the real DLL on Windows unchanged; this is the parallel native
//! engine, developed and validated against real captured calls before it is wired
//! into a live session.
//!
//! Layers (bottom-up):
//! - [`framing`] — one DVC message ⇄ one NUL-terminated JSON document.
//! - [`capture`] — read `RDPIO_WEBRTC_CAPTURE` capture files (offline fixtures).
//! - [`rpc`] — typed view over the JSON-RPC message forms (call / result / event).
//! - [`objects`] — the remoted JS object model (`RTCPeerConnection`, …).
//! - [`session`] — the redirector state machine: tracks the object graph and (in
//!   a later stage) drives a real WebRTC engine, emitting the response/event
//!   messages back to the server.
//!
//! The protocol schema this encodes is documented in the `rdpio-webrtc1-protocol`
//! project memory, reversed from a live optimized Teams call.

#[cfg(feature = "engine")]
pub mod bridge;
pub mod capture;
pub mod devices;
#[cfg(feature = "engine")]
pub mod dispatch;
#[cfg(feature = "engine")]
pub mod engine;
pub mod framing;
pub mod ice;
pub mod objects;
pub mod presentation;
pub mod rpc;
pub mod session;

pub use capture::{parse_capture, CaptureError, CaptureRecord, Direction};
pub use devices::{DeviceKind, DeviceProvider, MediaDevice, NO_GROUP};
pub use ice::TurnResolver;
pub use objects::ObjectType;
pub use presentation::{MediaElement, PresentationModel, Rect};
pub use rpc::{RpcMessage, RpcMessageKind};
pub use session::{RedirectorModel, SessionError};

#[cfg(feature = "engine")]
pub use bridge::{NativeRedirector, CHANNEL_NAME};
#[cfg(feature = "engine")]
pub use dispatch::Redirector;
#[cfg(feature = "engine")]
pub use engine::{EngineError, MediaSink, VideoCaptureSource, WebrtcEngine};
