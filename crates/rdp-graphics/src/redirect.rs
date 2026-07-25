//! Bridge point for hosting an external dynamic-virtual-channel *redirector*
//! (e.g. Microsoft's Teams "Optimized" WebRTC add-in, `MsRdcWebRTCAddIn.dll`),
//! whose channels the graphics DVC mux would otherwise decline.
//!
//! The graphics channel stays free of any platform / COM concern: it merely asks
//! the (optional) redirector whether to accept an otherwise-unknown channel,
//! forwards that channel's reassembled data to it, and drains whatever bytes the
//! redirector wants to put back on the wire. The Windows-specific COM host that
//! actually loads and drives the add-in lives in `rdp-client`.

/// A pluggable handler for dynamic channels rdpio does not natively implement.
///
/// `Send` so the mux can own it on the session thread; the implementor is
/// expected to marshal to whatever apartment/thread the hosted add-in requires
/// (the WebRTC add-in runs its own media threads). Every method must be
/// non-blocking from the mux's perspective — the session loop calls them inline.
pub trait DvcRedirector: Send {
    /// Whether this redirector claims `name` — a channel the mux is about to
    /// decline. Cheap; called for every unhandled create-request.
    fn claims(&self, name: &str) -> bool;

    /// A create-request for a claimed channel arrived. Return `true` to accept it
    /// (the mux replies success and routes the channel's data here) or `false` to
    /// let the mux decline it as usual.
    fn on_create(&mut self, channel_id: u32, name: &str) -> bool;

    /// A complete (reassembled) message arrived on a channel we accepted.
    fn on_data(&mut self, channel_id: u32, message: &[u8]);

    /// The server closed a channel we accepted.
    fn on_close(&mut self, channel_id: u32);

    /// Drain `(channel_id, payload)` pairs the redirector wants sent to the
    /// server. Called every mux poll because the add-in produces data
    /// asynchronously on its own threads, not only in response to inbound data.
    fn drain_outbound(&mut self) -> Vec<(u32, Vec<u8>)>;
}
