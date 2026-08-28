//! anland RDP server: wires the anland bridge + platform backends into the
//! upstream IronRDP `RdpServer`.
//!
//! This module is the integration point. It spawns the [`AnlandBackends`]
//! (which spawns the hardened Unix-socket bridge), builds a TLS acceptor from
//! the configured cert/key, and constructs an `RdpServer` with:
//!
//! - `with_tls` — TLS-encrypted RDP (no NLA; the loopback default needs no
//!   credentials; an external bind will switch to `with_hybrid` + credentials
//!   in a follow-up).
//! - `with_input_handler(AnlandInputHandler)` — forwards keyboard/mouse from
//!   `mstsc` to the Android input sink via the bridge. Mouse is mapped
//!   (absolute move, button press/release, vertical scroll); keyboard
//!   translates the RDP Set 1 scancode (+ extended flag) to the Linux evdev
//!   keycode via [`evdev_scancodes`] so modifier keys (Win/Meta, right
//!   Ctrl/Alt) and navigation keys reach niri correctly — Win+T fires niri's
//!   Mod+T instead of a raw scancode.
//! - `with_display_handler(AnlandDisplay)` — advertises the current anland
//!   desktop size (config initial, then the client-resized live size) and
//!   implements MS-RDPEDISP `request_layout` (dynamic resolution): a client
//!   window resize re-sends `STREAM_START` at the new size over the bridge
//!   (the Android consumer rebuilds its encoder/niri mode) and emits a
//!   `DisplayUpdate::Resize` so the vendored server runs the core
//!   Deactivation-Reactivation at the new size. `updates()` only yields on a
//!   resize — anland uses the EGFX AVC420 path, no legacy bitmap updates.
//! - `with_gfx_factory`/`with_cliprdr_factory`/`with_sound_factory`/`with_connection_handler`
//!   = `None` for now — EGFX video pump, CLIPRDR, and RDPSND are the
//!   follow-up sub-phases.
//!
//! ## Linux evdev button codes (from the bridge spec)
//!
//! BTN_LEFT=272, BTN_RIGHT=273, BTN_MIDDLE=274, BTN_SIDE=275, BTN_EXTRA=276.

/// anland RDP server module.
pub mod cliprdr;
mod evdev_scancodes;
pub mod gfx;
pub mod rdpsnd;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_pdu::input::fast_path::SynchronizeFlags;
use ironrdp_server::{
    DesktopSize, DisplayUpdate, KeyboardEvent, MouseEvent, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::sync::{broadcast, watch};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::anland_bridge::{transport::BridgeEndpoint, AnlandBridge};
use crate::platform::{AnlandBackends, PlatformBackends};
use crate::server::cliprdr::AnlandCliprdrFactory;
use crate::server::gfx::{spawn_video_pump, AnlandGfxFactory};
use crate::server::rdpsnd::{spawn_audio_pump, AnlandRdpsndFactory};

/// Linux evdev button codes carried on the bridge `MOUSE_BUTTON` payload.
mod evdev {
    pub const BTN_LEFT: i32 = 272;
    pub const BTN_RIGHT: i32 = 273;
    pub const BTN_MIDDLE: i32 = 274;
    pub const BTN_SIDE: i32 = 275;
    pub const BTN_EXTRA: i32 = 276;
}

/// Minimal server config (the full config module is wired in main.rs).
pub struct AnlandServerConfig {
    pub listen_addr: SocketAddr,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub bridge_endpoint: BridgeEndpoint,
    /// 16-byte bridge auth token (32 hex chars decoded).
    pub bridge_token: Vec<u8>,
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    /// Upper bound a client-requested MS-RDPEDISP live resize is clamped to
    /// (the initial ANLAND_WIDTH/HEIGHT are the connect-time size, not a pin).
    pub max_width: u16,
    pub max_height: u16,
}

pub struct AnlandRdpServer {
    rdp_server: RdpServer,
    shutdown: broadcast::Sender<()>,
}

impl AnlandRdpServer {
    pub fn new(config: &AnlandServerConfig) -> Result<Self> {
        let (shutdown, _) = broadcast::channel(8);

        // Spawn the bridge backends. Returns the backends (held here for the
        // display/video/clipboard surfaces) plus the bridge handle (the input
        // handler forwards keyboard/mouse through it).
        let (mut backends, bridge) = AnlandBackends::spawn(
            &config.bridge_endpoint,
            &config.bridge_token,
            config.width,
            config.height,
            config.fps,
            shutdown.subscribe(),
        )
        .context("failed to spawn anland bridge")?;

        let display_suppressed = Arc::new(AtomicBool::new(false));

        // TLS acceptor from the configured cert/key. No client auth (the
        // loopback default has no NLA); an external bind will switch to
        // `with_hybrid` + credentials in a follow-up.
        let tls_acceptor = build_tls_acceptor(&config.cert_path, &config.key_path)?;

        // EGFX AVC420 video pump: factory (per-connection handler + server
        // handle) + the platform video source (already-encoded MediaCodec
        // frames over the bridge). The factory holds a bridge clone for
        // start/stop; latest_handle + state are shared with the pump.
        let (gfx_factory, latest_handle, gfx_state) = AnlandGfxFactory::new(bridge.clone());
        let video_source = backends.video_source();

        // CLIPRDR text + image clipboard: bidirectional CF_UNICODETEXT /
        // CF_DIB between the Android clipboard (bridge watches) and mstsc.
        let clipboard_rx = backends
            .take_clipboard_rx()
            .context("backends clipboard_rx already taken")?;
        let clipboard_image_rx = backends
            .take_clipboard_image_rx()
            .context("backends clipboard_image_rx already taken")?;
        let file_list_rx = backends
            .take_file_list_rx()
            .context("backends file_list_rx already taken")?;
        let file_content_rx = backends
            .take_file_content_rx()
            .context("backends file_content_rx already taken")?;
        let cliprdr_factory = AnlandCliprdrFactory::new(
            bridge.clone(),
            clipboard_rx,
            clipboard_image_rx,
            file_list_rx,
            file_content_rx,
        );

        // RDPSND audio: the Linux desktop capture backend (from the platform
        // backends) feeds the per-connection audio sender; the factory holds
        // the shared slot the pump writes into.
        let audio_source = backends.audio_source();
        let (rdpsnd_factory, latest_audio_sender) = AnlandRdpsndFactory::new();

        // Display: shared live geometry + MS-RDPEDISP resize handling. The
        // state is shared with the EGFX video pump so a client resize resets
        // the graphics pipeline at the new size.
        let display_state = Arc::new(AnlandDisplayState::new(config.width, config.height));
        let display = AnlandDisplay {
            state: Arc::clone(&display_state),
            bridge: bridge.clone(),
            fps: config.fps,
            max_width: config.max_width,
            max_height: config.max_height,
        };
        // Clone the bridge for the video pump before moving it into the input
        // handler (which forwards keyboard/mouse through it).
        let bridge_for_pump = bridge.clone();
        let input = AnlandInputHandler {
            bridge,
            lock_state: Default::default(),
        };

        let mut rdp_server = RdpServer::builder()
            .with_addr(config.listen_addr)
            .with_tls(tls_acceptor)
            .with_input_handler(input)
            .with_display_handler(display)
            .with_cliprdr_factory(Some(Box::new(cliprdr_factory)))
            .with_sound_factory(Some(Box::new(rdpsnd_factory)))
            .with_rdpdr_factory(None)
            .with_gfx_factory(Some(Box::new(gfx_factory)))
            .with_usb_factory(None)
            .with_camera_factory(None)
            .with_connection_handler(None)
            .build();

        // Share the SuppressOutput flag with the bridge so a minimized mstsc
        // pauses the Android encoder via the video pump's suppression path.
        rdp_server.set_display_suppressed_handle(Arc::clone(&display_suppressed));
        rdp_server.set_honor_client_desktop_size(false);

        // Spawn the video pump: pulls encoded frames from the platform source
        // and ships them over EGFX AVC420. It also consumes the bridge's
        // video-discontinuity flag (Android reconnect / bad frame) to recover
        // with a fresh keyframe.
        if let Some(source) = video_source {
            let bridge_discontinuity = backends.video_discontinuity().clone();
            spawn_video_pump(
                source,
                latest_handle,
                gfx_state,
                rdp_server.event_sender().clone(),
                bridge_for_pump,
                display_suppressed.clone(),
                bridge_discontinuity,
                display_state,
                shutdown.subscribe(),
            );
        } else {
            warn!("anland RDP server: no video source from backends; EGFX pump not started");
        }

        // Spawn the audio pump: forwards Linux desktop audio chunks to the
        // RDPSND sender for the current connection; mutes on display
        // suppression. Audio is optional — the capture backend may be absent
        // until the PipeWire sink-monitor source is wired.
        if let Some(audio_source) = audio_source {
            spawn_audio_pump(
                audio_source,
                latest_audio_sender,
                display_suppressed.clone(),
                shutdown.subscribe(),
            );
        } else {
            warn!("anland RDP server: no audio source from backends; RDPSND pump not started");
        }

        info!(addr = %config.listen_addr, "anland RDP server initialized");
        // Keep the backends alive for the server's lifetime (the bridge task
        // is already spawned; this just holds the inbound channels + handle).
        std::mem::forget(backends);

        Ok(Self {
            rdp_server,
            shutdown,
        })
    }

    #[must_use]
    pub fn shutdown_sender(&self) -> broadcast::Sender<()> {
        self.shutdown.clone()
    }

    pub async fn run(&mut self) -> Result<()> {
        let result = Box::pin(self.rdp_server.run()).await;
        let _ = self.shutdown.send(());
        result
    }
}

/// Build a `TlsAcceptor` from PEM cert + key files.
fn build_tls_acceptor(cert_path: &std::path::Path, key_path: &std::path::Path) -> Result<TlsAcceptor> {
    let cert_pem = std::fs::read(cert_path)
        .with_context(|| format!("read cert {}", cert_path.display()))?;
    let key_pem = std::fs::read(key_path)
        .with_context(|| format!("read key {}", key_path.display()))?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("parse cert PEM: {e}"))?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| anyhow::anyhow!("parse key PEM: {e}"))?
        .context("no private key in PEM")?;
    let server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("build TLS server config: {e}"))?;
    Ok(TlsAcceptor::from(Arc::new(server_cfg)))
}

/// Shared live-session display state: the current desktop geometry + the
/// resize bookkeeping (revision bump + reactivation-echo suppression latch),
/// guarded by a single mutex, plus a pending-resize `watch` the display
/// `updates()` stream drains to drive the vendored server's
/// Deactivation-Reactivation at the new size.
pub(crate) struct AnlandDisplayState {
    inner: Mutex<DisplayStateInner>,
    /// Latest accepted resize, coalesced. The `updates()` stream drains it
    /// into a `DisplayUpdate::Resize` → the vendored server re-runs the core
    /// Deactivation-Reactivation with the new `DesktopSize`.
    pending_resize: watch::Sender<Option<DesktopSize>>,
}

struct DisplayStateInner {
    /// Current live geometry (the config size until the first client resize).
    size: DesktopSize,
    /// Bumped once per accepted client resize; the EGFX video pump polls this
    /// to reset the graphics pipeline at the new size.
    revision: u64,
    /// Set by `request_layout` right before it drives a Deactivation-Reactivation;
    /// consumed by the next `request_initial_size` so that a reactivation WE
    /// triggered ourselves doesn't re-adopt the client's stale capset echo (which
    /// would immediately undo the resize we just applied — see the macOS
    /// `CaptureDisplay::suppress_next_adopt` rationale).
    suppress_next_initial_adopt: bool,
}

impl AnlandDisplayState {
    fn new(width: u16, height: u16) -> Self {
        let (pending_resize, _) = watch::channel(None);
        Self {
            inner: Mutex::new(DisplayStateInner {
                size: DesktopSize { width, height },
                revision: 0,
                suppress_next_initial_adopt: false,
            }),
            pending_resize,
        }
    }

    /// Current live geometry (config size until the first client resize).
    fn size(&self) -> DesktopSize {
        self.inner.lock().expect("display state lock poisoned").size
    }

    /// Monotonic resize counter; the EGFX pump compares against its last
    /// seen value to detect a change.
    fn revision(&self) -> u64 {
        self.inner.lock().expect("display state lock poisoned").revision
    }

    /// Adopt a new geometry and bump the revision (one state transition for
    /// the size + counter pair, so the pump's compare can never tear them).
    fn apply_resize(&self, width: u16, height: u16) {
        let mut g = self.inner.lock().expect("display state lock poisoned");
        g.size = DesktopSize { width, height };
        g.revision += 1;
    }

    /// True exactly once after `arm_reactivation_echo_suppress`.
    fn take_reactivation_echo_suppress(&self) -> bool {
        std::mem::take(
            &mut self
                .inner
                .lock()
                .expect("display state lock poisoned")
                .suppress_next_initial_adopt,
        )
    }

    fn arm_reactivation_echo_suppress(&self) {
        self.inner
            .lock()
            .expect("display state lock poisoned")
            .suppress_next_initial_adopt = true;
    }
}

/// anland display handler. Serves the current live geometry (the config size
/// until a client resize) and implements MS-RDPEDISP `request_layout`: a
/// client window resize mid-session is clamped, pushed to the Android encoder
/// as a new `STREAM_START` over the bridge (the consumer rebuilds its route
/// at the new size — `startRemoteStream` → `DisplaySession.startStream` →
/// ANiri `adapt_to_size` re-modes the virtual display), queued for the core
/// Deactivation-Reactivation via `updates()`, and signalled to the EGFX pump
/// through [`AnlandDisplayState::revision`] so it resets the graphics
/// pipeline at the new size.
struct AnlandDisplay {
    state: Arc<AnlandDisplayState>,
    bridge: AnlandBridge,
    fps: u8,
    max_width: u16,
    max_height: u16,
}

/// Yields a `DisplayUpdate::Resize` exactly once per accepted client resize.
/// The vendored server polls this for the whole session (a `None` ends the
/// connection), so it waits between resizes rather than ever completing.
struct AnlandDisplayUpdates {
    pending_resize: watch::Receiver<Option<DesktopSize>>,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for AnlandDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        loop {
            // Drain the latest coalesced resize, then wait for the next.
            if self.pending_resize.has_changed().unwrap_or(false) {
                if let Some(size) = *self.pending_resize.borrow_and_update() {
                    return Ok(Some(DisplayUpdate::Resize(size)));
                }
            }
            if self.pending_resize.changed().await.is_err() {
                return Ok(None); // state dropped — end the session
            }
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for AnlandDisplay {
    async fn size(&mut self) -> DesktopSize {
        self.state.size()
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(AnlandDisplayUpdates {
            pending_resize: self.state.pending_resize.subscribe(),
        }))
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        // This fires once at connect (mstsc fullscreen/maximize sends its
        // desktop width/height in Confirm Active) AND again on every
        // Deactivation-Reactivation — including one WE just triggered for a
        // mid-session `request_layout` resize. During that self-triggered
        // reactivation the client echoes a stale capset size (often its
        // original connect-time size), so blindly re-adopting it would undo
        // the resize. Skip adoption exactly once when `request_layout` armed
        // this reactivation.
        if self.state.take_reactivation_echo_suppress() {
            let cur = self.state.size();
            debug!(
                width = cur.width,
                height = cur.height,
                "anland display: self-triggered reactivation — keeping server-driven size"
            );
            return cur;
        }
        self.adopt_client_size(client_size)
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        // Pick the primary monitor's requested dimensions (same shape as the
        // macOS CaptureDisplay path).
        let Some(monitor) = layout
            .monitors()
            .iter()
            .find(|m| m.is_primary())
            .or_else(|| layout.monitors().first())
        else {
            debug!("anland display: client sent a monitor layout with no monitors — ignoring");
            return;
        };
        let (w, h) = monitor.dimensions();
        let (Ok(width), Ok(height)) = (u16::try_from(w), u16::try_from(h)) else {
            warn!(w, h, "anland display: client-requested monitor size out of range — ignoring");
            return;
        };
        let (width, height) = self.clamp(width, height);

        let cur = self.state.size();
        if (width, height) == (cur.width, cur.height) {
            return; // unchanged — no churn
        }

        self.apply_resize(width, height);
        // The resize drives a Deactivation-Reactivation; suppress the echo so
        // `request_initial_size` doesn't re-adopt the stale client size.
        self.state.arm_reactivation_echo_suppress();
        // Queue the Deactivation-Reactivation so mstsc re-negotiates the
        // desktop size — what real RDS servers send on an MS-RDPEDISP
        // resize (a bare EGFX surface swap without it was visually broken).
        let _ = self
            .state
            .pending_resize
            .send(Some(DesktopSize { width, height }));
        info!(
            width,
            height,
            "anland display: client resize accepted — reconfiguring encoder + graphics pipeline"
        );
    }
}

impl AnlandDisplay {
    /// Clamp a client-requested size to the protocol-legal / config band. The
    /// Android encoder rejects <64px surfaces; niri modes top out at
    /// 4096x2160 by default (`ANLAND_MAX_WIDTH`/`ANLAND_MAX_HEIGHT`).
    fn clamp(&self, width: u16, height: u16) -> (u16, u16) {
        const MIN_DIM: u16 = 320;
        (width.clamp(MIN_DIM, self.max_width), height.clamp(MIN_DIM, self.max_height))
    }

    /// Publish a new geometry and reconfigure the Android encoder at that size
    /// (shared by the connect-time initial size and the mid-session resize).
    /// `STREAM_START` is the bridge's resize signal — the consumer rebuilds
    /// its route, and ANiri `adapt_to_size` re-modes the virtual display.
    fn apply_resize(&self, width: u16, height: u16) {
        self.state.apply_resize(width, height);
        self.bridge.start_stream(width, height, self.fps);
    }

    /// Adopt the client's connect-time desktop size (clamped), returning the
    /// negotiated size. mstsc reports its window/fullscreen size here; serving
    /// it directly means the session starts at the client's resolution without
    /// a resize round-trip (the lamco 0.2.3 fix).
    fn adopt_client_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        let (width, height) = self.clamp(client_size.width, client_size.height);
        let cur = self.state.size();
        if (width, height) == (cur.width, cur.height) {
            return cur;
        }
        info!(
            client_w = client_size.width,
            client_h = client_size.height,
            width,
            height,
            "anland display: serving client-requested desktop resolution"
        );
        self.apply_resize(width, height);
        DesktopSize { width, height }
    }
}

/// Forwards `mstsc` keyboard/mouse to the Android input sink over the bridge.
/// Mouse is fully mapped; keyboard translates the RDP scancode (+ extended
/// flag) to the evdev keycode via [`evdev_scancodes`], and reconciles the
/// remote lock state (CapsLock/NumLock/ScrollLock) from the client's
/// `Synchronize` event by toggling only the lock keys that differ.
struct AnlandInputHandler {
    bridge: AnlandBridge,
    /// Lock state as tracked on the remote session (see [`evdev_scancodes::LockState`]).
    lock_state: evdev_scancodes::LockState,
}

impl RdpServerInputHandler for AnlandInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Synchronize(flags) => self.sync_lock_state(flags),
            KeyboardEvent::Pressed { code, extended } => {
                // Track lock-key presses: each forwarded lock-key press toggles
                // the remote xkb state, so the tracked state must flip in lock
                // step or a later Synchronize would double-toggle.
                self.note_lock_key(code);
                let Some(keycode) = evdev_scancodes::scancode_to_evdev(code, extended) else {
                    debug!(
                        scancode = format!("0x{code:02X}"),
                        extended,
                        "anland input: unmapped scancode dropped"
                    );
                    return;
                };
                self.bridge.send_key(0, keycode);
            }
            KeyboardEvent::Released { code, extended } => {
                let Some(keycode) = evdev_scancodes::scancode_to_evdev(code, extended) else {
                    return;
                };
                self.bridge.send_key(1, keycode);
            }
            // Unicode text input is dropped (RDP defaults to scancodes; the
            // remote xkb layout governs the resulting characters).
            KeyboardEvent::UnicodePressed(_) | KeyboardEvent::UnicodeReleased(_) => {}
        }
    }


    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => {
                // Absolute position in display coords; no relative delta.
                self.bridge
                    .send_mouse_motion(0, f32::from(x), f32::from(y), 0.0, 0.0);
            }
            MouseEvent::LeftPressed => self.bridge.send_mouse_button(evdev::BTN_LEFT, 1),
            MouseEvent::LeftReleased => self.bridge.send_mouse_button(evdev::BTN_LEFT, 0),
            MouseEvent::RightPressed => self.bridge.send_mouse_button(evdev::BTN_RIGHT, 1),
            MouseEvent::RightReleased => self.bridge.send_mouse_button(evdev::BTN_RIGHT, 0),
            MouseEvent::MiddlePressed => self.bridge.send_mouse_button(evdev::BTN_MIDDLE, 1),
            MouseEvent::MiddleReleased => self.bridge.send_mouse_button(evdev::BTN_MIDDLE, 0),
            MouseEvent::Button4Pressed => self.bridge.send_mouse_button(evdev::BTN_SIDE, 1),
            MouseEvent::Button4Released => self.bridge.send_mouse_button(evdev::BTN_SIDE, 0),
            MouseEvent::Button5Pressed => self.bridge.send_mouse_button(evdev::BTN_EXTRA, 1),
            MouseEvent::Button5Released => self.bridge.send_mouse_button(evdev::BTN_EXTRA, 0),
            MouseEvent::VerticalScroll { value } => {
                self.bridge.send_mouse_axis(0.0, f32::from(value));
            }
            MouseEvent::Scroll { x, y } => {
                self.bridge.send_mouse_axis(x as f32, y as f32);
            }
            MouseEvent::RelMove { x, y } => {
                self.bridge.send_mouse_motion(0, 0.0, 0.0, x as f32, y as f32);
            }
        }
    }
}

impl AnlandInputHandler {
    /// Reconcile the remote lock state to the client's `Synchronize` report by
    /// toggling (press+release) each lock key whose state differs. Idempotent:
    /// an in-sync report produces no churn.
    fn sync_lock_state(&mut self, flags: SynchronizeFlags) {
        let toggles = evdev_scancodes::lock_toggles(
            flags.contains(SynchronizeFlags::NUM_LOCK),
            flags.contains(SynchronizeFlags::CAPS_LOCK),
            flags.contains(SynchronizeFlags::SCROLL_LOCK),
            &mut self.lock_state,
        );
        for keycode in toggles {
            debug!(keycode, "anland input: toggling remote lock key");
            self.bridge.send_key(0, keycode);
            self.bridge.send_key(1, keycode);
        }
    }

    /// Flip the tracked lock state when a lock key is pressed (it toggles the
    /// remote session, so the server-side mirror must follow).
    fn note_lock_key(&mut self, scancode: u8) {
        match scancode {
            0x3A => self.lock_state.caps_lock = !self.lock_state.caps_lock, // CapsLock
            0x45 => self.lock_state.num_lock = !self.lock_state.num_lock, // NumLock
            0x46 => self.lock_state.scroll_lock = !self.lock_state.scroll_lock, // ScrollLock
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn display_state_reports_initial_then_updated_geometry() {
        let state = AnlandDisplayState::new(1280, 720);
        assert_eq!(
            (state.size().width, state.size().height),
            (1280, 720),
            "initial geometry is the config size"
        );
        state.set_size(2560, 1600);
        assert_eq!(
            (state.size().width, state.size().height),
            (2560, 1600),
            "set_size updates the live geometry"
        );
        assert_eq!(state.revision(), 0, "set_size is not a client resize");
    }

    /// The updates stream must yield exactly one `Resize` per accepted request,
    /// then wait for the next — a stream that re-yielded the same request would
    /// put the vendored server into an infinite Deactivation-Reactivation loop.
    #[tokio::test]
    async fn updates_stream_yields_resize_exactly_once_then_waits() {
        let state = AnlandDisplayState::new(1280, 720);
        let mut updates = AnlandDisplayUpdates {
            pending_resize: state.pending_resize.subscribe(),
        };

        let _ = state
            .pending_resize
            .send(Some(DesktopSize { width: 1920, height: 1080 }));
        let update = updates.next_update().await.expect("next_update failed");
        match update {
            Some(DisplayUpdate::Resize(sz)) => {
                assert_eq!((sz.width, sz.height), (1920, 1080));
            }
            other => panic!("expected a resize update, got {other:?}"),
        }

        // No new request → next_update must stay pending, not re-yield the
        // stale 1920x1080 (which would loop the reactivation forever).
        let timed = tokio::time::timeout(Duration::from_millis(100), updates.next_update());
        assert!(
            timed.await.is_err(),
            "stream re-yielded a resize without a new request"
        );
    }
}
