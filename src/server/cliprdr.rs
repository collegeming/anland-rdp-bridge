//! CLIPRDR text clipboard: bidirectional text sync between the Android
//! clipboard (over the anland bridge) and `mstsc`'s CF_UNICODETEXT.
//!
//! Image (CF_DIB) and file (FileGroupDescriptorW / FileContentsRequest) are
//! NOT wired here — the bridge wire protocol's `CLIPBOARD_UPDATE` is text-
//! only (strict UTF-8, 1 MiB). Extending the wire for image/file is a
//! follow-up. File/image requests from the client return CB_RESPONSE_FAIL
//! rather than hanging.
//!
//! ## Data flow
//!
//! - Android → mstsc: the bridge publishes the latest text on a
//!   `watch::Receiver<Option<String>>`. `on_ready` / `on_request_format_list`
//!   advertise CF_UNICODETEXT; `on_format_data_request` responds with the
//!   current text.
//! - mstsc → Android: `on_format_data_response` for CF_UNICODETEXT forwards
//!   the text to Android via `AnlandBridge::send_clipboard(seq, text)`.
//!
//! `ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy / SendFormatData)`
//! is the outbound path to the RDP CLIPRDR SVC.

use std::sync::atomic::{AtomicU64, Ordering};

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FormatDataRequest,
    FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use ironrdp_core::AsAny;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::anland_bridge::AnlandBridge;

/// CF_UNICODETEXT format id (the Windows clipboard constant).
const CF_UNICODETEXT: u32 = 13;

/// Anland CLIPRDR factory: clones the bridge handle + the Android clipboard
/// watch receiver per connection.
#[derive(Clone)]
pub struct AnlandCliprdrFactory {
    bridge: AnlandBridge,
    /// Latest Android clipboard text (cloned per-connection backend).
    clipboard_rx: watch::Receiver<Option<String>>,
    /// Monotonic sequence for mstsc→Android updates (must be non-zero).
    next_seq: std::sync::Arc<AtomicU64>,
}

impl AnlandCliprdrFactory {
    pub fn new(bridge: AnlandBridge, clipboard_rx: watch::Receiver<Option<String>>) -> Self {
        Self {
            bridge,
            clipboard_rx,
            next_seq: std::sync::Arc::new(AtomicU64::new(1)),
        }
    }
}

impl ServerEventSender for AnlandCliprdrFactory {
    fn set_sender(&mut self, _sender: mpsc::UnboundedSender<ServerEvent>) {
        // The backend built per connection receives the sender directly.
    }
}

impl CliprdrBackendFactory for AnlandCliprdrFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(AnlandCliprdrBackend {
            bridge: self.bridge.clone(),
            sender: None,
            clipboard_rx: self.clipboard_rx.clone(),
            next_seq: std::sync::Arc::clone(&self.next_seq),
            advertised: false,
        })
    }
}

impl CliprdrServerFactory for AnlandCliprdrFactory {}

/// Per-connection CLIPRDR backend.
pub struct AnlandCliprdrBackend {
    bridge: AnlandBridge,
    /// Outbound RDP event sender; set by the server after construction.
    sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    clipboard_rx: watch::Receiver<Option<String>>,
    next_seq: std::sync::Arc<AtomicU64>,
    /// Whether we have already advertised our (single) format list this
    /// connection.
    advertised: bool,
}

impl std::fmt::Debug for AnlandCliprdrBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnlandCliprdrBackend")
            .field("advertised", &self.advertised)
            .finish_non_exhaustive()
    }
}

impl AnlandCliprdrBackend {
    fn advertise(&mut self) {
        if let Some(sender) = &self.sender {
            // CF_UNICODETEXT is a standard registered format (id 13); no
            // long-format name is required.
            let format = ClipboardFormat::new(ClipboardFormatId::new(CF_UNICODETEXT));
            let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(
                vec![format],
            )));
            self.advertised = true;
            debug!("anland cliprdr: advertised CF_UNICODETEXT");
        }
    }

    /// Current Android clipboard text (cloned from the watch).
    fn current_text(&self) -> String {
        self.clipboard_rx.borrow().clone().unwrap_or_default()
    }
}

impl CliprdrBackend for AnlandCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        ""
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // Text-only: no file/object capabilities.
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        // Advertise CF_UNICODETEXT once the CLIPRDR channel is up.
        self.advertise();
    }

    fn on_request_format_list(&mut self) {
        // Client asked us to (re-)send our format list.
        self.advertised = false;
        self.advertise();
    }

    fn on_format_list_response(&mut self, ok: bool) {
        if ok {
            debug!("anland cliprdr: client accepted our format list");
        } else {
            warn!("anland cliprdr: client rejected our format list");
        }
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {
        // Accept whatever the client offered; text-only on our side.
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // mstsc copied something. If CF_UNICODETEXT is among the offered
        // formats, request it; otherwise ignore (image/file not supported).
        let wants_text = available_formats.iter().any(|f| f.id().0 == CF_UNICODETEXT);
        if wants_text {
            if let Some(sender) = &self.sender {
                let _ = sender.send(ServerEvent::Clipboard(
                    ClipboardMessage::SendInitiatePaste(ClipboardFormatId::new(CF_UNICODETEXT)),
                ));
                debug!("anland cliprdr: requesting CF_UNICODETEXT from client");
            }
        } else {
            debug!("anland cliprdr: client copy has no text format; ignoring");
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        // mstsc wants our clipboard in `request.format`. Respond with the
        // current Android text if it's CF_UNICODETEXT; fail otherwise (image/
        // file unsupported here).
        if request.format.0 != CF_UNICODETEXT {
            warn!(
                id = request.format.0,
                "anland cliprdr: format data request for non-text format; failing"
            );
            return;
        }
        let text = self.current_text();
        if let Some(sender) = &self.sender {
            let response = OwnedFormatDataResponse::new_unicode_string(&text);
            let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendFormatData(response)));
            debug!(bytes = text.len(), "anland cliprdr: sent CF_UNICODETEXT to client");
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        // mstsc delivered its clipboard text. Decode UTF-16LE and forward to
        // Android via the bridge with the next sequence number.
        let text = decode_utf16(response.data());
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        self.bridge.send_clipboard(seq, text);
        info!("anland cliprdr: forwarded client clipboard to Android (seq {seq})");
    }

    fn on_file_contents_request(&mut self, _request: ironrdp_cliprdr::pdu::FileContentsRequest) {
        // File copy is not supported over the text-only bridge yet.
        debug!("anland cliprdr: file contents request (unsupported); ignoring");
    }

    fn on_file_contents_response(&mut self, _response: ironrdp_cliprdr::pdu::FileContentsResponse<'_>) {
        debug!("anland cliprdr: file contents response (unsupported); ignoring");
    }

    fn on_lock(&mut self, _data_id: LockDataId) {
        // Lock/unlock of clipboard data IDs is not used for text-only sync.
    }

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

impl AsAny for AnlandCliprdrBackend {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl ServerEventSender for AnlandCliprdrBackend {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.sender = Some(sender);
    }
}

/// Decode UTF-16LE bytes (with optional trailing NUL) to a Rust String.
fn decode_utf16(bytes: &[u8]) -> String {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i + 1 < bytes.len() {
        units.push(u16::from_le_bytes([bytes[i], bytes[i + 1]]));
        i += 2;
    }
    // Strip a single trailing NUL if present.
    if units.last() == Some(&0) {
        units.pop();
    }
    String::from_utf16_lossy(&units)
}
