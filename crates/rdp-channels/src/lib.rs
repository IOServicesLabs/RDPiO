//! Virtual channel multiplexing.
//!
//! Two layers live here:
//! - **Static channels** joined during MCS activation (the I/O channel plus any
//!   statically-defined virtual channels advertised in the GCC network data).
//! - **DRDYNVC** (MS-RDPEDYC), the dynamic virtual channel manager that runs on
//!   top of a static channel and carries RDPGFX, RDPSND, CLIPRDR, and friends.
#![forbid(unsafe_code)]

/// Well-known dynamic virtual channel names we intend to open.
pub mod names {
    /// The RDPGFX graphics pipeline endpoint (MS-RDPEGFX, opened over DRDYNVC).
    pub const GRAPHICS: &str = "Microsoft::Windows::RDS::Graphics";
    /// The microphone (audio input) endpoint (MS-RDPEAI, opened over DRDYNVC).
    pub const AUDIO_INPUT: &str = "AUDIO_INPUT";
    /// The speaker (audio output) endpoint (MS-RDPEA). Modern Windows streams the
    /// session's audio over this dynamic channel rather than the static `rdpsnd`
    /// SVC; the RDPSND PDUs are identical, just a different transport.
    pub const AUDIO_PLAYBACK: &str = "AUDIO_PLAYBACK_DVC";
    /// Unreliable/UDP variant of the speaker endpoint. We accept it so the server
    /// does not treat audio redirection as unsupported, even though all audio data
    /// is currently handled over the reliable channel.
    pub const AUDIO_PLAYBACK_LOSSY: &str = "AUDIO_PLAYBACK_LOSSY_DVC";
    /// The camera enumerator endpoint (MS-RDPECAM, opened over DRDYNVC).
    pub const CAMERA_ENUMERATOR: &str = "RDCamera_Device_Enumerator";
    /// Prefix for the per-device camera channels the client names (the server
    /// opens one per announced camera). The DVC demuxer routes channels with
    /// this prefix to the camera handler.
    pub const CAMERA_DEVICE_PREFIX: &str = "rdpio_cam";
    /// The multi-touch/pen input endpoint (MS-RDPEI, opened over DRDYNVC).
    pub const RDPINPUT: &str = "Microsoft::Windows::RDS::Input";

    // --- Server-initiated media/video *redirection* channels we deliberately
    // decline. Accepting any of these without implementing its full redirector
    // protocol makes the host route video *out* of the RDPGFX graphics pipeline
    // into a stream we don't decode (→ black video); declining makes the host
    // fall back to encoding that video into the graphics channel, which we DO
    // decode. Matched by prefix because the server suffixes versions (…v08.01)
    // and instance numbers (…webrtc.1). See [`is_redirection_channel`].

    /// MS-RDPEVOR "video optimized remoting" control channel.
    pub const VIDEO_CONTROL_PREFIX: &str = "Microsoft::Windows::RDS::Video::Control";
    /// MS-RDPEVOR video *sample* stream.
    pub const VIDEO_DATA_PREFIX: &str = "Microsoft::Windows::RDS::Video::Data";
    /// MS-RDPEGT geometry-tracking channel (tells the client where video regions
    /// are so a redirector can overlay them); useless without a redirector.
    pub const GEOMETRY_PREFIX: &str = "Microsoft::Windows::RDS::Geometry";
    /// Teams "Optimized" WebRTC media redirector (`…webrtc.1`). Needs a bridge to
    /// Microsoft's `MsRdcWebRTCAddIn.dll` / SlimCore engine — not implemented.
    pub const WEBRTC_REDIRECTOR_PREFIX: &str = "com.microsoft.rdc.dvc.webrtc";
    /// Multimedia Redirection (`…mmr.1`): browser/media-player video fetched and
    /// decoded on the client. Big separate feature — not implemented.
    pub const MMR_REDIRECTOR_PREFIX: &str = "com.microsoft.rdc.dvc.mmr";

    /// True if `name` is a server-side media/video *redirection* channel that
    /// rdpio intentionally declines so that video stays in the graphics pipeline
    /// we decode. Kept separate from truly-unknown channels for clear logging.
    pub fn is_redirection_channel(name: &str) -> bool {
        name.starts_with(VIDEO_CONTROL_PREFIX)
            || name.starts_with(VIDEO_DATA_PREFIX)
            || name.starts_with(GEOMETRY_PREFIX)
            || name.starts_with(WEBRTC_REDIRECTOR_PREFIX)
            || name.starts_with(MMR_REDIRECTOR_PREFIX)
    }
}

pub mod drdynvc;

pub mod svc;

pub mod cliprdr;

pub mod rdpsnd;

pub mod rdpdr;

pub mod disp;

pub mod audio_input;

pub mod emt;

pub mod camera;

pub mod rdpei;

pub mod serial;
