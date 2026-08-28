#![allow(dead_code)]
//! Frame framing, message-type constants, and typed payload decode for the
//! anland bridge protocol v1.
//!
//! Every frame on the wire is:
//!
//! ```text
//! [u32 frame_length, big-endian][u8 message_type][payload]
//! ```
//!
//! `frame_length` includes the type byte, so the payload length is
//! `frame_length - 1`. `frame_length` must be 1–16 MiB inclusive. Unless
//! stated otherwise, numeric payload fields are little-endian.

use anyhow::{ensure, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum frame length: 16 MiB.
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Message type identifiers.
///
/// Application messages are 1–19; authentication messages are 32–35. No
/// application message is accepted before `AUTH_OK` completes.
pub mod msg {
    // Application messages.
    pub const KEY: u8 = 1;
    pub const MOUSE_MOTION: u8 = 2;
    pub const MOUSE_BUTTON: u8 = 3;
    pub const MOUSE_AXIS: u8 = 4;
    pub const CLIPBOARD_UPDATE: u8 = 5;
    pub const CLIPBOARD_ACK: u8 = 6;
    /// An image (PNG bytes) added to the clipboard, separate from the text
    /// channel — `sequence:u64 || png[]`.
    ///
    /// NOTE: 7 is the Android consumer's `INPUT_RESET` (a legacy message not
    /// in the lamco spec); the image message is 8 to avoid a wire collision.
    pub const CLIPBOARD_IMAGE: u8 = 8;
    pub const VIDEO_FRAME: u8 = 16;
    pub const IDR_REQUEST: u8 = 17;
    pub const STREAM_START: u8 = 18;
    pub const STREAM_STOP: u8 = 19;
    pub const FILE_LIST: u8 = 23;
    pub const FILE_CONTENT_REQUEST: u8 = 24;
    pub const FILE_CONTENT_RESPONSE: u8 = 25;

    // Authentication messages.
    pub const AUTH_INIT: u8 = 32;
    pub const AUTH_SERVER_PROOF: u8 = 33;
    pub const AUTH_CLIENT_PROOF: u8 = 34;
    pub const AUTH_OK: u8 = 35;
}

/// A decoded frame: a message type and its raw payload (without the type
/// byte).
#[derive(Debug, Clone)]
pub struct Frame {
    pub msg_type: u8,
    pub payload: Vec<u8>,
}

/// Read one frame from `r`. Validates the length is 1–16 MiB; returns the
/// type byte and the payload (everything after it).
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let frame_len = u32::from_be_bytes(len_buf);
    ensure!(
        (1..=MAX_FRAME_LEN).contains(&frame_len),
        "anland bridge: frame length {frame_len} out of range (1..={MAX_FRAME_LEN})"
    );
    // frame_len includes the type byte, so the buffer is exactly frame_len
    // bytes: [type][payload...].
    let mut buf = vec![0u8; usize::try_from(frame_len)?];
    r.read_exact(&mut buf).await?;
    let msg_type = buf[0];
    let payload = buf[1..].to_vec();
    Ok(Frame { msg_type, payload })
}

/// Write one frame to `w` with the given type and payload. The length prefix
/// counts the type byte, so `frame_len = 1 + payload.len()`.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, msg_type: u8, payload: &[u8]) -> Result<()> {
    let frame_len = 1u32
        .checked_add(u32::try_from(payload.len())?)
        .filter(|&l| l <= MAX_FRAME_LEN)
        .ok_or_else(|| anyhow::anyhow!("anland bridge: frame too large"))?;
    let mut out = Vec::with_capacity(4 + 1 + payload.len());
    out.extend_from_slice(&frame_len.to_be_bytes());
    out.push(msg_type);
    out.extend_from_slice(payload);
    w.write_all(&out).await?;
    Ok(())
}

/// A decoded `VIDEO_FRAME` (message type 16) payload.
///
/// Wire layout (little-endian):
///
/// ```text
/// u16 encoded_width
/// u16 encoded_height
/// u16 visible_width
/// u16 visible_height
/// u32 timestamp_ms
/// u8  keyframe
/// u8  annex_b_h264[]
/// ```
///
/// Visible dimensions define the fixed RDP desktop and AVC420 region; encoded
/// dimensions are aligned to 16 pixels. An empty H.264 payload is invalid.
#[derive(Debug, Clone)]
pub struct VideoFramePayload {
    pub encoded_width: u16,
    pub encoded_height: u16,
    pub visible_width: u16,
    pub visible_height: u16,
    pub timestamp_ms: u32,
    pub is_keyframe: bool,
    /// Raw H.264 NAL units in Annex-B start-code framing — the exact bytes
    /// mstsc's decoder requires.
    pub nal: Vec<u8>,
}

impl VideoFramePayload {
    /// Fixed header size: 4×u16 + u32 + u8 = 13 bytes.
    pub const HEADER_LEN: usize = 13;

    /// Decode a `VIDEO_FRAME` payload. Rejects short payloads and empty H.264.
    pub fn decode(payload: &[u8]) -> Result<Self> {
        ensure!(
            payload.len() >= Self::HEADER_LEN,
            "anland bridge: video frame payload too short ({} < {})",
            payload.len(),
            Self::HEADER_LEN
        );
        let encoded_width = u16::from_le_bytes(payload[0..2].try_into().unwrap());
        let encoded_height = u16::from_le_bytes(payload[2..4].try_into().unwrap());
        let visible_width = u16::from_le_bytes(payload[4..6].try_into().unwrap());
        let visible_height = u16::from_le_bytes(payload[6..8].try_into().unwrap());
        let timestamp_ms = u32::from_le_bytes(payload[8..12].try_into().unwrap());
        let is_keyframe = payload[12] != 0;
        let nal = payload[Self::HEADER_LEN..].to_vec();
        ensure!(!nal.is_empty(), "anland bridge: video frame has empty H.264 payload");
        Ok(Self {
            encoded_width,
            encoded_height,
            visible_width,
            visible_height,
            timestamp_ms,
            is_keyframe,
            nal,
        })
    }
}

/// Clipboard text bound: 1 MiB excluding the 8-byte sequence field.
pub const CLIPBOARD_MAX_TEXT_BYTES: usize = 1_048_576;

/// A decoded `CLIPBOARD_UPDATE` / `CLIPBOARD_ACK` (messages 5 / 6) payload.
///
/// Both carry a `u64` little-endian sequence; `CLIPBOARD_UPDATE` follows it
/// with the strict-UTF-8 text bytes (zero bytes = clear).
#[derive(Debug, Clone)]
pub struct ClipboardUpdate {
    pub sequence: u64,
    pub text: String,
}

impl ClipboardUpdate {
    /// Decode a `CLIPBOARD_UPDATE` payload. Validates the sequence is non-zero,
    /// the text is strict UTF-8, embedded NUL is forbidden, and the text is
    /// within the 1 MiB bound.
    pub fn decode(payload: &[u8]) -> Result<Self> {
        ensure!(
            payload.len() >= 8,
            "anland bridge: clipboard payload too short for sequence"
        );
        let sequence = u64::from_le_bytes(payload[0..8].try_into().unwrap());
        ensure!(sequence != 0, "anland bridge: clipboard sequence must be non-zero");
        let text_bytes = &payload[8..];
        ensure!(
            text_bytes.len() <= CLIPBOARD_MAX_TEXT_BYTES,
            "anland bridge: clipboard text exceeds {} bytes",
            CLIPBOARD_MAX_TEXT_BYTES
        );
        // Embedded NUL is forbidden.
        ensure!(
            !text_bytes.contains(&0u8),
            "anland bridge: clipboard text contains embedded NUL"
        );
        // Strict UTF-8 (rejects surrogate / overlong / truncated sequences).
        let text = std::str::from_utf8(text_bytes)
            .map(|s| s.to_owned())
            .map_err(|e| anyhow::anyhow!("anland bridge: clipboard text is not strict UTF-8: {e}"))?;
        Ok(Self { sequence, text })
    }

    /// Encode a `CLIPBOARD_UPDATE` payload for sending.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.text.len());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(self.text.as_bytes());
        out
    }
}

/// Decode the `u64` little-endian sequence from a `CLIPBOARD_ACK` payload.
pub fn decode_clipboard_ack(payload: &[u8]) -> Result<u64> {
    ensure!(
        payload.len() >= 8,
        "anland bridge: clipboard ack payload too short"
    );
    let sequence = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    ensure!(sequence != 0, "anland bridge: clipboard ack sequence must be non-zero");
    Ok(sequence)
}

/// A decoded `CLIPBOARD_IMAGE` (message type 7) payload: a non-zero `u64`
/// sequence plus PNG-encoded image bytes (non-empty).
#[derive(Debug, Clone)]
pub struct ClipboardImage {
    pub sequence: u64,
    pub png: Vec<u8>,
}

impl ClipboardImage {
    pub const SEQ_LEN: usize = 8;

    pub fn decode(payload: &[u8]) -> Result<Self> {
        ensure!(
            payload.len() >= Self::SEQ_LEN + 1,
            "anland bridge: clipboard image too short"
        );
        let sequence = u64::from_le_bytes(payload[0..Self::SEQ_LEN].try_into().unwrap());
        ensure!(sequence != 0, "anland bridge: clipboard image sequence must be non-zero");
        let png = payload[Self::SEQ_LEN..].to_vec();
        ensure!(!png.is_empty(), "anland bridge: clipboard image has empty payload");
        Ok(Self { sequence, png })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SEQ_LEN + self.png.len());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&self.png);
        out
    }
}

/// One clipboard file entry (name + size), carried in `FILE_LIST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
}

/// A decoded `FILE_LIST` (message type 23) payload: Android's current
/// clipboard file list.
///
/// Wire layout (little-endian):
///
/// ```text
/// u64 sequence
/// u32 count
/// entries[]: u16 name_len || name (UTF-8) || u64 size
/// ```
#[derive(Debug, Clone)]
pub struct FileList {
    pub sequence: u64,
    pub entries: Vec<FileEntry>,
}

impl FileList {
    pub fn decode(payload: &[u8]) -> Result<Self> {
        ensure!(payload.len() >= 12, "anland bridge: file list too short");
        let sequence = u64::from_le_bytes(payload[0..8].try_into().unwrap());
        ensure!(sequence != 0, "anland bridge: file list sequence must be non-zero");
        let count = u32::from_le_bytes(payload[8..12].try_into().unwrap());
        ensure!(count <= 1024, "anland bridge: file list count {count} too large");
        let mut pos = 12usize;
        let mut entries = Vec::with_capacity(count as usize);
        for _ in 0..count {
            ensure!(
                pos + 2 <= payload.len(),
                "anland bridge: file list truncated at entry name length"
            );
            let name_len = u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;
            ensure!(
                pos + name_len + 8 <= payload.len(),
                "anland bridge: file list truncated in entry {pos}"
            );
            let name = std::str::from_utf8(&payload[pos..pos + name_len])
                .map_err(|e| anyhow::anyhow!("anland bridge: file name not UTF-8: {e}"))?
                .to_owned();
            pos += name_len;
            let size = u64::from_le_bytes(payload[pos..pos + 8].try_into().unwrap());
            pos += 8;
            entries.push(FileEntry { name, size });
        }
        Ok(Self { sequence, entries })
    }
}

/// A `FILE_CONTENT_REQUEST` (message type 24) payload: fetch `length` bytes at
/// `offset` of entry `index`. Server → Android.
#[derive(Debug, Clone, Copy)]
pub struct FileContentRequest {
    pub request_id: u32,
    pub index: u32,
    pub offset: u64,
    pub length: u32,
}

impl FileContentRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20);
        out.extend_from_slice(&self.request_id.to_le_bytes());
        out.extend_from_slice(&self.index.to_le_bytes());
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.length.to_le_bytes());
        out
    }
}

/// A `FILE_CONTENT_RESPONSE` (message type 25) payload: the requested bytes,
/// or empty on EOF/error. Android → server.
#[derive(Debug, Clone)]
pub struct FileContentResponse {
    pub request_id: u32,
    pub data: Vec<u8>,
}

impl FileContentResponse {
    pub const HEADER_LEN: usize = 4;

    pub fn decode(payload: &[u8]) -> Result<Self> {
        ensure!(
            payload.len() >= Self::HEADER_LEN,
            "anland bridge: file content response too short"
        );
        let request_id = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let data = payload[Self::HEADER_LEN..].to_vec();
        Ok(Self { request_id, data })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_LEN + self.data.len());
        out.extend_from_slice(&self.request_id.to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }
}

/// Encode a `STREAM_START` payload: `visible_width:u16, visible_height:u16, fps:u8`.
pub fn encode_stream_start(width: u16, height: u16, fps: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.push(fps);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trip() {
        let mut buf: Vec<u8> = Vec::new();
        // write_frame writes to AsyncWrite; a Vec<u8> isn't AsyncWrite directly,
        // so encode manually for the round-trip and verify decode.
        let payload = b"hello anland";
        let frame_len = 1u32 + payload.len() as u32;
        buf.extend_from_slice(&frame_len.to_be_bytes());
        buf.push(msg::KEY);
        buf.extend_from_slice(payload);

        let mut reader = &buf[..];
        let frame = read_frame(&mut reader).await.unwrap();
        assert_eq!(frame.msg_type, msg::KEY);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn video_frame_decode() {
        let mut p = Vec::new();
        p.extend_from_slice(&1920u16.to_le_bytes()); // encoded_width
        p.extend_from_slice(&1080u16.to_le_bytes()); // encoded_height
        p.extend_from_slice(&1280u16.to_le_bytes()); // visible_width
        p.extend_from_slice(&720u16.to_le_bytes()); // visible_height
        p.extend_from_slice(&42_000u32.to_le_bytes()); // timestamp_ms
        p.push(1u8); // keyframe
        p.extend_from_slice(&[0, 0, 0, 1, 0x67]); // Annex-B SPS start
        let v = VideoFramePayload::decode(&p).unwrap();
        assert_eq!(v.encoded_width, 1920);
        assert_eq!(v.visible_width, 1280);
        assert_eq!(v.timestamp_ms, 42_000);
        assert!(v.is_keyframe);
        assert_eq!(&v.nal, &[0, 0, 0, 1, 0x67]);
    }

    #[test]
    fn video_frame_rejects_empty_nal() {
        let p = vec![0u8; VideoFramePayload::HEADER_LEN];
        assert!(VideoFramePayload::decode(&p).is_err());
    }

    #[test]
    fn clipboard_decode_and_encode() {
        let mut p = Vec::new();
        p.extend_from_slice(&7u64.to_le_bytes());
        p.extend_from_slice("héllo".as_bytes());
        let c = ClipboardUpdate::decode(&p).unwrap();
        assert_eq!(c.sequence, 7);
        assert_eq!(c.text, "héllo");
        assert_eq!(c.encode(), p);
    }

    #[test]
    fn clipboard_rejects_embedded_nul() {
        let mut p = Vec::new();
        p.extend_from_slice(&1u64.to_le_bytes());
        p.extend_from_slice(b"a\0b");
        assert!(ClipboardUpdate::decode(&p).is_err());
    }

    #[test]
    fn clipboard_rejects_zero_sequence() {
        let p = vec![0u8; 9];
        assert!(ClipboardUpdate::decode(&p).is_err());
    }

    #[test]
    fn clipboard_rejects_non_utf8() {
        let mut p = Vec::new();
        p.extend_from_slice(&1u64.to_le_bytes());
        p.extend_from_slice(&[0xff, 0xfe]);
        assert!(ClipboardUpdate::decode(&p).is_err());
    }

    #[test]
    fn clipboard_image_round_trip() {
        let img = ClipboardImage {
            sequence: 3,
            png: b"\x89PNG\r\n\x1a\nfake-png".to_vec(),
        };
        let decoded = ClipboardImage::decode(&img.encode()).unwrap();
        assert_eq!(decoded.sequence, 3);
        assert_eq!(decoded.png, img.png);
    }

    #[test]
    fn clipboard_image_rejects_zero_seq_and_empty() {
        let mut zero_seq = Vec::new();
        zero_seq.extend_from_slice(&0u64.to_le_bytes());
        zero_seq.extend_from_slice(b"png");
        assert!(ClipboardImage::decode(&zero_seq).is_err());

        let empty = vec![0u8; ClipboardImage::SEQ_LEN];
        assert!(ClipboardImage::decode(&empty).is_err());
    }

    #[test]
    fn file_list_round_trip() {
        let mut p = Vec::new();
        p.extend_from_slice(&5u64.to_le_bytes()); // sequence
        p.extend_from_slice(&2u32.to_le_bytes()); // count
        for (name, size) in [("a.txt", 10u64), ("b.bin", 1_000_000u64)] {
            let nb = name.as_bytes();
            p.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            p.extend_from_slice(nb);
            p.extend_from_slice(&size.to_le_bytes());
        }
        let fl = FileList::decode(&p).unwrap();
        assert_eq!(fl.sequence, 5);
        assert_eq!(
            fl.entries,
            vec![
                FileEntry { name: "a.txt".into(), size: 10 },
                FileEntry { name: "b.bin".into(), size: 1_000_000 },
            ]
        );
    }

    #[test]
    fn file_list_rejects_truncated_and_zero_seq() {
        // Truncated mid-entry.
        let mut p = Vec::new();
        p.extend_from_slice(&1u64.to_le_bytes());
        p.extend_from_slice(&1u32.to_le_bytes());
        p.extend_from_slice(&5u16.to_le_bytes()); // name_len
        p.extend_from_slice(b"ab"); // only 2 of 5 bytes
        assert!(FileList::decode(&p).is_err());

        // Zero sequence.
        let mut z = Vec::new();
        z.extend_from_slice(&0u64.to_le_bytes());
        z.extend_from_slice(&0u32.to_le_bytes());
        assert!(FileList::decode(&z).is_err());
    }

    #[test]
    fn file_content_request_response_round_trip() {
        let req = FileContentRequest {
            request_id: 7,
            index: 1,
            offset: 4096,
            length: 64 * 1024,
        };
        let encoded = req.encode();
        // The request is one-directional; just pin the layout.
        assert_eq!(encoded.len(), 20);

        let resp = FileContentResponse {
            request_id: 7,
            data: vec![0xAB; 16],
        };
        let decoded = FileContentResponse::decode(&resp.encode()).unwrap();
        assert_eq!(decoded.request_id, 7);
        assert_eq!(decoded.data, vec![0xAB; 16]);
    }
}
