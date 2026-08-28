// OutboundCmd variants / bridge fields / methods that are part of the wire
// contract or reserved for future wiring (ClipboardAck, is_connected,
// ack_clipboard) — not dead code, just not yet called on the anland-only path.
#![allow(dead_code)]

//! Anland bridge orchestrator.
//!
//! Ties together the hardened [`transport`] listener, the HMAC [`auth`]
//! handshake, and the [`wire`] framing into a single session loop. Produces
//! inbound channels the RDP server consumes (already-encoded H.264 frames,
//! coalesced clipboard state) and exposes outbound control the RDP/EGFX side
//! drives (stream start/stop, IDR requests, input, clipboard).
//!
//! ## Session loop
//!
//! Rust listens; Android reconnects. One client at a time: on disconnect the
//! loop drains the inbound video queue, clears the connected flag, and returns
//! to `accept()`. Per accepted client:
//!
//! 1. Run the 4-step HMAC handshake (5 s per step). On any failure the socket
//!    is closed and the loop accepts a replacement.
//! 2. Mark connected only after `AUTH_OK`. No input, clipboard, stream, IDR,
//!    or video is processed before that.
//! 3. Reconnect replay order (deterministic): Android sends its current
//!    clipboard snapshot as the first application frame → Rust validates it,
//!    ACKs, applies it → Rust replays the latest still-unacknowledged `mstsc`
//!    update (if any) → Rust replays the desired `STREAM_START`/`STREAM_STOP`.
//! 4. Mark session connected; forward current input/video/clipboard traffic
//!    until the client disconnects.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

use super::auth;
use super::transport::{BridgeEndpoint, BridgeListener};
use super::wire::{self, msg, ClipboardUpdate, VideoFramePayload};

/// Inbound side of the bridge: channels the RDP server consumes.
pub struct AnlandBridgeInbound {
    /// Already-encoded H.264 AVC420 frames from Android `MediaCodec`.
    pub video_frames: mpsc::Receiver<VideoFramePayload>,
    /// Set when the EGFX pipeline detects a discontinuity (reconnect,
    /// queue overflow, surface loss, capability re-advertise) so the ship
    /// side clears its prediction chain and waits for a fresh keyframe.
    pub video_discontinuity: Arc<AtomicBool>,
    /// Latest coalesced clipboard text from Android (watch channel: a slow
    /// RDP consumer sees the newest state, not a stale backlog). `None`
    /// means cleared.
    pub clipboard: watch::Receiver<Option<String>>,
    /// Latest coalesced clipboard image (PNG bytes) from Android. `None`
    /// means cleared / text-only.
    pub clipboard_image: watch::Receiver<Option<Vec<u8>>>,
    /// Latest clipboard file list from Android (names + sizes). `None` /
    /// empty means no files on the clipboard.
    pub file_list: watch::Receiver<Option<Vec<wire::FileEntry>>>,
    /// File content responses from Android, correlated by `request_id`.
    pub file_content_rx: mpsc::Receiver<wire::FileContentResponse>,
}

/// Outbound control commands the RDP/EGFX side enqueues toward Android.
#[derive(Debug, Clone)]
pub enum OutboundCmd {
    StreamStart { width: u16, height: u16, fps: u8 },
    StreamStop,
    IdrRequest,
    Key { action: u8, keycode: u32 },
    MouseMotion {
        stream_id: u32,
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
    },
    MouseButton { button: i32, pressed: u8 },
    MouseAxis { dx: f32, dy: f32 },
    /// Forward an `mstsc`-origin clipboard update to Android.
    ClipboardUpdate { sequence: u64, text: String },
    /// Forward an `mstsc`-origin clipboard image (PNG bytes) to Android.
    ClipboardImage { sequence: u64, png: Vec<u8> },
    /// Acknowledge an Android clipboard update.
    ClipboardAck { sequence: u64 },
    /// Ask Android to return `length` bytes at `offset` of clipboard file
    /// entry `index` (CLIPRDR FileContentsRequest RANGE).
    FileContentRequest {
        request_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
}

/// A handle the RDP server uses to drive the bridge (outbound control) and
/// observe session liveness.
#[derive(Clone)]
pub struct AnlandBridge {
    outbound: mpsc::UnboundedSender<OutboundCmd>,
    connected: Arc<AtomicBool>,
    /// Desired stream state, replayed on every reconnect.
    desired_stream: Arc<Mutex<Option<(u16, u16, u8)>>>,
    /// Latest still-unacknowledged `mstsc` clipboard update, replayed on
    /// reconnect.
    pending_clipboard: Arc<Mutex<Option<(u64, String)>>>,
}

impl AnlandBridge {
    /// Spawn the bridge: bind the hardened listener and run the accept +
    /// session loop on a background task. Returns the outbound handle plus
    /// the inbound channels.
    pub fn spawn(
        endpoint: &BridgeEndpoint,
        token: &[u8],
        width: u16,
        height: u16,
        fps: u8,
        shutdown: broadcast::Receiver<()>,
    ) -> Result<(Self, AnlandBridgeInbound)> {
        let (video_tx, video_rx) = mpsc::channel::<VideoFramePayload>(8);
        let (clipboard_tx, clipboard_rx) = watch::channel(None);
        let (clipboard_image_tx, clipboard_image_rx) = watch::channel(None);
        let (file_list_tx, file_list_rx) = watch::channel(None);
        let (file_content_tx, file_content_rx) = mpsc::channel::<wire::FileContentResponse>(8);
        let video_discontinuity = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        let desired_stream = Arc::new(Mutex::new(Some((width, height, fps))));
        let pending_clipboard = Arc::new(Mutex::new(None));
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<OutboundCmd>();

        let bridge = Self {
            outbound: outbound_tx,
            connected: Arc::clone(&connected),
            desired_stream: Arc::clone(&desired_stream),
            pending_clipboard: Arc::clone(&pending_clipboard),
        };

        let inbound = AnlandBridgeInbound {
            video_frames: video_rx,
            video_discontinuity: Arc::clone(&video_discontinuity),
            clipboard: clipboard_rx,
            clipboard_image: clipboard_image_rx,
            file_list: file_list_rx,
            file_content_rx,
        };

        let session = SessionRunner {
            endpoint: endpoint.clone(),
            token: token.to_vec(),
            video_tx,
            clipboard_tx,
            clipboard_image_tx,
            file_list_tx,
            file_content_tx,
            video_discontinuity: Arc::clone(&video_discontinuity),
            connected: Arc::clone(&connected),
            desired_stream: Arc::clone(&desired_stream),
            pending_clipboard: Arc::clone(&pending_clipboard),
            outbound_rx,
            shutdown,
        };
        tokio::spawn(session.run());

        Ok((bridge, inbound))
    }

    /// `true` while an authenticated Android client is connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub fn start_stream(&self, width: u16, height: u16, fps: u8) {
        if let Ok(mut g) = self.desired_stream.lock() {
            *g = Some((width, height, fps));
        }
        let _ = self.outbound.send(OutboundCmd::StreamStart { width, height, fps });
    }

    pub fn stop_stream(&self) {
        if let Ok(mut g) = self.desired_stream.lock() {
            *g = None;
        }
        let _ = self.outbound.send(OutboundCmd::StreamStop);
    }

    pub fn request_idr(&self) {
        let _ = self.outbound.send(OutboundCmd::IdrRequest);
    }

    /// A new `mstsc`-origin clipboard update: stash it as the pending
    /// unacknowledged update (replayed on reconnect) and forward to Android.
    pub fn send_clipboard(&self, sequence: u64, text: String) {
        if let Ok(mut g) = self.pending_clipboard.lock() {
            *g = Some((sequence, text.clone()));
        }
        let _ = self.outbound.send(OutboundCmd::ClipboardUpdate { sequence, text });
    }

    /// A new `mstsc`-origin clipboard image: forward to Android.
    pub fn send_clipboard_image(&self, sequence: u64, png: Vec<u8>) {
        let _ = self.outbound.send(OutboundCmd::ClipboardImage { sequence, png });
    }

    /// Ask Android for `length` bytes at `offset` of clipboard file entry
    /// `index`. The response arrives on `AnlandBridgeInbound.file_content_rx`
    /// with the same `request_id`.
    pub fn request_file_content(&self, request_id: u32, index: u32, offset: u64, length: u32) {
        let _ = self.outbound.send(OutboundCmd::FileContentRequest {
            request_id,
            index,
            offset,
            length,
        });
    }

    /// Acknowledge an Android-origin update (matching sequence = compatibility
    /// ack for a pending `mstsc` update → drop it).
    pub fn ack_clipboard(&self, sequence: u64) {
        if let Ok(mut g) = self.pending_clipboard.lock() {
            if let Some((s, _)) = &*g {
                if *s == sequence {
                    *g = None;
                }
            }
        }
        let _ = self.outbound.send(OutboundCmd::ClipboardAck { sequence });
    }

    pub fn send_key(&self, action: u8, keycode: u32) {
        let _ = self.outbound.send(OutboundCmd::Key { action, keycode });
    }

    pub fn send_mouse_motion(&self, stream_id: u32, x: f32, y: f32, dx: f32, dy: f32) {
        let _ = self.outbound.send(OutboundCmd::MouseMotion { stream_id, x, y, dx, dy });
    }

    pub fn send_mouse_button(&self, button: i32, pressed: u8) {
        let _ = self.outbound.send(OutboundCmd::MouseButton { button, pressed });
    }

    pub fn send_mouse_axis(&self, dx: f32, dy: f32) {
        let _ = self.outbound.send(OutboundCmd::MouseAxis { dx, dy });
    }
}

/// Owned session-runner state moved into the background task.
struct SessionRunner {
    endpoint: BridgeEndpoint,
    token: Vec<u8>,
    video_tx: mpsc::Sender<VideoFramePayload>,
    clipboard_tx: watch::Sender<Option<String>>,
    clipboard_image_tx: watch::Sender<Option<Vec<u8>>>,
    file_list_tx: watch::Sender<Option<Vec<wire::FileEntry>>>,
    file_content_tx: mpsc::Sender<wire::FileContentResponse>,
    video_discontinuity: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    desired_stream: Arc<Mutex<Option<(u16, u16, u8)>>>,
    pending_clipboard: Arc<Mutex<Option<(u64, String)>>>,
    outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>,
    shutdown: broadcast::Receiver<()>,
}

impl SessionRunner {
    async fn run(mut self) {
        let listener = match self.bind().await {
            Ok(l) => l,
            Err(e) => {
                warn!("anland bridge: bind failed: {e:#}");
                return;
            }
        };
        info!("anland bridge: listener ready");

        loop {
            let mut shutdown = self.shutdown.resubscribe();
            tokio::select! {
                _ = shutdown.recv() => break,
                accept = listener.accept() => {
                    let Ok(stream) = accept else { continue };
                    self.serve_one(stream).await;
                }
            }
        }
    }

    async fn bind(&self) -> Result<BridgeListener> {
        match &self.endpoint {
            BridgeEndpoint::Unix(p) => BridgeListener::bind_unix(p).await,
            BridgeEndpoint::Tcp(_addr) => {
                // TCP loopback compatibility: TODO wire a tokio TcpListener
                // with the same accept/handshake loop. Unix is the supported
                // path; TCP is opt-in for shared-loopback deployments.
                anyhow::bail!("anland bridge: tcp endpoint not wired yet")
            }
        }
    }

    /// Serve one authenticated client until it disconnects, then reset for
    /// the next accept.
    async fn serve_one(&mut self, mut stream: tokio::net::UnixStream) {
        // 1. Handshake. On failure, drop the client and accept a replacement.
        if let Err(e) = auth::run_server_handshake(&mut stream, &self.token).await {
            warn!("anland bridge: handshake failed: {e:#}");
            return;
        }
        // 2. Mark connected only after AUTH_OK.
        self.connected.store(true, Ordering::Release);
        self.video_discontinuity.store(true, Ordering::Release);
        info!("anland bridge: client authenticated");

        // 3. Reconnect replay: desired stream state. Extract the value from
        // the (non-Send) MutexGuard and drop it BEFORE the await so the
        // spawned future stays Send.
        let replay = match self.desired_stream.lock() {
            Ok(g) => match *g {
                Some((w, h, fps)) => Some((msg::STREAM_START, wire::encode_stream_start(w, h, fps))),
                None => Some((msg::STREAM_STOP, Vec::new())),
            },
            Err(_) => None,
        };
        if let Some((t, p)) = replay {
            let _ = wire::write_frame(&mut stream, t, &p).await;
        }

        // 3b. Reconnect replay: the latest still-unacknowledged `mstsc`
        // clipboard update (Android may have missed it before disconnecting).
        // Same guard-drops-before-await pattern to stay Send.
        let pending = match self.pending_clipboard.lock() {
            Ok(g) => g.clone(),
            Err(_) => None,
        };
        if let Some((seq, text)) = pending {
            let cu = ClipboardUpdate { sequence: seq, text };
            let _ = wire::write_frame(&mut stream, msg::CLIPBOARD_UPDATE, &cu.encode()).await;
        }

        // 4. Run the read + write loop until disconnect or shutdown. Clone
        // the shared handles into locals so the loop body borrows `self` only
        // via `outbound` (the `&mut self.outbound_rx` field borrow); the
        // dispatch/write paths take the cloned handles, avoiding a whole-`self`
        // borrow that would conflict with that field borrow.
        let video_tx = self.video_tx.clone();
        let clipboard_tx = self.clipboard_tx.clone();
        let clipboard_image_tx = self.clipboard_image_tx.clone();
        let file_list_tx = self.file_list_tx.clone();
        let file_content_tx = self.file_content_tx.clone();
        let video_discontinuity = Arc::clone(&self.video_discontinuity);
        let connected = Arc::clone(&self.connected);
        let pending_clipboard = Arc::clone(&self.pending_clipboard);
        let outbound = &mut self.outbound_rx;
        let mut shutdown = self.shutdown.resubscribe();
        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    break;
                }
                frame = wire::read_frame(&mut stream) => {
                    match frame {
                        Ok(frame) => {
                            if !Self::dispatch_inbound(
                                &mut stream,
                                frame.msg_type,
                                &frame.payload,
                                &video_tx,
                                &clipboard_tx,
                                &clipboard_image_tx,
                                &file_list_tx,
                                &file_content_tx,
                                &video_discontinuity,
                                &pending_clipboard,
                            )
                            .await
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            debug!("anland bridge: inbound read ended: {e}");
                            break;
                        }
                    }
                }
                outbound_cmd = outbound.recv() => {
                    match outbound_cmd {
                        Some(cmd) => {
                            if !Self::write_outbound(&mut stream, cmd).await {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        // Reset for the next client.
        connected.store(false, Ordering::Release);
        self.video_discontinuity.store(true, Ordering::Release);
        info!("anland bridge: client disconnected; returning to accept");
    }

    /// Route one inbound application frame to the right channel. Returns
    /// `false` on a fatal protocol violation (closes the session). Writes any
    /// direct reply (e.g. CLIPBOARD_ACK) back on the same stream — safe
    /// because this runs in the inbound select arm, serialised against the
    /// outbound write arm. An associated function (not a method) so the session
    /// loop can pass cloned handles without a whole-`self` borrow that would
    /// conflict with the `&mut self.outbound_rx` field borrow.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_inbound(
        stream: &mut tokio::net::UnixStream,
        msg_type: u8,
        data: &[u8],
        video_tx: &mpsc::Sender<VideoFramePayload>,
        clipboard_tx: &watch::Sender<Option<String>>,
        clipboard_image_tx: &watch::Sender<Option<Vec<u8>>>,
        file_list_tx: &watch::Sender<Option<Vec<wire::FileEntry>>>,
        file_content_tx: &mpsc::Sender<wire::FileContentResponse>,
        video_discontinuity: &AtomicBool,
        pending_clipboard: &Arc<Mutex<Option<(u64, String)>>>,
    ) -> bool {
        match msg_type {
            msg::VIDEO_FRAME => match VideoFramePayload::decode(data) {
                Ok(frame) => {
                    if video_tx.send(frame).await.is_err() {
                        // RDP side gone; the ship loop will notice.
                        debug!("anland bridge: video channel closed");
                    }
                    true
                }
                Err(e) => {
                    warn!("anland bridge: bad video frame: {e}");
                    video_discontinuity.store(true, Ordering::Release);
                    true
                }
            },
            msg::CLIPBOARD_UPDATE => match ClipboardUpdate::decode(data) {
                Ok(update) => {
                    let _ = clipboard_tx.send(Some(update.text.clone()));
                    if let Err(e) = wire::write_frame(
                        stream,
                        msg::CLIPBOARD_ACK,
                        &update.sequence.to_le_bytes(),
                    )
                    .await
                    {
                        warn!("anland bridge: clipboard ack write failed: {e}");
                        return false;
                    }
                    true
                }
                Err(e) => {
                    warn!("anland bridge: bad clipboard update: {e}");
                    true
                }
            },
            msg::CLIPBOARD_IMAGE => match wire::ClipboardImage::decode(data) {
                Ok(image) => {
                    // Coalesce latest image into the watch; ACK back.
                    let _ = clipboard_image_tx.send(Some(image.png));
                    if let Err(e) = wire::write_frame(
                        stream,
                        msg::CLIPBOARD_ACK,
                        &image.sequence.to_le_bytes(),
                    )
                    .await
                    {
                        warn!("anland bridge: clipboard image ack write failed: {e}");
                        return false;
                    }
                    true
                }
                Err(e) => {
                    warn!("anland bridge: bad clipboard image: {e}");
                    true
                }
            },
            msg::FILE_LIST => match wire::FileList::decode(data) {
                Ok(list) => {
                    let _ = file_list_tx.send(Some(list.entries));
                    if let Err(e) = wire::write_frame(
                        stream,
                        msg::CLIPBOARD_ACK,
                        &list.sequence.to_le_bytes(),
                    )
                    .await
                    {
                        warn!("anland bridge: file list ack write failed: {e}");
                        return false;
                    }
                    true
                }
                Err(e) => {
                    warn!("anland bridge: bad file list: {e}");
                    true
                }
            },
            msg::FILE_CONTENT_RESPONSE => match wire::FileContentResponse::decode(data) {
                Ok(resp) => {
                    if file_content_tx.send(resp).await.is_err() {
                        debug!("anland bridge: file content channel closed");
                    }
                    true
                }
                Err(e) => {
                    warn!("anland bridge: bad file content response: {e}");
                    true
                }
            },
            msg::CLIPBOARD_ACK => {
                if let Ok(seq) = wire::decode_clipboard_ack(data) {
                    debug!("anland bridge: clipboard ack {seq}");
                    if let Ok(mut g) = pending_clipboard.lock() {
                        if let Some((s, _)) = &*g {
                            if *s == seq {
                                *g = None;
                            }
                        }
                    }
                }
                true
            }
            other => {
                warn!("anland bridge: unexpected inbound msg type {other}");
                true
            }
        }
    }

    /// Encode + write one outbound command. Returns `false` on write failure.
    async fn write_outbound(
        stream: &mut tokio::net::UnixStream,
        cmd: OutboundCmd,
    ) -> bool {
        let (msg_type, payload): (u8, Vec<u8>) = match cmd {
            OutboundCmd::StreamStart { width, height, fps } => {
                (msg::STREAM_START, wire::encode_stream_start(width, height, fps))
            }
            OutboundCmd::StreamStop => (msg::STREAM_STOP, Vec::new()),
            OutboundCmd::IdrRequest => (msg::IDR_REQUEST, Vec::new()),
            OutboundCmd::Key { action, keycode } => {
                let mut p = Vec::with_capacity(5);
                p.push(action);
                p.extend_from_slice(&keycode.to_le_bytes());
                (msg::KEY, p)
            }
            OutboundCmd::MouseMotion { stream_id, x, y, dx, dy } => {
                let mut p = Vec::with_capacity(20);
                p.extend_from_slice(&stream_id.to_le_bytes());
                p.extend_from_slice(&x.to_le_bytes());
                p.extend_from_slice(&y.to_le_bytes());
                p.extend_from_slice(&dx.to_le_bytes());
                p.extend_from_slice(&dy.to_le_bytes());
                (msg::MOUSE_MOTION, p)
            }
            OutboundCmd::MouseButton { button, pressed } => {
                let mut p = Vec::with_capacity(5);
                p.extend_from_slice(&button.to_le_bytes());
                p.push(pressed);
                (msg::MOUSE_BUTTON, p)
            }
            OutboundCmd::MouseAxis { dx, dy } => {
                let mut p = Vec::with_capacity(8);
                p.extend_from_slice(&dx.to_le_bytes());
                p.extend_from_slice(&dy.to_le_bytes());
                (msg::MOUSE_AXIS, p)
            }
            OutboundCmd::ClipboardUpdate { sequence, text } => {
                let cu = ClipboardUpdate { sequence, text };
                (msg::CLIPBOARD_UPDATE, cu.encode())
            }
            OutboundCmd::ClipboardImage { sequence, png } => {
                let ci = wire::ClipboardImage { sequence, png };
                (msg::CLIPBOARD_IMAGE, ci.encode())
            }
            OutboundCmd::ClipboardAck { sequence } => {
                (msg::CLIPBOARD_ACK, sequence.to_le_bytes().to_vec())
            }
            OutboundCmd::FileContentRequest {
                request_id,
                index,
                offset,
                length,
            } => (
                msg::FILE_CONTENT_REQUEST,
                wire::FileContentRequest {
                    request_id,
                    index,
                    offset,
                    length,
                }
                .encode(),
            ),
        };
        if let Err(e) = wire::write_frame(stream, msg_type, &payload).await {
            warn!("anland bridge: outbound write failed: {e}");
            return false;
        }
        true
    }
}
