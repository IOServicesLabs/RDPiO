//! Diagnostic: replay a real add-in capture's server Calls through the native
//! dispatcher and diff, method by method, our replies against the replies the
//! real `MsRdcWebRTCAddIn.dll` sent for the same Calls — JSON shape (key paths
//! and value types) and scalar values — plus the events each side emitted.
//! Teams in the Cloud PC decides what media to ask its service for partly from
//! these replies, so any structural gap here is a lead the SDP diffs can't show.
//!
//! Report-only (`#[ignore]`): `cargo test -p rdp-webrtc --features engine
//! --test reply_audit -- --ignored --nocapture`.

#![cfg(feature = "engine")]

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rdp_webrtc::framing::message_json;
use rdp_webrtc::rpc::RpcMessage;
use rdp_webrtc::{parse_capture, Direction, Redirector};
use serde_json::Value;

const FIXTURE: &[u8] = include_bytes!("fixtures/teams_call.wrtc");

/// Methods whose replies are dominated by SDP/ICE payloads, already diffed
/// exhaustively by the SDP tooling.
const SIGNALING: [&str; 4] = [
    "createOffer",
    "setLocalDescription",
    "setRemoteDescription",
    "addIceCandidate",
];

/// Key paths → value type, recursively (array elements collapse to `[]`).
fn shape(v: &Value, path: &str, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            if m.is_empty() {
                out.insert(format!("{path}: {{}}"));
            }
            for (k, v) in m {
                shape(v, &format!("{path}.{k}"), out);
            }
        }
        Value::Array(a) => {
            if a.is_empty() {
                out.insert(format!("{path}: []"));
            }
            for v in a {
                shape(v, &format!("{path}[]"), out);
            }
        }
        Value::String(_) => {
            out.insert(format!("{path}: str"));
        }
        Value::Number(_) => {
            out.insert(format!("{path}: num"));
        }
        Value::Bool(_) => {
            out.insert(format!("{path}: bool"));
        }
        Value::Null => {
            out.insert(format!("{path}: null"));
        }
    }
}

/// Scalar leaves (path → value) worth comparing: skip SDP text, ids, and
/// anything that is per-session noise.
fn scalars(v: &Value, path: &str, out: &mut BTreeMap<String, String>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                let noisy = matches!(
                    k.as_str(),
                    "sdp" | "candidate" | "rpcObjectId" | "rpcCallId" | "id" | "deviceId"
                        | "groupId" | "timestamp" | "usernameFragment"
                );
                if !noisy {
                    scalars(v, &format!("{path}.{k}"), out);
                }
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate().take(4) {
                scalars(v, &format!("{path}[{i}]"), out);
            }
        }
        other => {
            let s = other.to_string();
            out.insert(path.to_string(), s.chars().take(80).collect());
        }
    }
}

/// A stable label for an event: its target type plus the event-args keys.
fn event_label(v: &Value) -> String {
    let target = v
        .get("rpcEventTarget")
        .and_then(|t| t.get("rpcObjectType"))
        .and_then(|t| t.as_str())
        .unwrap_or("?");
    let args = v.get("rpcEventArgs");
    let name = args
        .and_then(|a| a.get("type").or_else(|| a.get("name")))
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let keys: Vec<&str> = args
        .and_then(|a| a.as_object())
        .map(|m| m.keys().map(|k| k.as_str()).collect())
        .unwrap_or_default();
    format!("{target} {name} {{{}}}", keys.join(","))
}

#[derive(Default)]
struct MethodDiff {
    calls: usize,
    ours_missing_reply: usize,
    addin_missing_reply: usize,
    missing_in_ours: BTreeSet<String>,
    extra_in_ours: BTreeSet<String>,
    value_diffs: BTreeMap<String, (String, String)>,
}

#[tokio::test]
#[ignore = "diagnostic report; run with --ignored --nocapture"]
async fn audit_replies_against_the_real_add_in() {
    let records = parse_capture(FIXTURE).expect("capture parses");

    // The add-in's side: replies by call id, and every event it emitted.
    let mut addin_replies: HashMap<u64, Value> = HashMap::new();
    let mut addin_events: BTreeMap<String, usize> = BTreeMap::new();
    for r in records.iter().filter(|r| r.dir != Direction::Inbound) {
        let Ok(v) = serde_json::from_slice::<Value>(message_json(&r.payload)) else {
            continue;
        };
        // A reply is any client message correlated by call id: most carry only
        // `hr` (no `result`), and the client's own Calls carry `rpcArgs`.
        let is_reply = v.get("rpcCallId").is_some() && v.get("rpcArgs").is_none();
        if is_reply {
            if let Some(id) = v.get("rpcCallId").and_then(|i| i.as_u64()) {
                addin_replies.insert(id, v);
            }
        } else if v.get("rpcEventArgs").is_some() || v.get("rpcEventTarget").is_some() {
            *addin_events.entry(event_label(&v)).or_default() += 1;
        }
    }

    // Our side: drive the dispatcher with the same Calls.
    let mut redirector = Redirector::new();
    let mut diffs: BTreeMap<String, MethodDiff> = BTreeMap::new();
    let mut our_events: BTreeMap<String, usize> = BTreeMap::new();
    for r in records.iter().filter(|r| r.dir == Direction::Inbound) {
        let Ok(msg) = RpcMessage::parse(message_json(&r.payload)) else {
            continue;
        };
        let method = format!(
            "{}.{}",
            msg.object_type.as_deref().unwrap_or("?"),
            msg.name.as_deref().unwrap_or("?")
        );
        let mut out = redirector.handle(&msg).await;
        out.extend(redirector.drain_ice().await);
        let ours = msg.call_id.and_then(|id| {
            out.iter()
                .find(|m| m.get("rpcCallId").and_then(|i| i.as_u64()) == Some(id))
                .cloned()
        });
        for m in &out {
            if m.get("rpcEventArgs").is_some() || m.get("rpcEventTarget").is_some() {
                *our_events.entry(event_label(m)).or_default() += 1;
            }
        }
        let Some(id) = msg.call_id else { continue };
        let d = diffs.entry(method.clone()).or_default();
        d.calls += 1;
        let theirs = addin_replies.get(&id);
        match (&ours, theirs) {
            (None, Some(_)) => d.ours_missing_reply += 1,
            (Some(_), None) => d.addin_missing_reply += 1,
            (None, None) => {}
            (Some(o), Some(t)) => {
                // The status code first: a success where the add-in failed (or
                // the reverse) changes what Teams does next.
                let hr = |v: &Value| v.get("hr").map(|h| h.to_string()).unwrap_or("-".into());
                if hr(o) != hr(t) {
                    d.value_diffs
                        .entry(".hr".into())
                        .or_insert((hr(o), hr(t)));
                }
                let (mut so, mut st) = (BTreeSet::new(), BTreeSet::new());
                shape(o, "", &mut so);
                shape(t, "", &mut st);
                d.missing_in_ours.extend(st.difference(&so).cloned());
                d.extra_in_ours.extend(so.difference(&st).cloned());
                if !SIGNALING.iter().any(|s| method.ends_with(s)) {
                    let (mut vo, mut vt) = (BTreeMap::new(), BTreeMap::new());
                    scalars(o, "", &mut vo);
                    scalars(t, "", &mut vt);
                    for (k, tv) in &vt {
                        match vo.get(k) {
                            Some(ov) if ov != tv => {
                                d.value_diffs
                                    .entry(k.clone())
                                    .or_insert((ov.clone(), tv.clone()));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    eprintln!("\n===== reply audit: ours vs add-in, per method =====");
    for (method, d) in &diffs {
        let clean = d.ours_missing_reply == 0
            && d.addin_missing_reply == 0
            && d.missing_in_ours.is_empty()
            && d.extra_in_ours.is_empty()
            && d.value_diffs.is_empty();
        if clean {
            continue;
        }
        eprintln!(
            "\n{method}  calls={} ours_no_reply={} addin_no_reply={}",
            d.calls, d.ours_missing_reply, d.addin_missing_reply
        );
        for p in d.missing_in_ours.iter().take(25) {
            eprintln!("   - add-in has, we lack: {p}");
        }
        for p in d.extra_in_ours.iter().take(15) {
            eprintln!("   + we have, add-in lacks: {p}");
        }
        for (k, (o, t)) in d.value_diffs.iter().take(15) {
            eprintln!("   ~ {k}: ours={o} add-in={t}");
        }
    }
    let clean: Vec<&String> = diffs
        .iter()
        .filter(|(_, d)| {
            d.ours_missing_reply == 0
                && d.addin_missing_reply == 0
                && d.missing_in_ours.is_empty()
                && d.extra_in_ours.is_empty()
                && d.value_diffs.is_empty()
        })
        .map(|(m, _)| m)
        .collect();
    eprintln!("\nidentical ({}): {:?}", clean.len(), clean);

    eprintln!("\n===== events: add-in count vs ours =====");
    let labels: BTreeSet<&String> = addin_events.keys().chain(our_events.keys()).collect();
    for l in labels {
        let (a, o) = (
            addin_events.get(l).copied().unwrap_or(0),
            our_events.get(l).copied().unwrap_or(0),
        );
        let mark = match (a, o) {
            (_, 0) => "MISSING",
            (0, _) => "extra",
            _ => "",
        };
        eprintln!("  add-in={a:<4} ours={o:<4} {mark:<8} {l}");
    }
}
