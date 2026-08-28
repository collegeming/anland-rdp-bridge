//! Private, authenticated anland bridge transport.
//!
//! The anland bridge is a Unix-domain socket (default
//! `/run/anland-rdp/bridge.sock`) carrying already-encoded H.264 AVC420
//! frames, clipboard text, and input between the RDP server (this process,
//! under Droidspaces Linux) and the Android `MediaCodec` anland consumer. It
//! is **never** exposed to the LAN; Windows still uses standard RDP/TLS/EGFX.
//!
//! This module is a fresh, permissively-licensed (MIT OR Apache-2.0)
//! reimplementation of the wire protocol specified in the predecessor
//! `lamco-anland-bridge/docs/anland-bridge.md`. No BSL-licensed source is
//! carried over — only the protocol facts (framing, HMAC derivation, message
//! types, handshake order, the fixed test vector) are reimplemented.
//!
//! ## Module split
//!
//! - [`wire`] — frame framing (`[u32 BE length][u8 type][payload]`), message
//!   type constants, and the typed `VideoFramePayload` decode.
//! - [`auth`] — HMAC-SHA256 mutual authentication v1: session-key derivation,
//!   server/client proof, the four-step handshake, and the fixed test vector.
//! - `transport` (pending) — the hardened Unix-socket listener (ancestor-chain
//!   validation, `0700` parent, `0600` socket, `.lock` + `flock`, stale-socket
//!   revalidation) and the accept loop.
//! - `bridge` (pending) — the orchestrator that spawns the listener, runs the
//!   handshake, and produces inbound channels (video frames, clipboard) plus
//!   outbound control (start/stop/idr/input/clipboard).

pub mod auth;
pub mod bridge;
pub mod transport;
pub mod wire;

pub use bridge::{AnlandBridge, AnlandBridgeInbound};
