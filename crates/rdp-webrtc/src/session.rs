//! Redirector session model.
//!
//! This is the state machine that sits behind the webrtc.1 channel. In its final
//! form it will own a real WebRTC engine (`webrtc-rs`): consume server **Calls**,
//! drive the engine, and emit **Result**/**Event** messages back. This first
//! stage implements the *observational* core — feed it the messages from a real
//! captured call (either direction) and it reconstructs the remoted object graph
//! and the signaling exchange. That both validates our understanding of the
//! protocol against ground truth and defines the exact surface the engine layer
//! must satisfy.
//!
//! The same [`RedirectorModel::observe`] entry point will drive the live engine:
//! a captured server Call and a live server Call are identical inputs; only the
//! *reaction* (record vs. execute) changes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;

use crate::objects::ObjectType;
use crate::rpc::{RpcMessage, RpcMessageKind};
use crate::Direction;

/// Errors from feeding raw bytes to the model.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("message JSON did not parse: {0}")]
    Json(#[from] serde_json::Error),
}

/// What we learned about a server Call, kept so its Result can be correlated.
#[derive(Debug, Clone)]
pub struct CallInfo {
    pub object_type: ObjectType,
    pub method: String,
}

/// The WebRTC signaling reconstructed from the message stream.
#[derive(Debug, Default)]
pub struct Signaling {
    /// ICE/TURN server URLs from `createPeerConnection`.
    pub ice_servers: Vec<String>,
    /// The SDP offer the client produced (`createOffer` result).
    pub offer_sdp: Option<String>,
    /// The SDP answer the server applied (`setRemoteDescription` arg).
    pub answer_sdp: Option<String>,
    /// `addTransceiver` calls seen.
    pub transceivers: usize,
    /// Most `a=candidate:` lines seen in a single local-description event (the
    /// local SDP grows one candidate at a time as ICE gathers, so the max is the
    /// final gathered-candidate count).
    pub local_candidates: usize,
}

/// Observational model of one redirector session.
#[derive(Debug, Default)]
pub struct RedirectorModel {
    /// `rpcObjectId` → its type, for every object the server created/addressed.
    pub objects: HashMap<u64, ObjectType>,
    /// Pending/known server calls, keyed by `rpcCallId`.
    pub calls: HashMap<u64, CallInfo>,
    /// Total messages observed (both directions).
    pub total: usize,
    pub calls_seen: usize,
    pub results_seen: usize,
    /// Results whose `rpcCallId` matched a server Call we saw.
    pub results_matched: usize,
    pub events_seen: usize,
    pub parse_errors: usize,
    /// How often each `rpcName` was invoked.
    pub method_counts: BTreeMap<String, usize>,
    /// How often each `rpcObjectType` appeared.
    pub object_type_counts: BTreeMap<String, usize>,
    /// `rpcObjectType` strings we don't map yet (should stay empty).
    pub unknown_types: BTreeSet<String>,
    pub signaling: Signaling,
}

/// Follow a path of object keys into a JSON value (`v["a"]["b"]`).
fn dig<'a>(mut v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    for key in path {
        v = v.get(key)?;
    }
    Some(v)
}

/// The first element of a JSON array (an `rpcArgs`/`rpcResult` positional arg).
fn arg0(v: &Value) -> Option<&Value> {
    v.as_array()?.first()
}

/// Count `a=candidate:` lines in an SDP blob.
fn count_candidates(sdp: &str) -> usize {
    sdp.match_indices("a=candidate:").count()
}

impl RedirectorModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse and observe one raw message (framing already stripped is fine, but
    /// this also tolerates a framed message — it strips to the first NUL).
    pub fn observe_raw(&mut self, dir: Direction, raw: &[u8]) {
        let json = crate::framing::message_json(raw);
        match RpcMessage::parse(json) {
            Ok(msg) => self.observe(dir, &msg),
            Err(_) => {
                self.parse_errors += 1;
            }
        }
    }

    /// Observe one parsed message and fold it into the model.
    pub fn observe(&mut self, _dir: Direction, msg: &RpcMessage) {
        self.total += 1;

        if let Some(t) = &msg.object_type {
            *self.object_type_counts.entry(t.clone()).or_default() += 1;
            if ObjectType::from_name(t) == ObjectType::Unknown {
                self.unknown_types.insert(t.clone());
            }
        }

        match msg.kind() {
            RpcMessageKind::Call => self.observe_call(msg),
            RpcMessageKind::Result => self.observe_result(msg),
            RpcMessageKind::Event => self.observe_event(msg),
            RpcMessageKind::Other => {}
        }
    }

    fn observe_call(&mut self, msg: &RpcMessage) {
        self.calls_seen += 1;
        let object_type = msg
            .object_type
            .as_deref()
            .map(ObjectType::from_name)
            .unwrap_or(ObjectType::Unknown);
        let method = msg.name.clone().unwrap_or_default();
        *self.method_counts.entry(method.clone()).or_default() += 1;

        // Any addressed/created object id → remember its type.
        if let Some(id) = msg.object_id_u64() {
            self.objects.insert(id, object_type);
        }
        // Correlate the eventual Result by call id.
        if let Some(cid) = msg.call_id {
            self.calls.insert(cid, CallInfo { object_type, method: method.clone() });
        }

        // Signaling extraction from the Call arguments.
        let args = msg.args.clone().unwrap_or(Value::Null);
        match method.as_str() {
            "createPeerConnection" => {
                if let Some(servers) = arg0(&args).and_then(|a| a.get("iceServers")).and_then(|s| s.as_array()) {
                    for s in servers {
                        if let Some(urls) = s.get("urls").and_then(|u| u.as_array()) {
                            for u in urls {
                                if let Some(u) = u.as_str() {
                                    self.signaling.ice_servers.push(u.to_string());
                                }
                            }
                        }
                    }
                }
            }
            "addTransceiver" => self.signaling.transceivers += 1,
            "setRemoteDescription" => {
                if let Some(sdp) = arg0(&args).and_then(|a| a.get("sdp")).and_then(|s| s.as_str()) {
                    self.signaling.answer_sdp = Some(sdp.to_string());
                }
            }
            _ => {}
        }
    }

    fn observe_result(&mut self, msg: &RpcMessage) {
        self.results_seen += 1;
        let matched = msg.call_id.and_then(|cid| self.calls.get(&cid).cloned());
        if let Some(info) = &matched {
            self.results_matched += 1;
            // createOffer's result carries the local SDP offer.
            if info.method == "createOffer" || info.method == "createAnswer" {
                if let Some(result) = &msg.result {
                    if let Some(sdp) = dig(result, &["desc", "sdp"]).and_then(|s| s.as_str()) {
                        self.signaling.offer_sdp = Some(sdp.to_string());
                    }
                }
            }
        }
    }

    fn observe_event(&mut self, msg: &RpcMessage) {
        self.events_seen += 1;
        // Local-description update events carry the growing local SDP with ICE
        // candidates; track the peak candidate count.
        if let Some(ev) = &msg.event_args {
            if let Some(sdp) = dig(ev, &["desc", "sdp"]).and_then(|s| s.as_str()) {
                self.signaling.local_candidates = self.signaling.local_candidates.max(count_candidates(sdp));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstructs_a_minimal_offer_exchange() {
        let mut m = RedirectorModel::new();
        // createPeerConnection with one TURN server.
        m.observe_raw(
            Direction::Inbound,
            br#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":11,"rpcName":"createPeerConnection","rpcArgs":[{"iceServers":[{"urls":["turn:relay.example:3478"]}]}],"rpcCallId":1}"#,
        );
        // addTransceiver twice (audio + video).
        for _ in 0..2 {
            m.observe_raw(
                Direction::Inbound,
                br#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":11,"rpcName":"addTransceiver","rpcArgs":["audio"],"rpcCallId":2}"#,
            );
        }
        // createOffer call, then its Result with an SDP.
        m.observe_raw(
            Direction::Inbound,
            br#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":11,"rpcName":"createOffer","rpcArgs":[],"rpcCallId":9}"#,
        );
        m.observe_raw(
            Direction::Outbound,
            br#"{"result":{"desc":{"type":"offer","sdp":"v=0\r\na=candidate:1 1 udp 1 1.2.3.4 5 typ host"}},"rpcCallId":9}"#,
        );
        // A local-description event with two candidates.
        m.observe_raw(
            Direction::Outbound,
            br#"{"rpcEventArgs":{"desc":{"sdp":"v=0\r\na=candidate:1 1 udp 1 1.2.3.4 5 typ host\r\na=candidate:2 1 udp 1 6.7.8.9 5 typ srflx"}}}"#,
        );
        // setRemoteDescription (answer).
        m.observe_raw(
            Direction::Inbound,
            br#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":11,"rpcName":"setRemoteDescription","rpcArgs":[{"sdp":"v=0\r\ns=answer"}]}"#,
        );

        assert_eq!(m.objects.get(&11), Some(&ObjectType::PeerConnection));
        assert_eq!(m.signaling.ice_servers, vec!["turn:relay.example:3478"]);
        assert_eq!(m.signaling.transceivers, 2);
        assert!(m.signaling.offer_sdp.as_deref().unwrap().starts_with("v=0"));
        assert_eq!(m.signaling.local_candidates, 2);
        assert!(m.signaling.answer_sdp.as_deref().unwrap().contains("answer"));
        assert_eq!(m.results_matched, 1);
        assert!(m.unknown_types.is_empty());
    }
}
