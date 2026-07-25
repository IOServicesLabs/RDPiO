//! Phase B — drive the `webrtc-rs` engine with the inputs from a real captured
//! optimized-Teams call and prove it interoperates with Teams' actual SDP.
//!
//! Two directions of the same negotiation:
//!   1. As the offerer (our real role): fed the captured `createPeerConnection` +
//!      `addTransceiver` calls, the engine regenerates a structurally-equivalent
//!      offer — right m-lines, Teams' codecs (opus/VP8/H264), ICE + DTLS.
//!   2. As the answerer: fed Teams' *real* 41 KB offer SDP, the engine parses it
//!      and produces a valid answer — proving webrtc-rs understands every codec
//!      and extension Teams uses (the cross-compat that matters for a live call).
//!
//! Requires the `engine` feature: `cargo test -p rdp-webrtc --features engine`.

#![cfg(feature = "engine")]

use rdp_webrtc::framing::message_json;
use rdp_webrtc::{parse_capture, WebrtcEngine};
use serde_json::Value;

const FIXTURE: &[u8] = include_bytes!("fixtures/teams_call.wrtc");

/// The negotiation inputs pulled out of the captured call.
struct Inputs {
    config: Value,
    /// (kind, initial direction, transceiverRpcObjectId) for each addTransceiver
    /// whose args carry a media kind.
    transceivers: Vec<(String, String, u64)>,
    /// (transceiverRpcObjectId, direction) from later setDirection calls.
    directions: Vec<(u64, String)>,
    /// The SDP offer the add-in produced (createOffer result).
    offer_sdp: Option<String>,
    /// The SDP answer Teams' media server sent (setRemoteDescription arg).
    answer_sdp: Option<String>,
}

fn extract() -> Inputs {
    let records = parse_capture(FIXTURE).expect("capture parses");
    let mut config = Value::Null;
    let mut transceivers = Vec::new();
    let mut directions = Vec::new();
    let mut offer_sdp = None;
    let mut answer_sdp = None;
    let mut offer_call_ids = std::collections::HashSet::new();

    for r in &records {
        let Ok(v) = serde_json::from_slice::<Value>(message_json(&r.payload)) else {
            continue;
        };
        let name = v.get("rpcName").and_then(Value::as_str).unwrap_or("");
        let arg0 = v.get("rpcArgs").and_then(|a| a.get(0));

        match name {
            "createPeerConnection" => {
                if config.is_null() {
                    if let Some(a0) = arg0 {
                        config = a0.clone();
                    }
                }
            }
            "addTransceiver" => {
                if let Some(a0) = arg0 {
                    if let Some(kind) = a0.get("kind").and_then(Value::as_str) {
                        let dir = a0
                            .get("direction")
                            .and_then(Value::as_str)
                            .unwrap_or("inactive")
                            .to_string();
                        let id = a0.get("transceiverRpcObjectId").and_then(Value::as_u64).unwrap_or(0);
                        transceivers.push((kind.to_string(), dir, id));
                    }
                }
            }
            "setDirection" => {
                let id = v.get("rpcObjectId").and_then(Value::as_u64).unwrap_or(0);
                if let Some(d) = arg0.and_then(|a| a.get("direction")).and_then(Value::as_str) {
                    directions.push((id, d.to_string()));
                }
            }
            "createOffer" => {
                if let Some(cid) = v.get("rpcCallId").and_then(Value::as_u64) {
                    offer_call_ids.insert(cid);
                }
            }
            "setRemoteDescription" => {
                if let Some(sdp) = arg0.and_then(|a| a.get("sdp")).and_then(Value::as_str) {
                    answer_sdp = Some(sdp.to_string());
                }
            }
            _ => {}
        }

        // createOffer result → the offer SDP.
        if let (Some(result), Some(cid)) =
            (v.get("result"), v.get("rpcCallId").and_then(Value::as_u64))
        {
            if offer_call_ids.contains(&cid) {
                if let Some(sdp) = result.get("desc").and_then(|d| d.get("sdp")).and_then(Value::as_str) {
                    offer_sdp = Some(sdp.to_string());
                }
            }
        }
    }

    Inputs { config, transceivers, directions, offer_sdp, answer_sdp }
}

fn mline_count(sdp: &str) -> usize {
    sdp.lines().filter(|l| l.starts_with("m=")).count()
}

#[tokio::test]
async fn engine_regenerates_a_teams_style_offer() {
    let inp = extract();
    assert!(!inp.transceivers.is_empty(), "no transceivers extracted from capture");

    let mut engine = WebrtcEngine::new();
    engine.create_peer_connection(&inp.config).await.expect("create pc with Teams ICE config");
    for (kind, dir, id) in &inp.transceivers {
        // Synthesize a distinct sender object id (the replay only exercises the offer
        // shape, not `replaceTrack`, so the exact value is irrelevant as long as it's
        // unique per transceiver).
        engine
            .add_transceiver(kind, dir, *id, *id + 100_000, false)
            .await
            .unwrap_or_else(|e| panic!("add_transceiver(kind={kind}, dir={dir}, id={id}) failed: {e:?}"));
    }
    for (id, dir) in &inp.directions {
        let _ = engine.set_transceiver_direction(*id, dir).await;
    }

    let offer = engine.create_offer().await.expect("create offer");

    assert!(offer.starts_with("v=0"), "offer is not SDP");
    assert_eq!(
        mline_count(&offer),
        inp.transceivers.len(),
        "one m-line per transceiver"
    );
    assert!(offer.contains("m=audio"), "no audio m-line");
    assert!(offer.contains("m=video"), "no video m-line");
    let low = offer.to_lowercase();
    for codec in ["opus", "vp8", "h264"] {
        assert!(low.contains(codec), "offer is missing {codec} (Teams needs it)");
    }
    assert!(offer.contains("a=ice-ufrag:"), "no ICE ufrag");
    assert!(offer.contains("a=fingerprint:"), "no DTLS fingerprint");

    // The offer applies cleanly to our own peer connection.
    engine.set_local_offer(&offer).await.expect("set local offer");
}

#[tokio::test]
async fn engine_answers_teams_real_offer() {
    // The strongest interop proof: consume Teams' actual captured offer SDP (all
    // its codecs, header extensions, ICE, DTLS) and produce a valid answer.
    let inp = extract();
    let teams_offer = inp.offer_sdp.expect("captured Teams offer SDP");
    assert!(teams_offer.len() > 10_000, "expected the large real offer");

    let mut engine = WebrtcEngine::new();
    engine.create_peer_connection(&inp.config).await.expect("create pc");
    engine
        .set_remote_offer(&teams_offer)
        .await
        .expect("webrtc-rs accepts Teams' real offer SDP");
    let answer = engine.create_answer().await.expect("create answer");

    assert!(answer.starts_with("v=0"), "answer is not SDP");
    assert_eq!(
        mline_count(&answer),
        mline_count(&teams_offer),
        "answer must mirror the offer's m-lines"
    );
    assert!(answer.to_lowercase().contains("opus"), "answer negotiated no opus");
    // The captured answer exists too — sanity that we parsed the pair.
    assert!(inp.answer_sdp.is_some(), "capture also contained Teams' answer");
}
