//! anland / Linux / Android platform backends.
//!
//! These backends source frames/audio/clipboard/input from Android
//! `MediaCodec` / `AudioRecord` / clipboard manager / native input, reached
//! over the private anland Unix-socket bridge ([`crate::anland_bridge`]).
//!
//! ## Current wiring
//!
//! - [`AnlandVideoSource`] is **fully wired** to the bridge: it pulls
//!   already-encoded H.264 Annex-B frames from `AnlandBridgeInbound` and maps
//!   them to [`EncodedVideoFrame`] for the EGFX ship side; `start`/`stop`/
//!   `request_keyframe` forward to the [`AnlandBridge`] control surface.
//! - `AudioSource` returns `None` for now — Android `AudioRecord` + optional
//!   `MediaCodec` AAC is the next phase (it taps audio directly, not over the
//!   video bridge).
//! - Clipboard / input / drive redirection are NOT re-abstracted here; they
//!   already implement upstream ironrdp traits and will be wired against the
//!   Android clipboard manager / native input in the main.rs integration
//!   phase.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, watch};

use crate::anland_bridge::{
    transport::BridgeEndpoint, AnlandBridge, AnlandBridgeInbound,
    wire::VideoFramePayload,
};

use super::{AudioChunk, AudioSource, DisplayInfo, EncodedVideoFrame, PlatformBackends, VideoFrameSource};

/// anland backend set. Holds the bridge handle (for input/clipboard control
/// by the RDP side) and the inbound video receiver + discontinuity flag +
/// clipboard watch (consumed by the EGFX ship side and the CLIPRDR side).
pub struct AnlandBackends {
    /// Cloned handle for the RDP side to drive input/clipboard (the original
    /// is also returned from [`Self::spawn`] so the server entry can hold it).
    bridge: AnlandBridge,
    /// Taken once by `video_source`; `None` after the EGFX ship side is
    /// constructed.
    video_rx: Option<mpsc::Receiver<VideoFramePayload>>,
    /// Discontinuity flag shared with the EGFX ship side (read on reconnect,
    /// queue overflow, surface loss). Held here so a future ship-side getter
    /// can expose it; the trait doesn't surface it yet.
    video_discontinuity: Arc<AtomicBool>,
    /// Latest coalesced clipboard from Android, for the CLIPRDR side.
    clipboard_rx: Option<watch::Receiver<Option<String>>>,
    display: DisplayInfo,
    /// Stream geometry/FPS, passed to `AnlandVideoSource::start`.
    width: u16,
    height: u16,
    fps: u8,
}

impl AnlandBackends {
    /// Spawn the anland bridge and construct the backend set. Returns the
    /// backends plus the bridge handle (the server entry holds the handle to
    /// drive input/clipboard; the backends hold a clone for the video
    /// source's start/stop/idr control).
    pub fn spawn(
        endpoint: &BridgeEndpoint,
        token: &[u8],
        width: u16,
        height: u16,
        fps: u8,
        shutdown: broadcast::Receiver<()>,
    ) -> anyhow::Result<(Self, AnlandBridge)> {
        let (bridge, inbound) = AnlandBridge::spawn(endpoint, token, width, height, fps, shutdown)?;
        let AnlandBridgeInbound {
            video_frames,
            video_discontinuity,
            clipboard,
        } = inbound;
        let backends = Self {
            bridge: bridge.clone(),
            video_rx: Some(video_frames),
            video_discontinuity,
            clipboard_rx: Some(clipboard),
            display: DisplayInfo {
                width,
                height,
                dpi: 96.0,
                scale_factor: 1.0,
            },
            width,
            height,
            fps,
        };
        Ok((backends, bridge))
    }

    /// Take the clipboard watch receiver (for the CLIPRDR side to observe
    /// Android clipboard). `None` after the first take.
    pub fn take_clipboard_rx(&mut self) -> Option<watch::Receiver<Option<String>>> {
        self.clipboard_rx.take()
    }

    /// The shared discontinuity flag, for the EGFX ship side to read on
    /// reconnect / queue overflow / surface loss.
    pub fn video_discontinuity(&self) -> &Arc<AtomicBool> {
        &self.video_discontinuity
    }
}

impl PlatformBackends for AnlandBackends {
    fn video_source(&mut self) -> Option<Box<dyn VideoFrameSource + Send>> {
        let rx = self.video_rx.take()?;
        Some(Box::new(AnlandVideoSource {
            bridge: self.bridge.clone(),
            rx,
            width: self.width,
            height: self.height,
            fps: self.fps,
        }))
    }

    fn audio_source(&mut self) -> Option<Box<dyn AudioSource + Send>> {
        // Audio is tapped directly (Android AudioRecord + optional MediaCodec
        // AAC), not over the video bridge. Wired in the audio phase.
        None
    }

    fn display_info(&self) -> DisplayInfo {
        self.display
    }
}

/// Pulls already-encoded H.264 Annex-B frames from the Android `MediaCodec`
/// surface encoder over the anland bridge and maps each to
/// [`EncodedVideoFrame`] for the EGFX ship pipeline. No encode step — Android
/// has already encoded. `MediaCodec` emits Annex-B natively, the exact framing
/// mstsc's decoder requires (verified by macrdp 2026-05-20), so `data` is
/// passed through unchanged.
pub struct AnlandVideoSource {
    bridge: AnlandBridge,
    rx: mpsc::Receiver<VideoFramePayload>,
    width: u16,
    height: u16,
    fps: u8,
}

#[async_trait::async_trait]
impl VideoFrameSource for AnlandVideoSource {
    async fn next_frame(&mut self) -> anyhow::Result<Option<EncodedVideoFrame>> {
        match self.rx.recv().await {
            Some(f) => Ok(Some(EncodedVideoFrame {
                data: f.nal,
                is_keyframe: f.is_keyframe,
                pts_ms: i64::from(f.timestamp_ms),
                // Empty for anland: MediaCodec emits SPS/PPS in-band in the
                // Annex-B stream (macrdp's VideoToolbox path populates these
                // out-of-band; anland needs no AVCC→Annex-B conversion).
                parameter_sets: Vec::new(),
                display_width: f.visible_width,
                display_height: f.visible_height,
                encoded_width: f.encoded_width,
                encoded_height: f.encoded_height,
            })),
            None => Ok(None),
        }
    }

    fn request_keyframe(&self) {
        self.bridge.request_idr();
    }

    fn start(&self) {
        // Set desired stream state + send STREAM_START now; the bridge session
        // also replays it on reconnect.
        self.bridge.start_stream(self.width, self.height, self.fps);
    }

    fn stop(&self) {
        self.bridge.stop_stream();
    }
}

/// Audio source stub — Android `AudioRecord` (+ optional `MediaCodec` AAC) is
/// wired in the audio phase. Kept here as a typed placeholder so the trait is
/// satisfied once audio is added.
#[allow(dead_code)]
pub struct AnlandAudioSource;

#[async_trait::async_trait]
impl AudioSource for AnlandAudioSource {
    async fn next_chunk(&mut self) -> anyhow::Result<Option<AudioChunk>> {
        anyhow::bail!("anland audio source not wired yet — AudioRecord pending")
    }
    fn supports_aac(&self) -> bool {
        false
    }
    fn start(&self) {}
    fn stop(&self) {}
}
