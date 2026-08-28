//! anland / Linux / Android platform backends.
//!
//! These backends source frames/audio/clipboard/input from Android
//! `MediaCodec` / PipeWire / clipboard manager / native input, reached
//! over the private anland Unix-socket bridge ([`crate::anland_bridge`]) and
//! the local PipeWire sound service.
//!
//! ## Current wiring
//!
//! - [`AnlandVideoSource`] is **fully wired** to the bridge: it pulls
//!   already-encoded H.264 Annex-B frames from `AnlandBridgeInbound` and maps
//!   them to [`EncodedVideoFrame`] for the EGFX ship side; `start`/`stop`/
//!   `request_keyframe` forward to the [`AnlandBridge`] control surface.
//! - [`AnlandAudioSource`] is **fully wired** to PipeWire via the C capture
//!   shim (`native/anland_rdp_audio.c`, compiled by `build.rs`). It owns a
//!   virtual `Audio/Sink` ("anland-rdp-speaker") and drains S16LE PCM at the
//!   RDPSND-advertised 44.1 kHz / stereo so waves ship unchanged. No AAC path
//!   yet.
//! - Clipboard / input / drive redirection are NOT re-abstracted here; they
//!   already implement upstream ironrdp traits and will be wired against the
//!   Android clipboard manager / native input in the main.rs integration
//!   phase.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, watch};

use crate::anland_bridge::{
    transport::BridgeEndpoint, wire::VideoFramePayload, AnlandBridge, AnlandBridgeInbound,
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
    /// Latest coalesced clipboard image (PNG bytes), for the CLIPRDR side.
    clipboard_image_rx: Option<watch::Receiver<Option<Vec<u8>>>>,
    /// Latest clipboard file list, for the CLIPRDR file path.
    file_list_rx: Option<watch::Receiver<Option<Vec<crate::anland_bridge::wire::FileEntry>>>>,
    /// File content responses, for the CLIPRDR file path.
    file_content_rx: Option<mpsc::Receiver<crate::anland_bridge::wire::FileContentResponse>>,
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
            clipboard_image,
            file_list,
            file_content_rx,
        } = inbound;
        let backends = Self {
            bridge: bridge.clone(),
            video_rx: Some(video_frames),
            video_discontinuity,
            clipboard_rx: Some(clipboard),
            clipboard_image_rx: Some(clipboard_image),
            file_list_rx: Some(file_list),
            file_content_rx: Some(file_content_rx),
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

    /// Take the clipboard image watch receiver. `None` after the first take.
    pub fn take_clipboard_image_rx(&mut self) -> Option<watch::Receiver<Option<Vec<u8>>>> {
        self.clipboard_image_rx.take()
    }

    /// Take the file-list watch receiver (CLIPRDR file path).
    pub fn take_file_list_rx(
        &mut self,
    ) -> Option<watch::Receiver<Option<Vec<crate::anland_bridge::wire::FileEntry>>>> {
        self.file_list_rx.take()
    }

    /// Take the file-content response receiver (CLIPRDR file path).
    pub fn take_file_content_rx(
        &mut self,
    ) -> Option<mpsc::Receiver<crate::anland_bridge::wire::FileContentResponse>> {
        self.file_content_rx.take()
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
        Some(Box::new(AnlandAudioSource::new()))
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

/// PipeWire desktop-audio source. Drains S16LE PCM (44.1 kHz / stereo) from
/// the C capture shim (`native/anland_rdp_audio.c`) that owns the virtual
/// "anland-rdp-speaker" sink, and emits [`AudioChunk::Pcm`] waves for the
/// RDPSND pump. The rate matches the RDPSND-advertised format so no
/// resampling is needed on the Rust side (PipeWire resamples from the hardware
/// rate internally).
///
/// The C engine is process-global and idempotent to start; `stop`/`start`
/// don't tear it down (the RDPSND pump already gates shipping on its `paused`
/// flag, and rebuilding the PipeWire thread loop on every minimize/resume
/// would cost ~1s of dead air). The capture ring keeps draining and drops
/// oldest bytes on overflow while muted — harmless.
pub struct AnlandAudioSource {
    started: AtomicBool,
    /// Monotonic PTS counter (ms), advanced by each shipped chunk's duration.
    pts_ms: std::sync::Mutex<i64>,
    origin: Instant,
}

extern "C" {
    fn anland_rdp_audio_start() -> std::os::raw::c_int;
    fn anland_rdp_audio_pull(
        buf: *mut std::ffi::c_void,
        max_bytes: u32,
        rate: *mut u32,
        channels: *mut u32,
    ) -> std::os::raw::c_int;
    fn anland_rdp_audio_stop();
}

/// 20 ms of 44.1 kHz stereo S16 — the target pull size. Smaller than the
/// ~1s ring so a backed-up pump drops old audio, not current; larger than a
/// single PipeWire period so wave overhead stays low.
const PULL_BYTES: usize = 44_100 * 2 * 2 * 20 / 1000;

impl AnlandAudioSource {
    pub fn new() -> Self {
        Self {
            started: AtomicBool::new(false),
            pts_ms: std::sync::Mutex::new(0),
            origin: Instant::now(),
        }
    }
}

#[async_trait::async_trait]
impl AudioSource for AnlandAudioSource {
    async fn next_chunk(&mut self) -> anyhow::Result<Option<AudioChunk>> {
        let mut buf = vec![0u8; PULL_BYTES];
        loop {
            let mut rate: u32 = 0;
            let mut channels: u32 = 0;
            // SAFETY: the C shim reads `buf`/`max_bytes` and writes up to
            // `max_bytes` into `buf`; the out-params are valid pointers. Safe
            // to call from any thread once started (mutex-protected inside).
            let n = unsafe {
                anland_rdp_audio_pull(
                    buf.as_mut_ptr() as *mut std::ffi::c_void,
                    PULL_BYTES as u32,
                    &mut rate,
                    &mut channels,
                )
            };
            if n < 0 {
                anyhow::bail!("anland audio: PipeWire shim not started");
            }
            if n > 0 {
                buf.truncate(n as usize);
                // Advance the monotonic PTS by this chunk's playback duration
                // so waves are paced correctly even when the pull cadence
                // drifts. ms = bytes / (rate * channels * 2) * 1000.
                let dur_ms = if rate > 0 && channels > 0 {
                    (n as i64) * 1000 / i64::from(rate) / i64::from(channels) / 2
                } else {
                    0
                };
                let pts = {
                    let mut p = self.pts_ms.lock().expect("pts lock poisoned");
                    let cur = *p;
                    *p = cur.saturating_add(dur_ms);
                    cur
                };
                // Prefer the shim's negotiated format; fall back to the
                // advertised defaults if PipeWire hadn't reported yet.
                let sample_rate = if rate > 0 { rate } else { 44_100 };
                let ch = if channels > 0 {
                    channels as u16
                } else {
                    2
                };
                return Ok(Some(AudioChunk::Pcm {
                    samples: buf,
                    sample_rate,
                    channels: ch,
                    pts_ms: pts,
                }));
            }
            // Ring empty — back off briefly so the EGFX/video pump (which
            // shares the select) isn't starved. Cancellable by the pump's
            // shutdown select arm.
            tokio::time::sleep(Duration::from_millis(4)).await;
            let _ = self.origin; // keep the field meaningful for future wall-clock pts
        }
    }

    fn supports_aac(&self) -> bool {
        false
    }

    fn start(&self) {
        if !self.started.swap(true, Ordering::SeqCst) {
            // SAFETY: no-op if already started; first call bootstraps the
            // PipeWire thread loop + virtual sink.
            let rc = unsafe { anland_rdp_audio_start() };
            if rc != 0 {
                // Clear so a later start() retries; the pump will keep pulling
                // and bail on the shim's "not started" error.
                self.started.store(false, Ordering::SeqCst);
                tracing::warn!("anland audio: PipeWire shim start failed (rc={rc})");
            }
        }
    }

    fn stop(&self) {
        // Intentionally a no-op on the C engine: the RDPSND pump already
        // mutes shipping via its `paused` flag, and tearing down/rebuilding
        // the PipeWire thread loop on every SuppressOutput would cost ~1s of
        // dead air on resume. The capture ring keeps draining and drops oldest
        // bytes on overflow while muted — harmless and resume is instant.
    }
}

impl Drop for AnlandAudioSource {
    fn drop(&mut self) {
        if self.started.swap(false, Ordering::SeqCst) {
            // SAFETY: idempotent teardown; joins the PipeWire thread loop.
            unsafe { anland_rdp_audio_stop() };
        }
    }
}
