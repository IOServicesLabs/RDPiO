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

/// Several redirectors behind the mux's single slot (e.g. the Teams engine and
/// the Hyper handshake). Each channel belongs to the first member that claims
/// and accepts it.
#[derive(Default)]
pub struct ChainRedirector {
    members: Vec<Box<dyn DvcRedirector>>,
    owner: std::collections::HashMap<u32, usize>,
}

impl ChainRedirector {
    pub fn new(members: Vec<Box<dyn DvcRedirector>>) -> Self {
        Self { members, owner: Default::default() }
    }
}

impl DvcRedirector for ChainRedirector {
    fn claims(&self, name: &str) -> bool {
        self.members.iter().any(|m| m.claims(name))
    }

    fn on_create(&mut self, channel_id: u32, name: &str) -> bool {
        for (i, m) in self.members.iter_mut().enumerate() {
            if m.claims(name) && m.on_create(channel_id, name) {
                self.owner.insert(channel_id, i);
                return true;
            }
        }
        false
    }

    fn on_data(&mut self, channel_id: u32, message: &[u8]) {
        if let Some(&i) = self.owner.get(&channel_id) {
            self.members[i].on_data(channel_id, message);
        }
    }

    fn on_close(&mut self, channel_id: u32) {
        if let Some(i) = self.owner.remove(&channel_id) {
            self.members[i].on_close(channel_id);
        }
    }

    fn drain_outbound(&mut self) -> Vec<(u32, Vec<u8>)> {
        self.members.iter_mut().flat_map(|m| m.drain_outbound()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        name: &'static str,
        data: std::sync::Arc<std::sync::Mutex<Vec<(u32, Vec<u8>)>>>,
    }
    impl DvcRedirector for Fake {
        fn claims(&self, name: &str) -> bool {
            name == self.name
        }
        fn on_create(&mut self, _: u32, _: &str) -> bool {
            true
        }
        fn on_data(&mut self, id: u32, m: &[u8]) {
            self.data.lock().unwrap().push((id, m.to_vec()));
        }
        fn on_close(&mut self, _: u32) {}
        fn drain_outbound(&mut self) -> Vec<(u32, Vec<u8>)> {
            vec![(0, self.name.as_bytes().to_vec())]
        }
    }

    #[test]
    fn channels_route_to_the_member_that_claimed_them() {
        let a = Fake { name: "A", ..Default::default() };
        let b = Fake { name: "B", ..Default::default() };
        let (da, db) = (a.data.clone(), b.data.clone());
        let mut chain = ChainRedirector::new(vec![Box::new(a), Box::new(b)]);
        assert!(chain.claims("B") && !chain.claims("C"));
        assert!(chain.on_create(7, "B"));
        assert!(!chain.on_create(8, "C"));
        chain.on_data(7, b"hi");
        chain.on_data(8, b"lost");
        assert!(da.lock().unwrap().is_empty());
        assert_eq!(*db.lock().unwrap(), vec![(7, b"hi".to_vec())]);
        assert_eq!(chain.drain_outbound().len(), 2);
    }
}
