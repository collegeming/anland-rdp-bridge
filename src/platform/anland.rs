//! anland / Linux / Android platform backends — stubs.
//!
//! anland-rdp-bridge runs on Arch Linux ARM under Droidspaces, sourcing frames
//! from Android `MediaCodec` over a private Unix socket bridge. This module is
//! the home for those backends. It is currently a **stub**: the traits are
//! implemented so the Linux build compiles and the type wiring resolves, but
//! every method returns `Err(NotImplemented)` / `todo!()` until the anland
//! bridge module is wired in (see CLAUDE.md roadmap, phase 4).
//!
//! ## What the real anland backends will do
//!
//! - **VideoFrameSource** — connect to `/run/anland-rdp/bridge.sock`, perform
//!   the HMAC-SHA256 mutual auth handshake, then pull already-encoded H.264
//!   Annex-B frames the Android `MediaCodec` surface encoder produced. No
//!   encode step — unlike the macOS backend, anland ships bytes the encoder
//!   already emitted. `MediaCodec` emits Annex-B natively, matching what
//!   mstsc's decoder requires.
//! - **AudioSource** — capture PCM i16 via Android `AudioRecord`; optionally
//!   encode AAC via `MediaCodec` (raw AAC-LC access units, matching
//!   `WAVE_FORMAT_AAC_MS`). Resample to the client rate if needed.
//! - **ClipboardBackend** — implement the upstream `CliprdrBackend` trait
//!   against the Android clipboard manager + a file provider for image/file
//!   copy. (Lives in `clipboard.rs`, not here — it already implements the
//!   upstream trait; only the data source swaps.)
//! - **InputSink** — implement the upstream `RdpServerInputHandler` trait,
//!   forwarding keyboard/mouse to anland native input over the bridge.

use super::{AudioChunk, AudioSource, DisplayInfo, PlatformBackends, VideoFrameSource};

/// anland backend set — stubs pending bridge wiring.
pub struct AnlandBackends {
    display: DisplayInfo,
}

impl AnlandBackends {
    /// Construct with placeholder display geometry. The real constructor will
    /// read dimensions from the anland bridge config.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            display: DisplayInfo {
                width: 1280,
                height: 720,
                dpi: 96.0,
                scale_factor: 1.0,
            },
        })
    }
}

impl PlatformBackends for AnlandBackends {
    fn video_source(&self) -> Option<Box<dyn VideoFrameSource>> {
        Some(Box::new(AnlandVideoSource))
    }
    fn audio_source(&self) -> Option<Box<dyn AudioSource>> {
        Some(Box::new(AnlandAudioSource))
    }
    fn display_info(&self) -> DisplayInfo {
        self.display
    }
}

/// Pulls already-encoded H.264 Annex-B frames from the Android `MediaCodec`
/// surface encoder over the private anland Unix socket bridge. Stub for now.
pub struct AnlandVideoSource;

#[async_trait::async_trait]
impl VideoFrameSource for AnlandVideoSource {
    async fn next_frame(&mut self) -> anyhow::Result<Option<super::EncodedVideoFrame>> {
        anyhow::bail!("anland video source not wired yet — bridge module pending")
    }
    fn request_keyframe(&self) {
        // TODO: send IDR request over the anland bridge.
    }
    fn start(&self) {
        // TODO: signal the Android encoder to start producing.
    }
    fn stop(&self) {
        // TODO: signal the Android encoder to stop producing.
    }
}

/// Captures audio via Android `AudioRecord` (PCM i16) and optionally encodes
/// AAC via `MediaCodec`. Stub for now.
pub struct AnlandAudioSource;

#[async_trait::async_trait]
impl AudioSource for AnlandAudioSource {
    async fn next_chunk(&mut self) -> anyhow::Result<Option<AudioChunk>> {
        anyhow::bail!("anland audio source not wired yet — AudioRecord pending")
    }
    fn supports_aac(&self) -> bool {
        false // TODO: flip true once MediaCodec AAC backend is wired.
    }
    fn start(&self) {
        // TODO: start AudioRecord capture.
    }
    fn stop(&self) {
        // TODO: stop AudioRecord capture.
    }
}
