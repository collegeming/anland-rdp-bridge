//! CLIPRDR clipboard: bidirectional text (CF_UNICODETEXT) + image (CF_DIB)
//! sync between the Android clipboard (over the anland bridge) and `mstsc`.
//!
//! ## Data flow
//!
//! - Android → mstsc: the bridge publishes the latest text on
//!   `watch::Receiver<Option<String>>` and the latest image (PNG bytes) on
//!   `watch::Receiver<Option<Vec<u8>>>`. `on_ready` / `on_request_format_list`
//!   advertise CF_UNICODETEXT (always) + CF_DIB (when an image is present);
//!   `on_format_data_request` responds with the current text or a PNG→DIB
//!   conversion of the current image.
//! - mstsc → Android: `on_remote_copy` requests the client's text or image;
//!   `on_format_data_response` forwards the text via
//!   `AnlandBridge::send_clipboard(seq, text)` or converts the DIB→PNG and
//!   forwards via `AnlandBridge::send_clipboard_image(seq, png)`.
//!
//! File (FileGroupDescriptorW / FileContentsRequest) is NOT wired — it needs
//! the anland-side file provider contract (a follow-up wire extension).
//!
//! `ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy / SendFormatData)`
//! is the outbound path to the RDP CLIPRDR SVC. The PNG↔DIB conversions are
//! adapted from macrdp (MIT OR Apache-2.0, part of this fork's lineage).

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use image::{ImageEncoder, ImageReader};
use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags,
    FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest,
    FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use ironrdp_core::AsAny;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::{mpsc, watch, Mutex};
use tracing::{debug, info, warn};

use crate::anland_bridge::wire::{FileContentResponse as WireFileContent, FileEntry};
use crate::anland_bridge::AnlandBridge;

/// CF_UNICODETEXT (13) and CF_DIB (8) — the Windows clipboard constants.
const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;

/// Anland CLIPRDR factory: clones the bridge handle + the Android clipboard
/// text/image/file-list watches + the shared file-content receiver per
/// connection.
#[derive(Clone)]
pub struct AnlandCliprdrFactory {
    bridge: AnlandBridge,
    /// Latest Android clipboard text (cloned per-connection backend).
    clipboard_rx: watch::Receiver<Option<String>>,
    /// Latest Android clipboard image, PNG bytes (cloned per-connection).
    clipboard_image_rx: watch::Receiver<Option<Vec<u8>>>,
    /// Latest Android clipboard file list (cloned per-connection).
    file_list_rx: watch::Receiver<Option<Vec<FileEntry>>>,
    /// Shared file-content response receiver (RANGE requests correlate by
    /// `request_id` = CLIPRDR `stream_id`).
    file_content_rx: Arc<Mutex<mpsc::Receiver<WireFileContent>>>,
    /// Monotonic sequence for mstsc→Android updates (must be non-zero).
    next_seq: std::sync::Arc<AtomicU64>,
}

impl AnlandCliprdrFactory {
    pub fn new(
        bridge: AnlandBridge,
        clipboard_rx: watch::Receiver<Option<String>>,
        clipboard_image_rx: watch::Receiver<Option<Vec<u8>>>,
        file_list_rx: watch::Receiver<Option<Vec<FileEntry>>>,
        file_content_rx: mpsc::Receiver<WireFileContent>,
    ) -> Self {
        Self {
            bridge,
            clipboard_rx,
            clipboard_image_rx,
            file_list_rx,
            file_content_rx: Arc::new(Mutex::new(file_content_rx)),
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
            clipboard_image_rx: self.clipboard_image_rx.clone(),
            file_list_rx: self.file_list_rx.clone(),
            file_content_rx: Arc::clone(&self.file_content_rx),
            next_seq: std::sync::Arc::clone(&self.next_seq),
            pending_request: None,
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
    clipboard_image_rx: watch::Receiver<Option<Vec<u8>>>,
    file_list_rx: watch::Receiver<Option<Vec<FileEntry>>>,
    file_content_rx: Arc<Mutex<mpsc::Receiver<WireFileContent>>>,
    next_seq: std::sync::Arc<AtomicU64>,
    /// Which format we last asked the client for; `on_format_data_response`
    /// routes on this (the response carries no format id).
    pending_request: Option<ClipboardFormatId>,
}

impl std::fmt::Debug for AnlandCliprdrBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnlandCliprdrBackend")
            .field("pending_request", &self.pending_request)
            .finish_non_exhaustive()
    }
}

impl AnlandCliprdrBackend {
    /// Advertise: files → `ClipboardFileCopy` (registers `local_file_list` so
    /// `FileContentsRequest`s are serviced); otherwise CF_UNICODETEXT (always)
    /// + CF_DIB (when an image is present).
    fn advertise(&mut self) {
        if let Some(sender) = &self.sender {
            if let Some(entries) = self.current_files() {
                let files = build_descriptors(&entries);
                if !files.is_empty() {
                    let _ = sender.send(ServerEvent::ClipboardFileCopy(files));
                    debug!(
                        count = entries.len(),
                        "anland cliprdr: advertised file copy"
                    );
                    return;
                }
            }
            let mut formats = vec![ClipboardFormat::new(ClipboardFormatId::new(CF_UNICODETEXT))];
            if self.current_image().is_some() {
                formats.push(ClipboardFormat::new(ClipboardFormatId::new(CF_DIB)));
            }
            let count = formats.len();
            let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(
                formats,
            )));
            debug!("anland cliprdr: advertised {count} format(s)");
        }
    }

    /// Current Android clipboard file list, if any.
    fn current_files(&self) -> Option<Vec<FileEntry>> {
        self.file_list_rx.borrow().clone()
    }

    /// Current Android clipboard text (cloned from the watch).
    fn current_text(&self) -> String {
        self.clipboard_rx.borrow().clone().unwrap_or_default()
    }

    /// Current Android clipboard image (PNG bytes), if any.
    fn current_image(&self) -> Option<Vec<u8>> {
        self.clipboard_image_rx.borrow().clone()
    }
}

impl CliprdrBackend for AnlandCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        ""
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // Text + image + file. STREAM_FILECLIP_ENABLED is REQUIRED for file
        // paste — without it the cliprdr server refuses FileContentsRequests.
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
            | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
    }

    fn on_ready(&mut self) {
        self.advertise();
    }

    fn on_request_format_list(&mut self) {
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
        // Accept whatever the client offered; text+image on our side.
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // mstsc copied something. Prefer the image (CF_DIB) if offered, else
        // text (CF_UNICODETEXT); ignore otherwise.
        let has_image = available_formats.iter().any(|f| f.id().0 == CF_DIB);
        let has_text = available_formats.iter().any(|f| f.id().0 == CF_UNICODETEXT);
        let (id, name) = if has_image {
            (CF_DIB, "CF_DIB")
        } else if has_text {
            (CF_UNICODETEXT, "CF_UNICODETEXT")
        } else {
            debug!("anland cliprdr: client copy has neither text nor image; ignoring");
            return;
        };
        self.pending_request = Some(ClipboardFormatId::new(id));
        if let Some(sender) = &self.sender {
            let _ = sender.send(ServerEvent::Clipboard(
                ClipboardMessage::SendInitiatePaste(ClipboardFormatId::new(id)),
            ));
            debug!("anland cliprdr: requesting {name} from client");
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        // mstsc wants our clipboard in `request.format`.
        let response = match request.format.0 {
            CF_UNICODETEXT => {
                let text = self.current_text();
                OwnedFormatDataResponse::new_unicode_string(&text)
            }
            CF_DIB => match self.current_image() {
                Some(png) => match png_to_dib(&png) {
                    Ok(dib) => OwnedFormatDataResponse::new_data(dib),
                    Err(e) => {
                        warn!("anland cliprdr: PNG→DIB failed: {e:#}");
                        return;
                    }
                },
                None => {
                    debug!("anland cliprdr: CF_DIB requested but no image present");
                    return;
                }
            },
            id => {
                warn!(id, "anland cliprdr: format data request for unsupported format");
                return;
            }
        };
        if let Some(sender) = &self.sender {
            let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendFormatData(response)));
            debug!("anland cliprdr: sent clipboard data to client");
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        // Route on what we last asked for; the response carries no format id.
        let Some(requested) = self.pending_request.take() else {
            debug!("anland cliprdr: unsolicited format data response; ignoring");
            return;
        };
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        match requested.0 {
            CF_UNICODETEXT => {
                let text = decode_utf16(response.data());
                self.bridge.send_clipboard(seq, text);
                info!("anland cliprdr: forwarded client text to Android (seq {seq})");
            }
            CF_DIB => match dib_to_png(response.data()) {
                Ok(png) => {
                    self.bridge.send_clipboard_image(seq, png);
                    info!("anland cliprdr: forwarded client image to Android (seq {seq})");
                }
                Err(e) => {
                    warn!("anland cliprdr: DIB→PNG failed: {e:#}");
                }
            },
            _ => debug!("anland cliprdr: response for unsupported format; ignoring"),
        }
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        // Files live on Android; the request crosses the bridge asynchronously.
        // SIZE is answered synchronously from the file-list watch (we already
        // know the size); RANGE spawns a task that sends FILE_CONTENT_REQUEST
        // and awaits the matching FILE_CONTENT_RESPONSE.
        let idx = match usize::try_from(request.index) {
            Ok(i) => i,
            Err(_) => {
                warn!(index = request.index, "anland cliprdr: bad file index");
                return;
            }
        };
        let stream_id = request.stream_id;

        if request.flags.contains(FileContentsFlags::SIZE) {
            let size = self
                .current_files()
                .and_then(|entries| entries.get(idx).map(|e| e.size));
            let response = match size {
                Some(size) => FileContentsResponse::new_size_response(stream_id, size),
                None => FileContentsResponse::new_error(stream_id),
            };
            if let Some(sender) = &self.sender {
                let _ = sender.send(ServerEvent::Clipboard(
                    ClipboardMessage::SendFileContentsResponse(response),
                ));
            }
            debug!(stream_id, size, "anland cliprdr: served SIZE");
            return;
        }

        if request.flags.contains(FileContentsFlags::RANGE) {
            let bridge = self.bridge.clone();
            let sender = self.sender.clone();
            let rx = Arc::clone(&self.file_content_rx);
            let position = request.position;
            let requested_size = request.requested_size;
            tokio::spawn(async move {
                bridge.request_file_content(stream_id, idx as u32, position, requested_size);
                // Await the response correlated by stream_id (== request_id),
                // with a timeout so a vanished Android side can't leak a task.
                let data = match tokio::time::timeout(
                    Duration::from_secs(5),
                    async {
                        loop {
                            let resp = rx.lock().await.recv().await?;
                            if resp.request_id == stream_id {
                                return Some(resp.data);
                            }
                        }
                    },
                )
                .await
                {
                    Ok(Some(data)) => data,
                    _ => Vec::new(),
                };
                let response = if data.is_empty() {
                    FileContentsResponse::new_error(stream_id)
                } else {
                    FileContentsResponse::new_data_response(stream_id, data)
                };
                if let Some(s) = sender {
                    let _ = s.send(ServerEvent::Clipboard(
                        ClipboardMessage::SendFileContentsResponse(response),
                    ));
                }
            });
            debug!(stream_id, index = idx, position, "anland cliprdr: RANGE requested");
            return;
        }

        warn!(flags = ?request.flags, "anland cliprdr: unknown FileContentsRequest flags");
    }

    fn on_file_contents_response(&mut self, _response: ironrdp_cliprdr::pdu::FileContentsResponse<'_>) {
        debug!("anland cliprdr: file contents response (unsupported); ignoring");
    }

    fn on_lock(&mut self, _data_id: LockDataId) {
        // Lock/unlock of clipboard data IDs is not used for text+image sync.
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

/// Build CLIPRDR `FileDescriptor`s from the bridge file entries (normal files
/// with their size; directories would use DIRECTORY attributes).
fn build_descriptors(entries: &[FileEntry]) -> Vec<FileDescriptor> {
    entries
        .iter()
        .map(|e| {
            FileDescriptor::new(e.name.clone())
                .with_attributes(ClipboardFileAttributes::NORMAL)
                .with_file_size(e.size)
        })
        .collect()
}

/// Decode UTF-16LE bytes (with optional trailing NUL) to a Rust String.
fn decode_utf16(bytes: &[u8]) -> String {    let mut units = Vec::with_capacity(bytes.len() / 2);
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

/// Convert PNG bytes to a CF_DIB payload: a 40-byte BITMAPINFOHEADER followed
/// by 32bpp BGRA pixels, top-down (negative biHeight). 32bpp is the most
/// widely supported variant; BITMAPV5HEADER is deliberately not emitted since
/// it complicates color-space negotiation with older clients.
fn png_to_dib(png: &[u8]) -> Result<Vec<u8>> {
    let img = ImageReader::new(Cursor::new(png))
        .with_guessed_format()
        .context("guess PNG format")?
        .decode()
        .context("decode PNG")?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    let row_bytes = (w as usize) * 4;
    let pixel_bytes = row_bytes * (h as usize);

    let mut out = Vec::with_capacity(40 + pixel_bytes);
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(-(h as i32)).to_le_bytes()); // biHeight (negative = top-down)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0u32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // RGBA → BGRA, row order already top-down.
    for px in img.pixels() {
        let [r, g, b, a] = px.0;
        out.extend_from_slice(&[b, g, r, a]);
    }
    Ok(out)
}

/// Parse a CF_DIB payload into PNG bytes. Accepts any header ≥ 40
/// (BITMAPINFOHEADER), 24bpp or 32bpp uncompressed (BI_RGB), top-down or
/// bottom-up. Everything else is rejected.
fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    if dib.len() < 40 {
        bail!("DIB shorter than BITMAPINFOHEADER");
    }
    let bi_size = u32::from_le_bytes(dib[0..4].try_into().unwrap()) as usize;
    if bi_size < 40 || bi_size > dib.len() {
        bail!("bogus biSize {bi_size}");
    }
    let width = i32::from_le_bytes(dib[4..8].try_into().unwrap());
    let height_signed = i32::from_le_bytes(dib[8..12].try_into().unwrap());
    let bit_count = u16::from_le_bytes(dib[14..16].try_into().unwrap());
    let compression = u32::from_le_bytes(dib[16..20].try_into().unwrap());

    if width <= 0 {
        bail!("invalid width {width}");
    }
    if height_signed == 0 {
        bail!("invalid height 0");
    }
    // BI_RGB (0) canonical layout; BI_BITFIELDS (3) / BI_ALPHABITFIELDS (6)
    // accepted for 32bpp under the standard ARGB masks modern Windows emits.
    let bitfields = compression == 3 || compression == 6;
    if compression != 0 && !bitfields {
        bail!("unsupported BI_COMPRESSION {compression}");
    }
    if bit_count != 24 && bit_count != 32 {
        bail!("unsupported biBitCount {bit_count}");
    }
    if bitfields && bit_count != 32 {
        bail!("BI_BITFIELDS with biBitCount={bit_count} not supported");
    }

    let w = width as u32;
    let h = height_signed.unsigned_abs();
    let top_down = height_signed < 0;
    let bpp = (bit_count / 8) as usize;
    // BMP rows are padded to a 4-byte multiple.
    let stride = (w as usize * bpp + 3) & !3;
    // Out-of-band masks after a 40-byte header with BI_BITFIELDS.
    let mask_bytes = if bitfields && bi_size == 40 {
        if compression == 6 {
            16 // RGBA masks
        } else {
            12 // RGB masks
        }
    } else {
        0
    };
    let pixel_start = bi_size + mask_bytes;
    let need = pixel_start
        .checked_add(stride.checked_mul(h as usize).ok_or_else(|| anyhow!("overflow"))?)
        .ok_or_else(|| anyhow!("overflow"))?;
    if dib.len() < need {
        bail!("DIB payload truncated: have {}, need {need}", dib.len());
    }

    let cap = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow!("RGBA buffer size overflow"))?;
    let mut rgba: Vec<u8> = Vec::with_capacity(cap);
    for row in 0..h {
        let src_row = if top_down { row } else { h - 1 - row };
        let row_off = pixel_start + (src_row as usize) * stride;
        let row_bytes = &dib[row_off..row_off + w as usize * bpp];
        for chunk in row_bytes.chunks_exact(bpp) {
            let (b, g, r, a) = if bpp == 4 {
                (chunk[0], chunk[1], chunk[2], chunk[3])
            } else {
                (chunk[0], chunk[1], chunk[2], 0xFF)
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    encoder.write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny 2×1 RGBA PNG: two opaque red pixels.
    fn red_png() -> Vec<u8> {
        let mut png = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png);
        encoder
            .write_image(
                &[255, 0, 0, 255, 255, 0, 0, 255],
                2,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        png
    }

    #[test]
    fn png_to_dib_produces_bgra_top_down() {
        let dib = png_to_dib(&red_png()).unwrap();
        assert!(dib.len() >= 40 + 8);
        // BITMAPINFOHEADER: biSize=40, width=2, height=-1 (top-down), 32bpp.
        assert_eq!(&dib[0..4], &40u32.to_le_bytes());
        assert_eq!(&dib[4..8], &2i32.to_le_bytes());
        assert_eq!(&dib[8..12], &(-1i32).to_le_bytes());
        assert_eq!(&dib[14..16], &32u16.to_le_bytes());
        // Pixels are BGRA: red = [0, 0, 255, 255].
        assert_eq!(&dib[40..44], &[0, 0, 255, 255]);
        assert_eq!(&dib[44..48], &[0, 0, 255, 255]);
    }

    #[test]
    fn dib_to_png_round_trips() {
        let png = red_png();
        let dib = png_to_dib(&png).unwrap();
        let back = dib_to_png(&dib).unwrap();
        // Re-decode the PNG to check the pixels survive.
        let img = ImageReader::new(Cursor::new(&back))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .to_rgba8();
        assert_eq!((img.width(), img.height()), (2, 1));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn dib_to_png_rejects_garbage() {
        assert!(dib_to_png(&[0u8; 10]).is_err());
        let mut bad = vec![0u8; 40];
        bad[14..16].copy_from_slice(&64u16.to_le_bytes()); // biBitCount=64
        assert!(dib_to_png(&bad).is_err());
    }
}
