# Clipboard file transfer — wire extension + CLIPRDR backend design

Status: **implemented server-side** (wire messages + bridge plumbing + CLIPRDR
backend). The Android-side provider is the remaining implementation.

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
channel. Implemented as follows:

- **Advertise** `ClipboardFileCopy(Vec<FileDescriptor>)` (the vendored
  `ServerEvent::ClipboardFileCopy` variant) when the file-list watch is
  non-empty — this populates the cliprdr server's `local_file_list`, which is
  what makes `FileContentsRequest`s be serviced instead of short-circuiting
  with `CB_RESPONSE_FAIL`. `client_capabilities` advertises
  `STREAM_FILECLIP_ENABLED` (required for file paste).
- **SIZE** (`FileContentsFlags::SIZE`): answered **synchronously** from the
  file-list watch — the size is already known, no bridge round-trip.
- **RANGE** (`FileContentsFlags::RANGE`): spawns a background task that
  (1) sends `FILE_CONTENT_REQUEST` via `AnlandBridge::request_file_content`
  with `request_id = stream_id`; (2) awaits the matching
  `FILE_CONTENT_RESPONSE` on the shared `Arc<Mutex<mpsc::Receiver<_>>>` (a 5 s
  timeout guards a vanished Android side); (3) replies with
  `ClipboardMessage::SendFileContentsResponse` (`new_data_response`, or
  `new_error` on empty/EOF).
- Direction: **Android → Windows only** (copy a file on Android → paste into
  mstsc), mirroring macrdp's Mac→Windows-only model.

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
