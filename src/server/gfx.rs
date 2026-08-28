#![allow(dead_code)]
//! EGFX AVC420 video pump: pulls already-encoded H.264 frames from the anland
//! bridge (via the platform [`VideoFrameSource`]) and ships them to `mstsc`
//! over the RDP EGFX graphics pipeline.
//!
//! anland does not encode here — Android `MediaCodec` has already produced
//! Annex-B NALs. This module only manages the EGFX surface lifecycle and
//! pumps each [`EncodedVideoFrame`] through
//! [`GraphicsPipelineServer::send_avc420_frame`], framing the resulting DVC
//! output onto the wire via [`ServerEvent::Egfx`].
//!
//! Reimplemented from the wire/protocol (no BSL source carried over). The
//! state machine mirrors the bridge spec's discontinuity rules: reconnect,
//! queue overflow, EGFX output failure, capability re-advertise, and surface
//! loss all clear the prediction chain, request an IDR, and reject P-frames
//! until a fresh keyframe ships.

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ironrdp_dvc::encode_dvc_messages;
use ironrdp_egfx::pdu::{
    Avc420Region, CapabilitiesAdvertisePdu, CapabilitiesV10Flags, CapabilitiesV107Flags,
    CapabilitiesV81Flags, CapabilitiesV8Flags, CapabilitySet,
};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer, Surface};
use ironrdp_server::{
    EgfxServerMessage, GfxDvcBridge, GfxServerFactory, GfxServerHandle, ServerEvent, ServerEventSender,
};
use ironrdp_svc::ChannelFlags;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use crate::anland_bridge::AnlandBridge;
use crate::platform::{EncodedVideoFrame, VideoFrameSource};
use crate::server::AnlandDisplayState;

/// At most three EGFX frames in flight; frame ACKs provide backpressure.
const MAX_FRAMES_IN_FLIGHT: u32 = 3;
/// Poll cadence for the reconcile/suppression check between frames.
const VIDEO_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// After this long suppressed, stop the Android encoder.
const SUPPRESSION_PAUSE_AFTER: Duration = Duration::from_secs(2);

/// Shared EGFX negotiation/surface state, read by both the handler callbacks
/// (protocol side) and the video pump (capture side).
pub(crate) struct GraphicsState {
    ready: AtomicBool,
    supports_avc420: AtomicBool,
    has_surface: AtomicBool,
    surface_id: AtomicU16,
    needs_full_reinit: AtomicBool,
}

impl GraphicsState {
    fn new() -> Self {
        Self {
            ready: AtomicBool::new(false),
            supports_avc420: AtomicBool::new(false),
            has_surface: AtomicBool::new(false),
            surface_id: AtomicU16::new(0),
            needs_full_reinit: AtomicBool::new(false),
        }
    }

    fn reset(&self) {
        self.ready.store(false, Ordering::Release);
        self.supports_avc420.store(false, Ordering::Release);
        self.has_surface.store(false, Ordering::Release);
        self.surface_id.store(0, Ordering::Release);
        self.needs_full_reinit.store(false, Ordering::Release);
    }
}

/// EGFX callback handler: tracks negotiation + surface state and drives the
/// Android encoder (start on AVC420-ready, stop on close/suppression).
pub struct AnlandGraphicsHandler {
    bridge: AnlandBridge,
    state: Arc<GraphicsState>,
}

impl GraphicsPipelineHandler for AnlandGraphicsHandler {
    fn capabilities_advertise(&mut self, _pdu: &CapabilitiesAdvertisePdu) {
        // mstsc re-advertised caps mid-session: schedule a full re-init
        // (surface loss + IDR) so the prediction chain resets cleanly.
        if self.state.ready.load(Ordering::Acquire) {
            self.state.has_surface.store(false, Ordering::Release);
            self.state.surface_id.store(0, Ordering::Release);
            self.state.needs_full_reinit.store(true, Ordering::Release);
            warn!("anland gfx: mstsc re-advertised EGFX caps; scheduling decoder reinit");
        }
    }

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        // AVC420 is enabled iff the V8.1 capset carries AVC420_ENABLED; V8 has
        // no AVC; all V10+ capsets support AVC420.
        let supports_avc420 = match negotiated {
            CapabilitySet::V8_1 { flags, .. } => flags.contains(CapabilitiesV81Flags::AVC420_ENABLED),
            CapabilitySet::V8 { .. } => false,
            _ => true,
        };
        self.state.supports_avc420.store(supports_avc420, Ordering::Release);
        self.state.ready.store(true, Ordering::Release);
        if supports_avc420 {
            self.bridge.request_idr();
            info!(?negotiated, "anland gfx: EGFX AVC420 ready");
        } else {
            warn!(?negotiated, "anland gfx: client did not negotiate AVC420");
        }
    }

    fn on_surface_created(&mut self, surface: &Surface) {
        self.state.surface_id.store(surface.id, Ordering::Release);
        self.state.has_surface.store(true, Ordering::Release);
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        if self.state.surface_id.load(Ordering::Acquire) == surface_id {
            self.state.has_surface.store(false, Ordering::Release);
            self.state.surface_id.store(0, Ordering::Release);
            self.state.needs_full_reinit.store(true, Ordering::Release);
        }
    }

    fn on_close(&mut self) {
        // Do NOT call bridge.stop_stream() here: that clears the persistent
        // `desired_stream`, and the video pump (which re-sends STREAM_START on
        // resume) would then leave the Android encoder stopped forever after
        // reconnect. Stream start/stop is the suppression path's job; a closed
        // EGFX channel just means this RDP connection stops receiving frames.
        self.state.reset();
        info!("anland gfx: EGFX channel closed");
    }

    fn preferred_capabilities(&self) -> Vec<CapabilitySet> {
        // V10.x FIRST, in the upstream IronRDP order. This is load-bearing for
        // mstsc: V8.1 requires the client to explicitly set AVC420_ENABLED
        // (0x10), which mstsc does not send (it sends 0x0) — advertising only
        // V8.1 (the lamco approach) makes the negotiation land on V8.1 and
        // mstsc fails AVC420 → no video. V10.x has INVERTED semantics: 0x0
        // (AVC_DISABLED unset) means AVC is ENABLED, and mstsc's 0x0 then
        // negotiates AVC420 correctly. xrdp / FreeRDP shadow advertise V10 for
        // the same reason. V8.1 stays as the fallback for clients that speak
        // only V8.1 and DO set AVC420_ENABLED.
        vec![
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE,
            },
            CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::SMALL_CACHE,
            },
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
            },
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::SMALL_CACHE,
            },
        ]
    }
}

/// Factory the RDP server builds per connection. Produces the handler for
/// callbacks and the `(GfxDvcBridge, GfxServerHandle)` pair the video pump
/// drives directly.
pub struct AnlandGfxFactory {
    bridge: AnlandBridge,
    state: Arc<GraphicsState>,
    /// Latest server handle, set by `build_server_with_handle` and read by the
    /// video pump to discover the current connection's handle.
    latest_handle: Arc<Mutex<Option<GfxServerHandle>>>,
}

impl AnlandGfxFactory {
    pub fn new(bridge: AnlandBridge) -> (Self, Arc<Mutex<Option<GfxServerHandle>>>, Arc<GraphicsState>) {
        let state = Arc::new(GraphicsState::new());
        let latest_handle = Arc::new(Mutex::new(None));
        let factory = Self {
            bridge,
            state: Arc::clone(&state),
            latest_handle: Arc::clone(&latest_handle),
        };
        (factory, latest_handle, state)
    }
}

impl ServerEventSender for AnlandGfxFactory {
    fn set_sender(&mut self, _sender: mpsc::UnboundedSender<ServerEvent>) {
        // The video pump holds the sender directly; the factory doesn't emit.
    }
}

impl GfxServerFactory for AnlandGfxFactory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(AnlandGraphicsHandler {
            bridge: self.bridge.clone(),
            state: Arc::clone(&self.state),
        })
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        self.state.reset();
        let server = Arc::new(Mutex::new(GraphicsPipelineServer::new(Box::new(
            AnlandGraphicsHandler {
                bridge: self.bridge.clone(),
                state: Arc::clone(&self.state),
            },
        ))));
        if let Ok(mut latest) = self.latest_handle.lock() {
            *latest = Some(Arc::clone(&server));
        } else {
            warn!("anland gfx: latest-handle lock poisoned");
            return None;
        }
        Some((GfxDvcBridge::new(Arc::clone(&server)), server))
    }
}

/// Spawn the video pump: pull encoded frames from `source` and ship them over
/// EGFX. Runs until the source ends, the RDP event channel closes, or shutdown.
#[allow(clippy::too_many_arguments)]
pub fn spawn_video_pump(
    mut source: Box<dyn VideoFrameSource + Send>,
    latest_handle: Arc<Mutex<Option<GfxServerHandle>>>,
    state: Arc<GraphicsState>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    bridge: AnlandBridge,
    display_suppressed: Arc<AtomicBool>,
    // Set by the bridge on Android reconnect / bad frame — the pump treats it
    // as a discontinuity (clear the prediction chain, request an IDR, drop
    // P-frames until a fresh keyframe).
    bridge_discontinuity: Arc<AtomicBool>,
    // Shared display geometry + revision counter: a client-requested
    // MS-RDPEDISP resize bumps the revision; the pump resets the graphics
    // pipeline at the new size (see `check_resize`).
    display_state: Arc<AnlandDisplayState>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) {
    tokio::spawn(async move {
        let mut last_revision = display_state.revision();
        let mut current_handle: Option<GfxServerHandle> = None;
        let mut surface_id: Option<u16> = None;
        let mut awaiting_idr = true;
        let mut suppression_started: Option<Instant> = None;
        let mut stream_paused = false;
        let mut poll = tokio::time::interval(VIDEO_POLL_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                _ = poll.tick() => {
                    update_suppression(
                        &source, &bridge, &state, &display_suppressed,
                        &mut suppression_started, &mut stream_paused, &mut awaiting_idr,
                    );
                    if bridge_discontinuity.swap(false, Ordering::AcqRel) {
                        enter_discontinuity(&bridge, &mut awaiting_idr);
                    }
                    if let Err(e) = check_resize(
                        &display_state, &mut last_revision, current_handle.as_ref(),
                        &mut surface_id, &mut awaiting_idr, &bridge, &event_tx,
                    ) {
                        debug!("anland gfx: resize failed: {e}");
                    }
                }
                frame = source.next_frame() => {
                    let frame = match frame {
                        Ok(Some(f)) => f,
                        Ok(None) => break,
                        Err(e) => {
                            warn!("anland gfx: source error: {e}");
                            break;
                        }
                    };

                    update_suppression(
                        &source, &bridge, &state, &display_suppressed,
                        &mut suppression_started, &mut stream_paused, &mut awaiting_idr,
                    );
                    if bridge_discontinuity.swap(false, Ordering::AcqRel) {
                        enter_discontinuity(&bridge, &mut awaiting_idr);
                    }
                    if stream_paused || display_suppressed.load(Ordering::Acquire) {
                        continue;
                    }

                    // Reconcile to the latest connection's handle.
                    if let Ok(latest) = latest_handle.lock() {
                        let changed = match (&*latest, current_handle.as_ref()) {
                            (Some(l), Some(c)) => !Arc::ptr_eq(l, c),
                            (Some(_), None) | (None, Some(_)) => true,
                            (None, None) => false,
                        };
                        if changed {
                            current_handle = latest.clone();
                            surface_id = None;
                            awaiting_idr = true;
                        }
                    }

                    // A client-requested resize (revision bumped by
                    // `AnlandDisplay::request_layout`): reset the EGFX
                    // graphics pipeline at the new size (DeleteSurface old +
                    // ResetGraphics), then recreate the surface below.
                    if let Err(e) = check_resize(
                        &display_state, &mut last_revision, current_handle.as_ref(),
                        &mut surface_id, &mut awaiting_idr, &bridge, &event_tx,
                    ) {
                        debug!("anland gfx: resize failed: {e}");
                    }

                    if state.needs_full_reinit.swap(false, Ordering::AcqRel) {
                        surface_id = None;
                        enter_discontinuity(&bridge, &mut awaiting_idr);
                    }

                    // Create the surface if EGFX is ready + AVC420 + no surface,
                    // at the current (possibly client-resized) geometry.
                    if surface_id.is_none()
                        && state.ready.load(Ordering::Acquire)
                        && state.supports_avc420.load(Ordering::Acquire)
                    {
                        if let Some(handle) = current_handle.as_ref() {
                            let geom = display_state.size();
                            match create_surface(
                                handle, geom.width, geom.height,
                                align_16(geom.width), align_16(geom.height), &event_tx,
                            ) {
                                Ok(id) => {
                                    surface_id = Some(id);
                                    awaiting_idr = true;
                                    bridge.request_idr();
                                    info!(surface_id = id, w = geom.width, h = geom.height, "anland gfx: surface initialized");
                                }
                                Err(e) => debug!("anland gfx: surface not ready: {e}"),
                            }
                        }
                    }

                    // Drop P-frames until a fresh keyframe after discontinuity.
                    if awaiting_idr && !frame.is_keyframe {
                        continue;
                    }

                    let (Some(handle), Some(sid)) = (current_handle.as_ref(), surface_id) else {
                        continue;
                    };
                    match send_video_frame(handle, sid, &frame, &event_tx) {
                        Ok(()) => { if frame.is_keyframe { awaiting_idr = false; } }
                        Err(e) => {
                            debug!("anland gfx: frame dropped: {e}");
                            enter_discontinuity(&bridge, &mut awaiting_idr);
                        }
                    }
                }
            }
        }
        info!("anland gfx: video pump stopped");
    });
}

/// Update the stream-suppression state from `display_suppressed` and drive the
/// Android encoder start/stop accordingly. Restarting is done via
/// [`VideoFrameSource::start`] — `bridge.stop_stream()` clears the persisted
/// `desired_stream`, so a resume must re-send STREAM_START, and only the source
/// holds the width/height/fps needed to do it.
#[allow(clippy::fn_params_excessive_bools)]
fn update_suppression(
    source: &Box<dyn VideoFrameSource + Send>,
    bridge: &AnlandBridge,
    state: &GraphicsState,
    display_suppressed: &Arc<AtomicBool>,
    suppression_started: &mut Option<Instant>,
    stream_paused: &mut bool,
    awaiting_idr: &mut bool,
) {
    if display_suppressed.load(Ordering::Acquire) {
        if *stream_paused {
            return;
        }
        let started = suppression_started.get_or_insert_with(Instant::now);
        if started.elapsed() >= SUPPRESSION_PAUSE_AFTER {
            bridge.stop_stream();
            *stream_paused = true;
            *awaiting_idr = true;
            info!("anland gfx: display suppressed; stopped Android encoder");
        }
        return;
    }
    let was_suppressed = suppression_started.take().is_some();
    if *stream_paused {
        *stream_paused = false;
        *awaiting_idr = true;
        if state.ready.load(Ordering::Acquire) && state.supports_avc420.load(Ordering::Acquire) {
            // `bridge.stop_stream()` cleared `desired_stream`, so a bare IDR
            // would leave the Android encoder stopped forever — the source's
            // `start()` re-sends STREAM_START at its own geometry/fps.
            source.start();
            info!("anland gfx: display resumed; restarted Android encoder + requested IDR");
        }
    } else if was_suppressed {
        *awaiting_idr = true;
        bridge.request_idr();
    }
}

/// Create + map the EGFX surface and flush the resulting DVC output.
fn create_surface(
    handle: &GfxServerHandle,
    display_width: u16,
    display_height: u16,
    encoded_width: u16,
    encoded_height: u16,
    event_tx: &mpsc::UnboundedSender<ServerEvent>,
) -> Result<u16> {
    let (surface_id, channel_id, output) = {
        let mut server = handle
            .lock()
            .map_err(|_| anyhow::anyhow!("EGFX server lock poisoned"))?;
        if !server.is_ready() || !server.supports_avc420() {
            anyhow::bail!("EGFX AVC420 negotiation incomplete");
        }
        let channel_id = server.channel_id().context("EGFX channel has no ID")?;
        server.set_output_dimensions(display_width, display_height);
        let surface_id = server
            .create_surface(encoded_width, encoded_height)
            .context("EGFX surface creation failed")?;
        if !server.map_surface_to_output(surface_id, 0, 0) {
            anyhow::bail!("EGFX surface mapping failed");
        }
        (surface_id, channel_id, server.drain_output())
    };
    send_dvc_output(event_tx, channel_id, output)?;
    Ok(surface_id)
}

/// Send one encoded frame over EGFX AVC420 and flush the DVC output.
fn send_video_frame(
    handle: &GfxServerHandle,
    surface_id: u16,
    frame: &EncodedVideoFrame,
    event_tx: &mpsc::UnboundedSender<ServerEvent>,
) -> Result<()> {
    let (frame_id, channel_id, output) = {
        let mut server = handle
            .lock()
            .map_err(|_| anyhow::anyhow!("EGFX server lock poisoned"))?;
        let channel_id = server.channel_id().context("EGFX channel has no ID")?;
        let regions = [Avc420Region::full_frame(
            frame.display_width,
            frame.display_height,
            22,
        )];
        let frame_id = server
            .send_avc420_frame(surface_id, &frame.data, &regions, frame.pts_ms as u32)
            .context("EGFX backpressure or unavailable surface")?;
        (frame_id, channel_id, server.drain_output())
    };
    send_dvc_output(event_tx, channel_id, output)?;
    trace!(frame_id, bytes = frame.data.len(), "anland gfx: sent AVC420 frame");
    Ok(())
}

/// Encode + ship a batch of DVC messages via the RDP event channel.
fn send_dvc_output(
    event_tx: &mpsc::UnboundedSender<ServerEvent>,
    channel_id: u32,
    output: Vec<ironrdp_dvc::DvcMessage>,
) -> Result<()> {
    if output.is_empty() {
        return Ok(());
    }
    let messages = encode_dvc_messages(channel_id, output, ChannelFlags::SHOW_PROTOCOL)
        .context("failed to encode EGFX DVC output")?;
    event_tx
        .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages }))
        .map_err(|_| anyhow::anyhow!("RDP server event channel closed"))?;
    Ok(())
}

fn enter_discontinuity(bridge: &AnlandBridge, awaiting_idr: &mut bool) {
    *awaiting_idr = true;
    bridge.request_idr();
}

/// Detect a client-requested live resize (MS-RDPEDISP) by comparing the
/// display state's revision counter, and reset the EGFX graphics pipeline at
/// the new size: `resize_with_monitors` (DeleteSurface of the old surfaces +
/// RDPGFX_RESET_GRAPHICS at the new dimensions) so mstsc drops the stale-size
/// surface, then clear the local surface state — the surface-recreate path
/// rebuilds at the new geometry and the IDR request forces a fresh keyframe.
///
/// The core Deactivation-Reactivation itself is driven independently by the
/// `DisplayUpdate::Resize` the display handler emits (see `AnlandDisplay`).
/// Idempotent: only acts once per accepted resize.
#[allow(clippy::too_many_arguments)]
fn check_resize(
    display_state: &AnlandDisplayState,
    last_revision: &mut u64,
    current_handle: Option<&GfxServerHandle>,
    surface_id: &mut Option<u16>,
    awaiting_idr: &mut bool,
    bridge: &AnlandBridge,
    event_tx: &mpsc::UnboundedSender<ServerEvent>,
) -> Result<()> {
    let revision = display_state.revision();
    if revision == *last_revision {
        return Ok(());
    }
    *last_revision = revision;
    let (w, h) = {
        let s = display_state.size();
        (s.width, s.height)
    };

    if let Some(handle) = current_handle {
        let (channel_id, output) = {
            let mut server = handle
                .lock()
                .map_err(|_| anyhow::anyhow!("EGFX server lock poisoned"))?;
            if !server.is_ready() {
                // EGFX not negotiated yet; the surface-recreate path below
                // will build at the new size once it is.
                return Ok(());
            }
            let channel_id = server.channel_id().context("EGFX channel has no ID")?;
            server.resize_with_monitors(w, h, Vec::new());
            (channel_id, server.drain_output())
        };
        send_dvc_output(event_tx, channel_id, output)?;
    }

    *surface_id = None;
    *awaiting_idr = true;
    bridge.request_idr();
    info!(
        w,
        h,
        "anland gfx: client resize accepted — graphics pipeline reset at new size"
    );
    Ok(())
}

fn align_16(value: u16) -> u16 {
    value.saturating_add(15) & !15
}
