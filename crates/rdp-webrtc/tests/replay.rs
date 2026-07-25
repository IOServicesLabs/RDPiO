//! Replay a real captured optimized-Teams call through the redirector model and
//! assert it reconstructs the protocol exactly. This is the ground-truth check
//! that our understanding of the webrtc.1 wire is correct — and the fixture the
//! engine layer will later be driven by. The capture (`tests/fixtures/
//! teams_call.wrtc`) was recorded through the Windows bridge with
//! `RDPIO_WEBRTC_CAPTURE`; the ICE/DTLS/TURN material in it is ephemeral,
//! single-session, and long expired.

use rdp_webrtc::{parse_capture, RedirectorModel};

const FIXTURE: &[u8] = include_bytes!("fixtures/teams_call.wrtc");

fn build_model() -> RedirectorModel {
    let records = parse_capture(FIXTURE).expect("capture parses");
    let mut model = RedirectorModel::new();
    for r in &records {
        model.observe_raw(r.dir, &r.payload);
    }
    model
}

#[test]
fn capture_parses_into_many_records() {
    let records = parse_capture(FIXTURE).expect("capture parses");
    assert!(records.len() > 300, "expected a substantial call, got {}", records.len());
    // Both directions must be represented.
    assert!(records.iter().any(|r| r.dir == rdp_webrtc::Direction::Inbound));
    assert!(records.iter().any(|r| r.dir == rdp_webrtc::Direction::Outbound));
}

#[test]
fn every_message_is_valid_json_after_framing() {
    let model = build_model();
    // Framing (strip at first NUL) must yield valid JSON for every message; any
    // parse error means we've misunderstood the framing.
    assert_eq!(
        model.parse_errors, 0,
        "{} messages failed to parse as JSON after de-framing",
        model.parse_errors
    );
}

#[test]
fn all_object_types_are_recognized() {
    let model = build_model();
    assert!(
        model.unknown_types.is_empty(),
        "unmapped rpcObjectType(s): {:?}",
        model.unknown_types
    );
}

#[test]
fn reconstructs_the_object_model() {
    let model = build_model();
    // The core WebRTC objects must all appear.
    for t in [
        "RTCPeerConnection",
        "RTCRtpTransceiver",
        "MediaStream",
        "MediaStreamTrack",
        "MediaElement",
        "MediaDevices",
        "RDWebRTCRedirector",
    ] {
        assert!(
            model.object_type_counts.contains_key(t),
            "expected object type {t} in the call"
        );
    }
    // A peer connection object was created and tracked by id.
    assert!(
        model.objects.values().any(|&t| t == rdp_webrtc::ObjectType::PeerConnection),
        "no RTCPeerConnection object was registered"
    );
}

#[test]
fn reconstructs_the_signaling_exchange() {
    let model = build_model();
    let s = &model.signaling;

    // ICE servers came from createPeerConnection and include a Teams relay.
    assert!(!s.ice_servers.is_empty(), "no ICE servers extracted");
    assert!(
        s.ice_servers.iter().any(|u| u.contains("turn")),
        "expected a TURN server, got {:?}",
        s.ice_servers
    );

    // Transceivers were added (audio/video/appshare).
    assert!(s.transceivers > 0, "no transceivers seen");

    // The client produced an SDP offer (createOffer result) that looks real.
    let offer = s.offer_sdp.as_deref().expect("no offer SDP captured");
    assert!(offer.starts_with("v=0"), "offer isn't SDP: {:?}", &offer[..offer.len().min(40)]);
    assert!(offer.contains("m=audio"), "offer has no audio m-line");
    assert!(offer.contains("a=ice-ufrag:"), "offer has no ICE ufrag");

    // Trickle ICE: local-description events accumulated candidates.
    assert!(s.local_candidates > 0, "no local ICE candidates observed");

    // The server applied an answer.
    let answer = s.answer_sdp.as_deref().expect("no answer SDP captured");
    assert!(answer.starts_with("v=0"), "answer isn't SDP");

    // Request/response correlation by rpcCallId works.
    assert!(model.results_matched > 0, "no results correlated to calls");
}

/// Not an assertion — prints the reconstructed call so `cargo test -- --nocapture`
/// shows the protocol picture at a glance.
#[test]
fn print_capture_summary() {
    let model = build_model();
    let s = &model.signaling;
    eprintln!("\n=== webrtc.1 capture reconstruction ===");
    eprintln!(
        "messages: {}  calls: {}  results: {} (matched {})  events: {}  parse_errors: {}",
        model.total, model.calls_seen, model.results_seen, model.results_matched,
        model.events_seen, model.parse_errors
    );
    eprintln!("objects tracked: {}", model.objects.len());
    eprintln!("object types: {:?}", model.object_type_counts);
    eprintln!("top methods:");
    let mut methods: Vec<_> = model.method_counts.iter().collect();
    methods.sort_by(|a, b| b.1.cmp(a.1));
    for (name, n) in methods.iter().take(12) {
        eprintln!("  {n:4}  {name}");
    }
    eprintln!(
        "signaling: {} ICE servers, {} transceivers, {} local candidates, offer={}B, answer={}B",
        s.ice_servers.len(),
        s.transceivers,
        s.local_candidates,
        s.offer_sdp.as_deref().map(|x| x.len()).unwrap_or(0),
        s.answer_sdp.as_deref().map(|x| x.len()).unwrap_or(0),
    );
}
