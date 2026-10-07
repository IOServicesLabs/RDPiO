//! Replay the captured call through the presentation model and assert it
//! reconstructs the media surfaces: the video elements, their stream bindings,
//! self-view mirroring, and the redirected clip rect — the "where to render"
//! state a native renderer needs. Pure (no engine feature required).

use rdp_webrtc::framing::message_json;
use rdp_webrtc::rpc::RpcMessage;
use rdp_webrtc::{parse_capture, PresentationModel};

const FIXTURE: &[u8] = include_bytes!("fixtures/teams_call.wrtc");

fn build() -> PresentationModel {
    let records = parse_capture(FIXTURE).expect("capture parses");
    let mut model = PresentationModel::new();
    for r in &records {
        if let Ok(msg) = RpcMessage::parse(message_json(&r.payload)) {
            model.observe(&msg);
        }
    }
    model
}

/// The ids of the video elements the capture created, and of those it shut down.
fn video_element_lifecycle() -> (Vec<u64>, Vec<u64>) {
    let records = parse_capture(FIXTURE).expect("capture parses");
    let (mut created, mut shut) = (Vec::new(), Vec::new());
    for r in &records {
        let Ok(msg) = RpcMessage::parse(message_json(&r.payload)) else { continue };
        let first_arg = msg.args.as_ref().and_then(|a| a.get(0)).and_then(|v| v.as_str());
        match (msg.name.as_deref(), msg.object_id_u64()) {
            (Some("createMediaElement"), Some(id)) if first_arg == Some("video") => created.push(id),
            (Some("shutdown"), Some(id)) if msg.object_type.as_deref() == Some("MediaElement") => {
                shut.push(id)
            }
            _ => {}
        }
    }
    (created, shut)
}

#[test]
fn reconstructs_the_media_surfaces() {
    let model = build();

    // The call presented several video elements (self-view + participants),
    // and the ones Teams shut down are gone from the model (never drawn again).
    let (created, shut) = video_element_lifecycle();
    assert!(created.len() >= 3, "expected multiple video elements, got {created:?}");
    for id in &shut {
        assert!(!model.elements.contains_key(id), "shut-down element {id} still present");
    }
    assert!(
        created.iter().any(|id| model.elements.contains_key(id)),
        "no live video element left at the end of the capture"
    );

    // Every video element is bound to a source stream.
    for (id, e) in model.elements.iter().filter(|(_, e)| e.kind == "video") {
        assert!(e.src_stream_id.is_some(), "video element {id} has no srcObject stream");
    }

    // Streams carry tracks.
    assert!(
        model.streams.values().any(|s| !s.tracks.is_empty()),
        "no streams with tracks reconstructed"
    );

    // The redirected surface has a clip rect.
    let clip = model.clip_rect.expect("no clip rect observed");
    assert!(!clip.is_empty(), "clip rect is empty: {clip:?}");
}

#[test]
fn identifies_self_view_by_mirror_transform() {
    let model = build();
    // Teams mirrors the local self-view (scaleX(-1)); at least one element is.
    assert!(
        model.elements.values().any(|e| e.is_mirrored()),
        "expected a mirrored self-view element"
    );
    // object-fit was captured for presented elements.
    assert!(
        model.elements.values().any(|e| e.object_fit.as_deref() == Some("contain")),
        "expected an object-fit:contain element"
    );
}

#[test]
fn print_presentation_summary() {
    let model = build();
    eprintln!("\n=== presentation model ===");
    eprintln!(
        "elements: {} ({} video)  streams: {}  clip: {:?} visible={}",
        model.elements.len(),
        model.elements.values().filter(|e| e.kind == "video").count(),
        model.streams.len(),
        model.clip_rect,
        model.clip_visible,
    );
    for (id, e) in {
        let mut v: Vec<_> = model.elements.iter().collect();
        v.sort_by_key(|(id, _)| **id);
        v
    } {
        eprintln!(
            "  elem {id}: kind={} src_stream={:?} fit={:?} transform={:?} visible={}",
            e.kind, e.src_stream_id, e.object_fit, e.transform, e.visible
        );
    }
}
