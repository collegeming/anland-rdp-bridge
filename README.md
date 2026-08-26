# anland-rdp-bridge

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

A low-latency, low-power RDP server for the **anland** wireless-display path:
it streams an already-encoded **Android `MediaCodec` H.264** desktop to a
standard Windows `mstsc` client (or any standard RDP client) over RDP/TLS with
EGFX AVC420, bidirectional text + image + file clipboard (CLIPRDR), RDPSND
audio (PCM, optionally AAC), keyboard/mouse/wheel input, and optional
drive redirection (RDPDR). No custom client — only `mstsc`.

It runs on **Arch Linux ARM under Droidspaces**, sourcing frames from the
Android `MediaCodec` surface encoder over a private, authenticated Unix socket
bridge. It does not capture the Android screen and contains no software
H.264 fallback.

## Status

**Pre-alpha scaffolding.** This repository is a fork of
[`clintcan/macrdp`](https://github.com/clintcan/macrdp) (MIT OR Apache-2.0),
repurposed as the permissively-licensed base for the anland scenario. The
macOS backends inherited from macrdp are preserved behind a platform
abstraction layer; the anland / Linux / Android backends are stubs pending
wiring. Nothing here drives a real session yet — see the roadmap in
[CLAUDE.md](CLAUDE.md).

## Why fork macrdp instead of the BSL `lamco-rdp-server`?

macrdp is the permissively-licensed (MIT OR Apache-2.0) Rust RDP server that
already implements EGFX AVC420 hardware H.264, RDPSND (PCM + AAC), CLIPRDR
(text + image + file), drive redirection, and RDP-UDP multitransport — built
entirely on **upstream Devolutions IronRDP** (git rev `a5d1c682`), with no
dependency on the `lamco-admin/IronRDP` fork and no Business Source License
code anywhere. Forking macrdp as the base avoids the BUSL-1.1 restrictions
(no Change Date wait, no Competitive-Use / single-instance limits) while
reusing a tested RDP pipeline.

The anland scenario diverges from macrdp in two places: the frame source is
**already-encoded** Android `MediaCodec` Annex-B (macrdp captures raw BGRA
then encodes via VideoToolbox), and the target is Arch Linux ARM, not macOS.
Both are handled behind the `src/platform/` abstraction.

## Relationship to upstream

| Repository | Role |
|---|---|
| [`clintcan/macrdp`](https://github.com/clintcan/macrdp) | Code source; permissively-licensed Rust RDP server this is forked from |
| [`Devolutions/IronRDP`](https://github.com/Devolutions/IronRDP) | The RDP protocol crates macrdp (and this fork) build on, pinned at rev `a5d1c682` |
| `vendor/ironrdp-*` | Local patches to IronRDP crates macrdp carries (audio dispatch, SuppressOutput, RDPDR server-direction, DVC Soft-Sync, RDP-UDP) — kept verbatim |

This repository is **not** a macrdp release and is **not** affiliated with
macrdp or IronRDP. The MIT OR Apache-2.0 license is retained unchanged; the
macrdp and IronRDP copyright notices in `LICENSE-MIT` / `LICENSE-APACHE` and
the vendored crate `LICENSE-*` files are preserved.

## Data path

```text
ANiri EGL  →  anland / Android-native buffers
                │
                ▼
Android consumer
   ├─ Local:  Android Surface
   ├─ Remote: MediaCodec input Surface → hardware H.264 (Annex-B)
   └─ Both:   GPU fan-out → local Surface + MediaCodec Surface
                                       │
                                       ▼
/data/local/tmp/anland-rdp/bridge.sock   Android namespace
                 ║  bind-mounted directory
/run/anland-rdp/bridge.sock              Droidspaces mount namespace
                 │  authenticated local wire (HMAC-SHA256 mutual auth)
                 ▼
anland-rdp-bridge
   ├─ RDP TLS 1.3
   ├─ EGFX AVC420
   ├─ CLIPRDR  (text + image + file)
   ├─ RDPSND   (PCM, optional AAC)
   ├─ RDPDR    (optional drive redirection)
   └─ keyboard / mouse / wheel → anland native input
                 │
                 ▼
             Windows mstsc
```

## mstsc keyboard shortcut combos

Win-key shortcuts (`Win+T` → niri `Mod+T`, `Win+D`, …) only reach the remote
desktop when **mstsc** is set to forward them. In the client: *Local
Resources → Keyboard → Apply key combinations to* must be **"On the remote
computer"** (the default is *"Only when using the full screen"*, which keeps
non-fullscreen Win combos local to Windows — this looks exactly like a server
input bug but is client-side). The server itself translates RDP Set 1
scancodes to Linux evdev keycodes correctly, including the Win/Meta key in
both its extended and non-extended forms (`src/server/evdev_scancodes.rs`).

## Build

The anland target is Arch Linux ARM (aarch64). The macOS build path inherited
from macrdp still compiles (its backends keep their direct wiring); the
anland / Linux path is the one under active development.

```bash
# Arch Linux ARM (the anland target)
cargo build --release

# macOS (inherited, unchanged wiring — kept for reference / cross-check)
cargo build --release
```

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE)
at your option. The vendored `ironrdp-*` crates under `vendor/` retain their
own MIT OR Apache-2.0 licenses and copyright notices.
