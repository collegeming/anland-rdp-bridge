# Clipboard file transfer — wire extension + CLIPRDR backend design

Status: **protocol layer implemented** (wire messages + bridge plumbing + unit
tests). The CLIPRDR backend serving (`FileGroupDescriptorW` / `FileContentsRequest`)
and the Android-side provider are the remaining implementation.

## Why this is harder than text/image

The CLIPRDR file path is an **asynchronous request/response** protocol:
`mstsc` sends `FileContentsRequest` (SIZE or RANGE) over CLIPRDR, and the server
must reply with `FileContentsResponse`. For text/image the data is already in a
watch channel, so the sync `CliprdrBackend` callback can answer immediately.
For files, the content lives on the **Android side** and must be fetched over
the bridge — an async round-trip. The `CliprdrBackend` trait methods are **sync**,
so the file-serving cannot `await` inside the callback.

## Wire extension (implemented)

Three new message types carry the file path over the bridge:

| Type | Name | Direction | Payload |
|---:|---|---|---|
| 23 | `FILE_LIST` | Android → server | `sequence:u64, count:u32, entries[] (name_len:u16, name, size:u64)` |
| 24 | `FILE_CONTENT_REQUEST` | server → Android | `request_id:u32, index:u32, offset:u64, length:u32` |
| 25 | `FILE_CONTENT_RESPONSE` | Android → server | `request_id:u32, data[]` (empty = EOF/error) |

- `FILE_LIST` is ACKed (CLIPBOARD_ACK, same sequence) and coalesced into a
  `watch::Receiver<Option<Vec<FileEntry>>>`.
- `FILE_CONTENT_RESPONSE` is pushed to `mpsc::Receiver<FileContentResponse>`,
  correlated by `request_id`.
- `AnlandBridge::request_file_content(request_id, index, offset, length)` sends
  `FILE_CONTENT_REQUEST`.

The server-side plumbing in `anland_bridge` + `platform` is done; the CLIPRDR
backend is the next step.

## CLIPRDR backend design (next step)

The sync `CliprdrBackend` callback must bridge to the async file-content
channel. Proposal (mirrors macrdp's Mac→Windows-only file path, re-pointed at
Android):

- **Advertise** `FileGroupDescriptorW` (0xC004) when the file-list watch is
  non-empty (like `CF_DIB`).
- **`on_format_data_request`** for `FileGroupDescriptorW`: build the
  `FileDescriptor[]` from the watch entries (name + size), reply with
  `OwnedFormatDataResponse::new_file_list(...)`.
- **`on_file_contents_request`**: spawn a background task that owns a clone of
  the file-content receiver and the RDP event sender. The task:
  1. sends `FILE_CONTENT_REQUEST` via `AnlandBridge::request_file_content`;
  2. awaits the matching `FILE_CONTENT_RESPONSE` (by `request_id`) on
     `file_content_rx`;
  3. replies with `ClipboardMessage::SendFileContentsResponse` (SIZE →
     `new_size_response`, RANGE → `new_data_response`).
  The receiver must be shared across concurrent requests — either
  `Arc<tokio::sync::Mutex<mpsc::Receiver<_>>>` (single outstanding request at a
  time is the normal case) or a per-request oneshot. Direction: **Android →
  Windows only** (copy a file on Android → paste into mstsc), matching macrdp's
  Mac→Windows-only model.

## Android-side contract (to implement in the consumer)

- On Android clipboard change to a file (SAF / FileProvider URI), send
  `FILE_LIST` with the entries.
- On `FILE_CONTENT_REQUEST { index, offset, length }`, read that byte range of
  the file (ContentResolver, chunked to the requested length) and reply
  `FILE_CONTENT_RESPONSE { request_id, data }`; empty `data` for EOF/error.
- Bounds: length per request is capped by the 16 MiB frame limit; mstsc chunks
  at ≤ 1 MiB in practice.

## Out of scope here

Windows → Android file copy (a `FileContentsResponse` path) is not designed;
macrdp only does the local→Windows direction, and anland's bridge is the same
shape. It would need a second pair of messages + a Windows file provider.
