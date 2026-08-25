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
//!   forwards the RDP scancode as the evdev keycode (the full RDP→evdev
//!   scancode table from the inherited `input/scancodes.rs` is wired in a
//!   follow-up).
//! - `with_display_handler(FixedDisplay)` — advertises the fixed anland
//!   desktop size; `updates()` never yields because anland uses the EGFX
//!   AVC420 path (no legacy bitmap updates).
//! - `with_gfx_factory`/`with_cliprdr_factory`/`with_sound_factory`/`with_connection_handler`
//!   = `None` for now — EGFX video pump, CLIPRDR, and RDPSND are the
//!   follow-up sub-phases.
//!
//! ## Linux evdev button codes (from the bridge spec)
//!
//! BTN_LEFT=272, BTN_RIGHT=273, BTN_MIDDLE=274, BTN_SIDE=275, BTN_EXTRA=276.

/// anland RDP server module.
pub mod cliprdr;
pub mod gfx;
pub mod rdpsnd;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{Context, Result};
use ironrdp_server::{
    DesktopSize, DisplayUpdate, KeyboardEvent, MouseEvent, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::sync::broadcast;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

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

        // RDPSND audio: bridge audio chunks → the per-connection audio
        // sender; the factory holds the shared slot the pump writes into plus
        // the last negotiated format for mute/resume.
        let audio_rx = backends
            .take_audio_rx()
            .context("backends audio_rx already taken")?;
        let (rdpsnd_factory, latest_audio_sender, audio_last_format) =
            AnlandRdpsndFactory::new(bridge.clone());

        let display = FixedDisplay {
            width: config.width,
            height: config.height,
        };
        // Clone the bridge for the video/audio pumps before moving it into
        // the input handler (which forwards keyboard/mouse through it).
        let bridge_for_pump = bridge.clone();
        let bridge_for_audio = bridge.clone();
        let input = AnlandInputHandler { bridge };

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
                config.width,
                config.height,
                shutdown.subscribe(),
            );
        } else {
            warn!("anland RDP server: no video source from backends; EGFX pump not started");
        }

        // Spawn the audio pump: forwards Android audio chunks to the RDPSND
        // sender for the current connection; mutes on display suppression.
        spawn_audio_pump(
            audio_rx,
            latest_audio_sender,
            bridge_for_audio,
            display_suppressed.clone(),
            audio_last_format,
            shutdown.subscribe(),
        );

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

/// Fixed anland desktop size. anland streams EGFX AVC420; the legacy bitmap
/// `updates()` path is unused, so it never yields.
struct FixedDisplay {
    width: u16,
    height: u16,
}

struct PendingDisplayUpdates;

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for PendingDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for FixedDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: self.width,
            height: self.height,
        }
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(PendingDisplayUpdates))
    }
}

/// Forwards `mstsc` keyboard/mouse to the Android input sink over the bridge.
/// Mouse is fully mapped; keyboard forwards the RDP scancode as the evdev
/// keycode (the full RDP→evdev scancode table is a follow-up).
struct AnlandInputHandler {
    bridge: AnlandBridge,
}

impl RdpServerInputHandler for AnlandInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        // action: 0 = down, 1 = up (bridge KEY payload).
        let (action, code) = match event {
            KeyboardEvent::Pressed { code, .. } => (0u8, u32::from(code)),
            KeyboardEvent::Released { code, .. } => (1u8, u32::from(code)),
            // Unicode + Synchronize are not key events on the wire; the full
            // unicode/scancode mapping is wired with input/scancodes.rs later.
            KeyboardEvent::UnicodePressed(_) | KeyboardEvent::UnicodeReleased(_)
            | KeyboardEvent::Synchronize(_) => return,
        };
        // TODO: map RDP scancode → Linux evdev keycode via the inherited
        // input/scancodes.rs table. For now forward the scancode as-is.
        self.bridge.send_key(action, code);
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
