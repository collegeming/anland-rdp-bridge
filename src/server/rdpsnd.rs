#![allow(dead_code)]
//! RDPSND audio: forwards the **Linux desktop** audio (niri/anland playback,
//! captured on the Linux side via the platform [`AudioSource`]) to `mstsc`.
//!
//! ## Architecture (corrected)
//!
//! The shared desktop is the Arch/niri Linux session; its audio is produced by
//! Linux apps and played through the Android speaker via the anland audio
//! socket (see the consumer's `anland_audio.c` — a PipeWire sink-monitor
//! capture of desktop playback). The RDP client must hear that same Linux
//! audio, so the capture point is **on the Linux side**, not Android and not
//! over the bridge. [`AnlandAudioSource`] is the Linux capture backend
//! (PipeWire sink-monitor, mirroring `anland_audio.c`).
//!
//! A single [`spawn_audio_pump`] task owns the [`AudioSource`] and forwards
//! each [`AudioChunk`] to the *latest* RDPSND audio sender. The per-connection
//! [`AnlandRdpsndBackend`] (built by the factory) writes its dedicated
//! `AudioWave` sender into a shared slot on
//! [`SoundServerFactory::set_audio_sender`], so the pump always feeds the
//! current connection's dispatch task.
//!
//! Formats advertised: PCM (44.1 kHz / stereo / 16-bit) — always available.
//! AAC-LC (`WAVE_FORMAT_AAC_MS`) is structured in (the source may yield
//! `AudioChunk::Aac` and `make_wave` tags the packet duration) but not yet
//! advertised — the Linux AAC encoder is a follow-up.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError, RdpsndServerHandler};
use ironrdp_server::{AudioWave, SoundServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info};

use crate::platform::{AudioChunk, AudioSource};

/// PCM capture/playback parameters (44.1 kHz stereo 16-bit — the Windows
/// audio default).
const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;

/// RDPSND factory: builds a per-connection backend and holds the shared
/// audio-sender slot the pump writes into.
pub struct AnlandRdpsndFactory {
    /// Latest RDPSND audio sender, set per connection; the pump reads it.
    latest_audio_sender: Arc<Mutex<Option<mpsc::Sender<AudioWave>>>>,
}

impl AnlandRdpsndFactory {
    pub fn new() -> (Self, Arc<Mutex<Option<mpsc::Sender<AudioWave>>>>) {
        let latest_audio_sender = Arc::new(Mutex::new(None));
        let factory = Self {
            latest_audio_sender: Arc::clone(&latest_audio_sender),
        };
        (factory, latest_audio_sender)
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
            latest_audio_sender: Arc::clone(&self.latest_audio_sender),
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
    latest_audio_sender: Arc<Mutex<Option<mpsc::Sender<AudioWave>>>>,
}

impl std::fmt::Debug for AnlandRdpsndBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnlandRdpsndBackend").finish_non_exhaustive()
    }
}

impl RdpsndServerHandler for AnlandRdpsndBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        // Static PCM format. AAC advertised when the Linux AAC encoder lands.
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
        // The pump pulls from the Linux AudioSource and the crate stamps each
        // wave with the negotiated wFormatNo. Nothing to start here — the
        // source is driven by the pump (started on negotiation / resume).
        let af = format.format();
        debug!(
            use_aac = af.format == WaveFormat::AAC_MS,
            sample_rate = af.n_samples_per_sec,
            channels = af.n_channels,
            "anland rdpsnd: audio streaming starting"
        );
        Ok(())
    }

    fn stop(&mut self) {
        debug!("anland rdpsnd: audio streaming stopped");
    }
}

/// Spawn the audio pump: forward Linux desktop audio chunks from the platform
/// [`AudioSource`] to the current RDPSND sender. Mutes on display suppression
/// (client minimized) by stopping the source; ends on shutdown.
pub fn spawn_audio_pump(
    mut audio_source: Box<dyn AudioSource + Send>,
    latest_audio_sender: Arc<Mutex<Option<mpsc::Sender<AudioWave>>>>,
    display_suppressed: Arc<AtomicBool>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    tokio::spawn(async move {
        // Kick the capture shim once; the source is idempotent so later
        // suppression/resume is a no-op (the shim keeps buffering and drops
        // oldest on overflow — cheapest possible semantics for an RDP pipe).
        audio_source.start();

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                chunk = audio_source.next_chunk() => {
                    let chunk = match chunk {
                        Ok(Some(c)) => c,
                        Ok(None) => break,
                        Err(e) => {
                            debug!("anland rdpsnd: audio source ended: {e}");
                            break;
                        }
                    };

                    // Suppression (client minimized) just mutes the outbound
                    // channel — the source stays warm, niri/PipeWire keep
                    // capturing, and the first frame after restore ships
                    // immediately.
                    if display_suppressed.load(Ordering::Acquire) {
                        continue;
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

/// Convert an [`AudioChunk`] to an `AudioWave` for the RDPSND dispatch task.
/// PCM waves carry no duration; AAC access units carry their 1024-frame
/// packet duration so the vendor audio-lag model sizes the client buffer.
fn make_wave(chunk: &AudioChunk) -> AudioWave {
    match chunk {
        AudioChunk::Pcm {
            samples,
            sample_rate: _,
            channels: _,
            pts_ms,
        } => (samples.clone(), (*pts_ms).min(i64::from(u32::MAX)) as u32, None),
        AudioChunk::Aac {
            access_unit,
            sample_rate,
            channels: _,
            pts_ms,
        } => (
            access_unit.clone(),
            (*pts_ms).min(i64::from(u32::MAX)) as u32,
            // 1024 frames per AAC-LC access unit → ms.
            Some(1024.0 / f64::from(*sample_rate) * 1000.0),
        ),
    }
}
