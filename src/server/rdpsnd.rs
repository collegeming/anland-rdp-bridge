//! RDPSND audio: forwards Android audio (PCM i16LE, or raw AAC-LC access
//! units) from the anland bridge to `mstsc`.
//!
//! ## Design (mirrors the EGFX video pump)
//!
//! A single [`spawn_audio_pump`] task owns the bridge's audio receiver and
//! forwards each [`AudioWireChunk`] to the *latest* RDPSND audio sender. The
//! per-connection [`AnlandRdpsndBackend`] (built by the factory) writes its
//! dedicated `AudioWave` sender into a shared slot on
//! [`SoundServerFactory::set_audio_sender`], so the pump always feeds the
//! current connection's dispatch task. `start`/`stop` just tell Android to
//! capture via the bridge (`AUDIO_START` / `AUDIO_STOP`); the pump idles when
//! Android is not sending.
//!
//! Formats advertised: PCM (44.1 kHz / stereo / 16-bit) — always available.
//! AAC-LC (`WAVE_FORMAT_AAC_MS`) is structured in (the wire carries
//! `audio_format::AAC_LC`, and `make_wave` tags the AAC packet duration) but
//! not yet advertised — the Android `MediaCodec` AAC encoder backend is a
//! follow-up. `choose_format` prefers AAC when it is present.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError, RdpsndServerHandler};
use ironrdp_server::{AudioWave, SoundServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info};

use crate::anland_bridge::wire::{self, AudioWireChunk};
use crate::anland_bridge::AnlandBridge;

/// PCM capture/playback parameters (44.1 kHz stereo 16-bit — the Windows
/// audio default).
const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;

/// RDPSND factory: builds a per-connection backend and holds the shared
/// audio-sender slot + last-negotiated-format cell the pump reads.
pub struct AnlandRdpsndFactory {
    bridge: AnlandBridge,
    /// Latest RDPSND audio sender, set per connection; the pump reads it.
    latest_audio_sender: Arc<Mutex<Option<mpsc::Sender<AudioWave>>>>,
    /// Last negotiated capture format `(sample_rate, channels, aac)`; the pump
    /// uses it to restart Android capture after display-suppression.
    last_format: Arc<Mutex<Option<(u32, u16, bool)>>>,
}

impl AnlandRdpsndFactory {
    pub fn new(
        bridge: AnlandBridge,
    ) -> (
        Self,
        Arc<Mutex<Option<mpsc::Sender<AudioWave>>>>,
        Arc<Mutex<Option<(u32, u16, bool)>>>,
    ) {
        let latest_audio_sender = Arc::new(Mutex::new(None));
        let last_format = Arc::new(Mutex::new(None));
        let factory = Self {
            bridge,
            latest_audio_sender: Arc::clone(&latest_audio_sender),
            last_format: Arc::clone(&last_format),
        };
        (factory, latest_audio_sender, last_format)
    }
}

impl ServerEventSender for AnlandRdpsndFactory {
    fn set_sender(&mut self, _sender: mpsc::UnboundedSender<ServerEvent>) {
        // The pump ships waves through the dedicated audio sender, not the
        // unified event channel.
    }
}

impl SoundServerFactory for AnlandRdpsndFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(AnlandRdpsndBackend {
            bridge: self.bridge.clone(),
            latest_audio_sender: Arc::clone(&self.latest_audio_sender),
            last_format: Arc::clone(&self.last_format),
        })
    }

    fn set_audio_sender(&mut self, audio_sender: mpsc::Sender<AudioWave>) {
        if let Ok(mut slot) = self.latest_audio_sender.lock() {
            *slot = Some(audio_sender);
        }
    }
}

/// Per-connection RDPSND backend.
pub struct AnlandRdpsndBackend {
    bridge: AnlandBridge,
    latest_audio_sender: Arc<Mutex<Option<mpsc::Sender<AudioWave>>>>,
    last_format: Arc<Mutex<Option<(u32, u16, bool)>>>,
}

impl std::fmt::Debug for AnlandRdpsndBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnlandRdpsndBackend").finish_non_exhaustive()
    }
}

impl RdpsndServerHandler for AnlandRdpsndBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        // Static PCM format. AAC advertised when the Android AAC backend lands.
        static FORMATS: [AudioFormat; 1] = [AudioFormat {
            format: WaveFormat::PCM,
            n_channels: CHANNELS,
            n_samples_per_sec: SAMPLE_RATE,
            n_avg_bytes_per_sec: SAMPLE_RATE * (CHANNELS as u32 * (BITS_PER_SAMPLE as u32 / 8)),
            n_block_align: CHANNELS * (BITS_PER_SAMPLE / 8),
            bits_per_sample: BITS_PER_SAMPLE,
            data: None,
        }];
        &FORMATS
    }

    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        // `common` is in our preference order (AAC ahead of PCM once
        // advertised); take the top client-accepted format.
        common.first()
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        // `format.format()` is the negotiated AudioFormat; the crate stamps
        // its wFormatNo onto every wave. Tell Android whether to capture PCM
        // or AAC, and let the pump forward whatever arrives.
        let af = format.format();
        let use_aac = af.format == WaveFormat::AAC_MS;
        let sample_rate = af.n_samples_per_sec;
        let channels = af.n_channels;
        debug!(
            use_aac,
            sample_rate, channels, "anland rdpsnd: audio streaming starting"
        );
        if let Ok(mut slot) = self.last_format.lock() {
            *slot = Some((sample_rate, channels, use_aac));
        }
        self.bridge.start_audio(sample_rate, channels, use_aac);
        Ok(())
    }

    fn stop(&mut self) {
        debug!("anland rdpsnd: audio streaming stopped");
        if let Ok(mut slot) = self.last_format.lock() {
            *slot = None;
        }
        self.bridge.stop_audio();
    }
}

/// Spawn the audio pump: forward Android audio chunks to the current RDPSND
/// sender. Idles when Android is not capturing; mutes on display suppression
/// (client minimized) by telling Android to stop; ends on shutdown or when the
/// bridge disconnects.
pub fn spawn_audio_pump(
    mut audio_rx: mpsc::Receiver<AudioWireChunk>,
    latest_audio_sender: Arc<Mutex<Option<mpsc::Sender<AudioWave>>>>,
    bridge: AnlandBridge,
    display_suppressed: Arc<AtomicBool>,
    last_format: Arc<Mutex<Option<(u32, u16, bool)>>>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    tokio::spawn(async move {
        let mut poll = tokio::time::interval(Duration::from_millis(500));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut paused = false;
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                _ = poll.tick() => {
                    update_audio_suppression(
                        &bridge, &display_suppressed, &last_format, &mut paused,
                    );
                }
                chunk = audio_rx.recv() => {
                    let Some(chunk) = chunk else { break };
                    update_audio_suppression(
                        &bridge, &display_suppressed, &last_format, &mut paused,
                    );
                    if paused || display_suppressed.load(Ordering::Acquire) {
                        continue; // muted while minimized
                    }
                    let wave = make_wave(&chunk);
                    let sender = match latest_audio_sender.lock() {
                        Ok(g) => g.clone(),
                        Err(_) => continue,
                    };
                    if let Some(sender) = sender {
                        if sender.send(wave).await.is_err() {
                            // RDPSND closed; a reconnect rebuilds the sender.
                            debug!("anland rdpsnd: audio sender closed");
                        }
                    }
                }
            }
        }
        info!("anland rdpsnd: audio pump stopped");
    });
}

/// Stop Android capture after display suppression; restart it (with the last
/// negotiated format) on resume.
fn update_audio_suppression(
    bridge: &AnlandBridge,
    display_suppressed: &AtomicBool,
    last_format: &Mutex<Option<(u32, u16, bool)>>,
    paused: &mut bool,
) {
    if display_suppressed.load(Ordering::Acquire) {
        if !*paused {
            bridge.stop_audio();
            *paused = true;
            debug!("anland rdpsnd: muted (display suppressed)");
        }
        return;
    }
    if *paused {
        *paused = false;
        if let Ok(slot) = last_format.lock() {
            if let Some((sr, ch, aac)) = *slot {
                bridge.start_audio(sr, ch, aac);
                debug!("anland rdpsnd: resumed (display restored)");
            }
        }
    }
}

/// Convert a wire audio chunk to an `AudioWave` for the RDPSND dispatch task.
/// PCM waves carry no duration; AAC access units carry their 1024-frame
/// packet duration so the vendor audio-lag model sizes the client buffer.
fn make_wave(chunk: &AudioWireChunk) -> AudioWave {
    let ts = chunk.timestamp_ms.min(u64::from(u32::MAX)) as u32;
    let duration = if chunk.format == wire::audio_format::AAC_LC {
        // 1024 frames per AAC-LC access unit → ms.
        Some(1024.0 / f64::from(chunk.sample_rate) * 1000.0)
    } else {
        None
    };
    (chunk.data.clone(), ts, duration)
}
