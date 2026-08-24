# CLAUDE.md

This file provides guidance to AI agents working with code in this repository.

> Layout note: this is a lean index. Long-form reference material inherited
> from macrdp lives under `docs/` and the vendored `vendor/ironrdp-*/CLAUDE.md`
> divergence logs; load them only when working in those areas.

## What this project is

`anland-rdp-bridge` is a low-latency RDP server for the anland wireless-display
path: it streams an already-encoded **Android `MediaCodec` H.264** desktop to a
standard Windows `mstsc` client over RDP/TLS with EGFX AVC420, bidirectional
text + image + file clipboard, RDPSND audio (PCM, optional AAC),
keyboard/mouse/wheel input, and optional drive redirection. It runs on
**Arch Linux ARM under Droidspaces**, sourcing frames over a private,
authenticated Unix socket bridge. No custom client; no software H.264 fallback.

## Fork lineage & license

This is a fork of [`clintcan/macrdp`](https://github.com/clintcan/macrdp)
(MIT OR Apache-2.0), repurposed as the permissively-licensed base for anland.
macrdp builds entirely on **upstream Devolutions IronRDP** (git rev
`a5d1c682`) — it has **no** dependency on `lamco-admin/IronRDP` and **no**
Business Source License code. Forking macrdp avoids BUSL-1.1 entirely. The
license is retained unchanged (MIT OR Apache-2.0); macrdp and IronRDP copyright
notices in `LICENSE-*` and `vendor/*/LICENSE-*` are preserved.

## Why macrdp as the base (not `lamco-rdp-server`)

macrdp already implements, under a permissive license, every RDP-layer feature
anland needs: EGFX AVC420 hardware H.264, RDPSND (PCM + AAC), CLIPRDR
(text + image + file), drive redirection, and RDP-UDP multitransport. The RDP
protocol logic is platform-independent and reusable; only the **capture /
encode / input backends** are macOS-specific and need replacing with Android /
Linux equivalents. Crucially, anland's `MediaCodec` emits **Annex-B natively**
— the exact framing mstsc's decoder requires (macrdp verified this empirically
2026-05-20 and had to convert VideoToolbox's AVCC to Annex-B). So anland gets
the correct format for free and **skips the encode step entirely**.

## Architecture: what is inherited vs. what changes

### Inherited verbatim (keep)

- `vendor/ironrdp-*` — local IronRDP patches (audio dispatch, SuppressOutput,
  RDPDR server-direction, DVC Soft-Sync, RDP-UDP). Do not touch unless bumping
  the IronRDP pin.
- `Cargo.toml` `[patch.crates-io]` + `[patch."...IronRDP.git"]` block — the
  proven-compatible dependency wiring to upstream rev `a5d1c682`. Do not
  re-pin without re-verifying the vendored divergences still apply.
- The EGFX **ship side** of `src/h264.rs` (`ship_frames` / `ship_loop` / the
  RTT + standing-queue + no-ack-fallback backpressure state machine / the
  `GfxHandler` impl). This is the crown jewel — platform-independent, reused
  as-is once the macOS `cfg` gate is lifted.
- `src/audio.rs` RDPSND protocol layer, `src/clipboard.rs` CLIPRDR protocol
  layer + PNG↔DIB conversion, `src/avc444.rs`, `src/multitransport.rs`,
  `src/keyboard_layout.rs`.

### Replaced (macOS backend → anland / Linux / Android)

- `src/videotoolbox.rs` + the encode side of `src/h264.rs`
  (`submit_bgra` / `Encoder::encode_bgra`) — **deleted**. anland receives
  already-encoded Annex-B; no encode step.
- `src/capture.rs` (ScreenCaptureKit) → anland Unix-socket frame source.
- `src/input.rs` (CGEventPost) → anland native input via the bridge.
- `src/audio.rs` capture (ScreenCaptureKit audio tap) → Android `AudioRecord`.
- `src/aac.rs` (AudioToolbox) → Android `MediaCodec` AAC encoder.
- `src/clipboard.rs` data source (NSPasteboard) → Android clipboard manager +
  file provider.
- `src/auth.rs` / `src/auth_guard.rs` (macOS PAM) → anland HMAC token bridge
  + optional RDP credentials.

### Trimmed (macOS desktop-product features anland does not need)

- `src/camera/`, `src/usb_redirect/`, `src/switcher_hud.rs`, `src/shield.rs`,
  `src/runloop_thread.rs`, `src/virtual_display/` (anland has its own display
  routing), `src/cursor/` (replaced), `gui/`, `ifd-handler/`,
  `src/rdpdr/smartcard.rs` (keep `src/rdpdr/surface.rs` for drive redirection).
- `packaging/`, `dist/`, `scripts/` (macOS packaging) → anland ARM64 build.

### Added (anland-specific)

- `src/platform/` — the platform abstraction layer (this is the contract the
  anland backends implement). Currently anland-only (compiled away on macOS so
  the inherited strict macOS `clippy -D warnings` gate is unaffected); the
  migration phase lifts it to be shared by both platforms.
- `src/anland_bridge/` (pending) — the private Unix socket bridge: HMAC-SHA256
  mutual auth, framing, video-frame / clipboard / input relay. Ported from the
  prior `lamco-anland-bridge` work but rewritten under the permissive license
  (no BSL code carried over).

## The platform abstraction contract (`src/platform/`)

- `EncodedVideoFrame` — the unified already-encoded frame type both backends
  produce; the EGFX ship side consumes it.
- `VideoFrameSource` — async pull stream of `EncodedVideoFrame` + control
  signals (`start` / `stop` / `request_keyframe`). anland's impl connects to
  `/run/anland-rdp/bridge.sock` and pulls `MediaCodec` frames.
- `AudioSource` — async pull stream of `AudioChunk` (PCM i16, or raw AAC-LC
  AU). anland's impl taps Android `AudioRecord` (+ optional `MediaCodec` AAC).
- `PlatformBackends` — the combined backend set; `select_backends()` returns
  the anland set on Linux.
- Clipboard / input / drive redirection are **not** re-abstracted: they already
  implement upstream ironrdp traits (`CliprdrBackend`, `RdpServerInputHandler`),
  so each platform just implements the same trait against a different data source.

## Roadmap (phases)

1. **Done** — fork + rename to `anland_rdp_bridge`; platform trait skeleton
   (`src/platform/`) + anland stubs; module tree wired.
2. **Next** — port the anland bridge (`src/anland_bridge/`): Unix socket
   listener with the hardening rules (ancestor-chain validation, `0700`
   parent, `0600` socket, `.lock` + `flock`, stale-socket revalidation),
   HMAC-SHA256 mutual auth v1, framing. Reuse the wire protocol verbatim from
   the prior `lamco-anland-bridge` `docs/anland-bridge.md` spec.
3. — Wire `AnlandVideoSource` to the bridge; feed `EncodedVideoFrame` into the
   inherited EGFX ship side. Lift `h264.rs`'s `#![cfg(target_os = "macos")]`
   gate to function-level so the ship path compiles on Linux.
4. — Wire `AnlandAudioSource` (Android `AudioRecord` + optional `MediaCodec`
   AAC) into the inherited RDPSND protocol layer.
5. — Wire clipboard (Android clipboard manager + file provider) and input
   (anland native input) into the inherited CLIPRDR / input-handler traits.
6. — Trim the macOS desktop-product modules; rewrite `src/main.rs` for the
   anland config + run path; set up the ARM64 release build.
7. — Cross-validation against `mstsc` end-to-end.

## Conventions to preserve

- Match macrdp's comment density and style: comments state *constraints the
  code can't show* or *the specific live failure a branch prevents* (with the
  date it was found) — never what the next line does.
- Keep `vendor/ironrdp-*/CLAUDE.md` divergence logs updated when touching
  vendored crates.
- Do not re-pin the IronRDP git rev (`a5d1c682`) without re-verifying every
  vendored divergence still applies / is still needed.
- `#![cfg_attr(not(target_os = "macos"), allow(dead_code, ...))]` in `main.rs`
  silences the Linux stub path; do not relax the macOS strict clippy gate.
- No BSL-licensed code is ever carried into this repository. If porting logic
  from `lamco-rdp-server` / `lamco-anland-bridge`, reimplement from the spec,
  do not copy BSL source.
