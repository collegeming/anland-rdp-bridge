// Trait/struct fields declared by the platform contract but only exercised by
// backends not yet wired (AAC encoding, VideoToolbox parameter sets, display-info
// queries). They are part of the trait surface, not dead code — the anland-only
// build just doesn't read them yet.
#![allow(dead_code)]

//! Platform abstraction layer between the RDP protocol logic and the
//! capture / encode / input backends.
//!
//! `anland-rdp-bridge` runs on Arch Linux ARM (Droidspaces), sourcing frames
//! from Android `MediaCodec` over a private Unix socket. The macOS backends
//! inherited from macrdp (ScreenCaptureKit + VideoToolbox + CGEventPost +
//! NSPasteboard) keep their existing direct wiring in the top-level modules
//! (`capture.rs`, `videotoolbox.rs`, `audio.rs`, `aac.rs`, `input.rs`,
//! `clipboard.rs`); this module is the **anland / Linux side only** — it is
//! compiled away on macOS so the strict macOS `clippy -D warnings` gate is
//! unaffected. The eventual migration step (see CLAUDE.md roadmap) lifts these
//! traits to be shared by both platforms.
//!
//! ## What is and isn't abstracted here
//!
//! - **Video frame source** — abstracted. The single biggest divergence:
//!   macrdp captures raw BGRA then encodes via VideoToolbox (AVCC → Annex-B);
//!   anland receives already-encoded Annex-B from Android MediaCodec and
//!   skips the encode step entirely. Both produce [`EncodedVideoFrame`] which
//!   the EGFX ship side consumes.
//! - **Audio source** — abstracted. macrdp taps ScreenCaptureKit for 32-bit
//!   float PCM and optionally encodes AAC via AudioToolbox; anland taps
//!   Android `AudioRecord` (PCM i16) and optionally encodes AAC via
//!   `MediaCodec`.
//! - **Clipboard / input / drive redirection** — NOT re-abstracted. These
//!   already implement upstream ironrdp traits (`CliprdrBackend`,
//!   `RdpServerInputHandler`, `RdpServerDisplay`). Each platform backend just
//!   implements the same upstream trait against a different data source
//!   (NSPasteboard vs Android clipboard manager; CGEventPost vs anland native
//!   input). No new trait is needed; the platform module only wires the choice.

mod anland;
pub use anland::AnlandBackends;

/// An already-encoded H.264 AVC420 frame ready to ship over the EGFX pipeline.
///
/// Both backends produce this struct; the EGFX ship side (backpressure state
/// machine + `GraphicsPipelineServer::send_avc420_frame`) consumes it. Neither
/// side encodes inside the ship path — the encode step (if any) happens before
/// this struct is built.
///
/// ## Annex-B framing (verified empirically, 2026-05-20 by macrdp)
///
/// mstsc's decoder requires Annex-B (`00 00 00 01` start codes). macrdp had to
/// convert VideoToolbox's native AVCC to Annex-B; anland's `MediaCodec` emits
/// Annex-B natively, so `data` is already in the correct framing.
#[derive(Debug, Clone)]
pub struct EncodedVideoFrame {
    /// H.264 NAL units in Annex-B start-code framing — the exact bytes
    /// mstsc's decoder expects.
    pub data: Vec<u8>,
    /// Whether this is an IDR (keyframe). The ship side waits for a keyframe
    /// before un-suppressing, and forces one after any backpressure skip so
    /// the client never applies P-frame deltas against frames it never got.
    pub is_keyframe: bool,
    /// Presentation timestamp, milliseconds.
    pub pts_ms: i64,
    /// SPS / PPS NAL units (raw, no length prefix and no start code).
    /// Populated only on keyframes. Empty for anland (in-band in the Annex-B
    /// stream MediaCodec emits); populated for macOS (VideoToolbox emits them
    /// out-of-band, required for the AVCC → Annex-B conversion).
    pub parameter_sets: Vec<Vec<u8>>,
    /// Logical display dimensions — the surface and AVC420 region are built
    /// from these, not from a fresh size read at ship time, so a size change
    /// between setup and ship can't tear the region away from the surface.
    pub display_width: u16,
    pub display_height: u16,
    /// Encoded buffer dimensions (may be 16-aligned larger than display).
    pub encoded_width: u16,
    pub encoded_height: u16,
}

/// Source of already-encoded H.264 frames for the EGFX ship pipeline.
///
/// The contract is a pull-based async stream: the ship loop (`ship_loop`)
/// awaits [`next_frame`](VideoFrameSource::next_frame) and hands each frame
/// to `send_avc420_frame`. Control signals (`start`/`stop`/`request_keyframe`)
/// flow in the other direction — the ship side drives the source on reconnect,
/// display suppression, and IDR recovery.
#[async_trait::async_trait]
pub trait VideoFrameSource: Send {
    /// Await the next encoded frame, or `Ok(None)` if the source ended.
    async fn next_frame(&mut self) -> anyhow::Result<Option<EncodedVideoFrame>>;

    /// Request that the next produced frame be an IDR (keyframe). Called on
    /// initial surface setup, after a backpressure-induced skip, on client
    /// reconnection, and after a geometry discontinuity.
    fn request_keyframe(&self);

    /// Start / resume frame production. Called once the EGFX channel is ready
    /// and AVC420 has been negotiated.
    fn start(&self);

    /// Stop / pause frame production. Called when the display is suppressed
    /// (client minimized via SuppressOutput) or the channel closed, so the
    /// upstream encoder drains naturally rather than backpressuring.
    fn stop(&self);
}

/// A chunk of audio ready to ship over the RDPSND channel.
#[derive(Debug, Clone)]
pub enum AudioChunk {
    /// 16-bit signed interleaved PCM (little-endian bytes) at the negotiated
    /// sample rate.
    Pcm {
        samples: Vec<u8>,
        sample_rate: u32,
        channels: u16,
        pts_ms: i64,
    },
    /// A raw AAC-LC access unit (object type 2, 1024 frames, NO ADTS/LATM
    /// headers, no in-band AudioSpecificConfig) — the verified wire format for
    /// `WAVE_FORMAT_AAC_MS` (0xA106). ~11x smaller than PCM.
    Aac {
        access_unit: Vec<u8>,
        sample_rate: u32,
        channels: u16,
        pts_ms: i64,
    },
}

/// Source of audio for the RDPSND channel.
///
/// The protocol handler (`audio.rs`'s `RdpsndServerHandler` impl) advertises
/// formats (AAC ahead of PCM, PCM as fallback) and pulls chunks that match the
/// negotiated format. The source is responsible only for capture (and AAC
/// encoding if it advertised AAC); resampling to the client's rate happens
/// here when the capture rate differs.
#[async_trait::async_trait]
pub trait AudioSource: Send {
    /// Await the next audio chunk matching the negotiated format, or `Ok(None)`
    /// if the source ended.
    async fn next_chunk(&mut self) -> anyhow::Result<Option<AudioChunk>>;

    /// Whether this source can produce AAC access units. If true, the protocol
    /// layer advertises `WAVE_FORMAT_AAC_MS` ahead of PCM.
    fn supports_aac(&self) -> bool;

    /// Start / resume capture. Called once RDPSND is negotiated. Idempotent.
    fn start(&self);

    /// Stop / pause capture (client minimized / SuppressOutput).
    fn stop(&self);
}

/// Display geometry the surface and input coordinate space are built from.
#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    pub width: u16,
    pub height: u16,
    pub dpi: f32,
    pub scale_factor: f32,
}

/// A combined set of platform backends for one RDP session.
///
/// On macOS this wraps ScreenCaptureKit / VideoToolbox / AudioToolbox /
/// CGEventPost / NSPasteboard. On anland this wraps the Android `MediaCodec`
/// frame source, `AudioRecord`, native input, and clipboard manager reached
/// over the private anland Unix socket bridge. The RDP server builder is
/// wired with whichever set the target platform selects.
///
/// `Send` (not `Send + Sync`): the backends own single-consumer channels
/// (e.g. `mpsc::Receiver`) that are `Send` but not `Sync`. The RDP server
/// drives them from a dedicated task/thread, so `Sync` is not required.
pub trait PlatformBackends: Send {
    /// The frame source feeding the EGFX ship pipeline. `None` if EGFX AVC420
    /// is not in use (legacy bitmap path). Takes the receiver out of the
    /// backends (`&mut self`) — the source is constructed once.
    fn video_source(&mut self) -> Option<Box<dyn VideoFrameSource + Send>>;
    /// The audio source feeding RDPSND. `None` if audio is disabled.
    fn audio_source(&mut self) -> Option<Box<dyn AudioSource + Send>>;
    /// Display geometry for the surface and input coordinate space.
    fn display_info(&self) -> DisplayInfo;
}
