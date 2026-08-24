//! HMAC-SHA256 mutual authentication v1 for the anland bridge.
//!
//! Both sides share a 128-bit token (displayed as 32 lowercase hex chars,
//! decoded to 16 bytes). The raw hex text and decoded bytes are never sent on
//! the wire. Each connection derives a session key from the token, both sides
//! contribute independent 32-byte nonces, and each side proves possession of
//! the token by producing a role- and transcript-bound HMAC that the other
//! verifies in constant time.
//!
//! ## Derivation (`||` is concatenation)
//!
//! ```text
//! session_key = HMAC-SHA256(token_bytes, KDF_CONTEXT)
//! transcript  = MAGIC || version || android_nonce[32] || rust_nonce[32]
//! server_hmac = HMAC-SHA256(session_key, SERVER_CONTEXT || transcript)
//! client_hmac = HMAC-SHA256(session_key, CLIENT_CONTEXT || transcript)
//! ```
//!
//! Rust proves possession first (sends `AUTH_SERVER_PROOF` before reading the
//! client proof), so an unauthenticated listener never receives the client's
//! proof or the bearer token. A proof is role-bound (server vs client context)
//! and transcript-bound (fresh nonces per connection), so it cannot be
//! reflected between roles or replayed under fresh nonces.
//!
//! ## Handshake order
//!
//! | Order | Type | Direction | Payload |
//! |---:|---|---|---|
//! | 1 | `AUTH_INIT`          | Android → Rust | `version:u8 \|\| android_nonce[32]` |
//! | 2 | `AUTH_SERVER_PROOF`  | Rust → Android | `version:u8 \|\| rust_nonce[32] \|\| server_hmac[32]` |
//! | 3 | `AUTH_CLIENT_PROOF`  | Android → Rust | `version:u8 \|\| client_hmac[32]` |
//! | 4 | `AUTH_OK`            | Rust → Android | `version:u8` |
//!
//! Any malformed frame, wrong version, proof mismatch, timeout, or pre-auth
//! application message closes the accepted socket. No input, clipboard,
//! stream, IDR, or video is processed before `AUTH_OK`.

use std::time::Duration;

use anyhow::{bail, ensure, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;

use super::wire::{self, msg};

type HmacSha256 = Hmac<Sha256>;

/// Exact ASCII constants (these byte strings are part of the wire protocol).
const MAGIC: &[u8] = b"ANLB";
const KDF_CONTEXT: &[u8] = b"ANLAND-BRIDGE-HMAC-SHA256-v1";
const SERVER_CONTEXT: &[u8] = b"ANLAND-BRIDGE-SERVER-v1";
const CLIENT_CONTEXT: &[u8] = b"ANLAND-BRIDGE-ANDROID-v1";

/// Protocol version (carried in every auth frame and the `AUTH_OK` ack).
pub const VERSION: u8 = 1;

/// Per-step handshake timeout. The spec applies a 5 s budget to each of:
/// `AUTH_INIT` read, server-proof write, client-proof read, `AUTH_OK` write.
const HANDSHAKE_STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// Nonce length (32 bytes, cryptographically secure random).
pub const NONCE_LEN: usize = 32;

/// Tag length (HMAC-SHA256 output = 32 bytes).
const TAG_LEN: usize = 32;

/// Derive the session key: `HMAC-SHA256(token_bytes, KDF_CONTEXT)`.
pub fn session_key(token: &[u8]) -> [u8; TAG_LEN] {
    let mut mac = HmacSha256::new_from_slice(token).expect("HMAC accepts any key length");
    mac.update(KDF_CONTEXT);
    finalize_tag(&mut mac)
}

/// Build the transcript: `MAGIC || version || android_nonce || rust_nonce`.
fn transcript(android_nonce: &[u8; NONCE_LEN], rust_nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    let mut t = Vec::with_capacity(MAGIC.len() + 1 + NONCE_LEN + NONCE_LEN);
    t.extend_from_slice(MAGIC);
    t.push(VERSION);
    t.extend_from_slice(android_nonce);
    t.extend_from_slice(rust_nonce);
    t
}

/// Server proof: `HMAC-SHA256(session_key, SERVER_CONTEXT || transcript)`.
pub fn server_proof(
    session_key: &[u8; TAG_LEN],
    android_nonce: &[u8; NONCE_LEN],
    rust_nonce: &[u8; NONCE_LEN],
) -> [u8; TAG_LEN] {
    let mut mac = HmacSha256::new_from_slice(session_key).expect("HMAC accepts any key length");
    mac.update(SERVER_CONTEXT);
    mac.update(&transcript(android_nonce, rust_nonce));
    finalize_tag(&mut mac)
}

/// Client proof: `HMAC-SHA256(session_key, CLIENT_CONTEXT || transcript)`.
pub fn client_proof(
    session_key: &[u8; TAG_LEN],
    android_nonce: &[u8; NONCE_LEN],
    rust_nonce: &[u8; NONCE_LEN],
) -> [u8; TAG_LEN] {
    let mut mac = HmacSha256::new_from_slice(session_key).expect("HMAC accepts any key length");
    mac.update(CLIENT_CONTEXT);
    mac.update(&transcript(android_nonce, rust_nonce));
    finalize_tag(&mut mac)
}

/// Verify a received client proof in constant time. Returns `Err` on mismatch.
pub fn verify_client_proof(
    session_key: &[u8; TAG_LEN],
    android_nonce: &[u8; NONCE_LEN],
    rust_nonce: &[u8; NONCE_LEN],
    received: &[u8],
) -> Result<()> {
    ensure!(
        received.len() == TAG_LEN,
        "anland bridge: client proof must be {TAG_LEN} bytes, got {}",
        received.len()
    );
    let expected = client_proof(session_key, android_nonce, rust_nonce);
    if !ct_eq(&expected, received) {
        bail!("anland bridge: client proof mismatch");
    }
    Ok(())
}

/// Run the server side of the handshake over an already-accepted stream.
///
/// Returns the negotiated version on success. Any malformed frame, wrong
/// version, proof mismatch, timeout, or pre-auth application message is an
/// error — the caller closes the socket on any error path.
pub async fn run_server_handshake<S>(
    stream: &mut S,
    token: &[u8],
) -> Result<u8>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let sk = session_key(token);

    // 1. Read AUTH_INIT: version:u8 || android_nonce[32].
    let init = timeout(HANDSHAKE_STEP_TIMEOUT, wire::read_frame(stream))
        .await
        .map_err(|_| anyhow::anyhow!("anland bridge: AUTH_INIT timed out"))??;
    ensure!(
        init.msg_type == msg::AUTH_INIT,
        "anland bridge: expected AUTH_INIT, got type {}",
        init.msg_type
    );
    ensure!(
        init.payload.len() == 1 + NONCE_LEN,
        "anland bridge: AUTH_INIT payload must be {} bytes, got {}",
        1 + NONCE_LEN,
        init.payload.len()
    );
    let version = init.payload[0];
    ensure!(
        version == VERSION,
        "anland bridge: AUTH_INIT version {version} != {VERSION}"
    );
    let mut android_nonce = [0u8; NONCE_LEN];
    android_nonce.copy_from_slice(&init.payload[1..]);

    // 2. Generate rust_nonce, send AUTH_SERVER_PROOF: version || rust_nonce || server_hmac.
    let mut rust_nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut rust_nonce)
        .map_err(|e| anyhow::anyhow!("anland bridge: nonce generation failed: {e}"))?;
    let proof = server_proof(&sk, &android_nonce, &rust_nonce);
    let mut server_payload = Vec::with_capacity(1 + NONCE_LEN + TAG_LEN);
    server_payload.push(VERSION);
    server_payload.extend_from_slice(&rust_nonce);
    server_payload.extend_from_slice(&proof);
    timeout(HANDSHAKE_STEP_TIMEOUT, wire::write_frame(stream, msg::AUTH_SERVER_PROOF, &server_payload))
        .await
        .map_err(|_| anyhow::anyhow!("anland bridge: AUTH_SERVER_PROOF write timed out"))??;

    // 3. Read AUTH_CLIENT_PROOF: version:u8 || client_hmac[32].
    let cp = timeout(HANDSHAKE_STEP_TIMEOUT, wire::read_frame(stream))
        .await
        .map_err(|_| anyhow::anyhow!("anland bridge: AUTH_CLIENT_PROOF timed out"))??;
    ensure!(
        cp.msg_type == msg::AUTH_CLIENT_PROOF,
        "anland bridge: expected AUTH_CLIENT_PROOF, got type {}",
        cp.msg_type
    );
    ensure!(
        cp.payload.len() == 1 + TAG_LEN,
        "anland bridge: AUTH_CLIENT_PROOF payload must be {} bytes, got {}",
        1 + TAG_LEN,
        cp.payload.len()
    );
    ensure!(
        cp.payload[0] == VERSION,
        "anland bridge: AUTH_CLIENT_PROOF version {} != {VERSION}",
        cp.payload[0]
    );
    verify_client_proof(&sk, &android_nonce, &rust_nonce, &cp.payload[1..])?;

    // 4. Send AUTH_OK: version:u8.
    timeout(HANDSHAKE_STEP_TIMEOUT, wire::write_frame(stream, msg::AUTH_OK, &[VERSION]))
        .await
        .map_err(|_| anyhow::anyhow!("anland bridge: AUTH_OK write timed out"))??;

    Ok(VERSION)
}

/// Finalize an HMAC into a fixed-length tag. Uses `finalize_reset` (which
/// borrows) so callers that hold `&mut HmacSha256` can finalize without
/// giving up ownership.
fn finalize_tag(mac: &mut HmacSha256) -> [u8; TAG_LEN] {
    let bytes = mac.finalize_reset().into_bytes();
    let mut out = [0u8; TAG_LEN];
    out.copy_from_slice(&bytes);
    out
}

/// Constant-time equality. Lengths must match; returns false immediately on
/// length mismatch (which is safe — the length is not secret here).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hex-decode a lowercase string into bytes (test helper).
    fn hex_to_bytes(h: &str) -> Vec<u8> {
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Hex-encode bytes into a lowercase string (test helper).
    fn hex_encode(b: &[u8]) -> String {
        let mut s = String::with_capacity(b.len() * 2);
        for byte in b {
            s.push_str(&format!("{byte:02x}"));
        }
        s
    }

    /// Fixed test vector from the protocol spec — pins the exact derivation
    /// so a future algorithm change fails in CI.
    #[test]
    fn spec_test_vector() {
        let token = hex_to_bytes("00112233445566778899aabbccddeeff");
        let android_nonce = [0x11u8; NONCE_LEN];
        let rust_nonce = [0x22u8; NONCE_LEN];

        let sk = session_key(&token);
        assert_eq!(
            hex_encode(&sk),
            "fa806927e7b10cd06c481c74a81503cff44da67661f5deeb8c3dcfb5dbdc04ae",
            "session_key mismatch"
        );

        let sp = server_proof(&sk, &android_nonce, &rust_nonce);
        assert_eq!(
            hex_encode(&sp),
            "97af20e18477ea22f34f97eeb792b8c258b0e100b6d3ffffbe94257a059a3763",
            "server_hmac mismatch"
        );

        let cp = client_proof(&sk, &android_nonce, &rust_nonce);
        assert_eq!(
            hex_encode(&cp),
            "0d479895e8d397f7c3ed60ec3fba4dc3e1cf8316ec1ee402189ce97a27912143",
            "client_hmac mismatch"
        );

        // A valid client proof verifies; a tampered tag does not.
        verify_client_proof(&sk, &android_nonce, &rust_nonce, &cp).unwrap();
        let mut bad = cp;
        bad[0] ^= 0xff;
        assert!(verify_client_proof(&sk, &android_nonce, &rust_nonce, &bad).is_err());
    }

    /// The server and client contexts are distinct — a server proof must NOT
    /// verify as a client proof (role-binding).
    #[test]
    fn proofs_are_role_bound() {
        let token = hex_to_bytes("00112233445566778899aabbccddeeff");
        let android_nonce = [0x11u8; NONCE_LEN];
        let rust_nonce = [0x22u8; NONCE_LEN];
        let sk = session_key(&token);

        let sp = server_proof(&sk, &android_nonce, &rust_nonce);
        // The server proof must not be accepted as a client proof.
        assert!(verify_client_proof(&sk, &android_nonce, &rust_nonce, &sp).is_err());
    }

    /// A transcript under fresh nonces makes a stale proof invalid
    /// (replay resistance).
    #[test]
    fn proofs_are_transcript_bound() {
        let token = hex_to_bytes("00112233445566778899aabbccddeeff");
        let android_nonce = [0x11u8; NONCE_LEN];
        let rust_nonce = [0x22u8; NONCE_LEN];
        let sk = session_key(&token);
        let cp = client_proof(&sk, &android_nonce, &rust_nonce);

        // Change the android nonce: the old proof must no longer verify.
        let mut android_nonce2 = android_nonce;
        android_nonce2[0] ^= 0xff;
        assert!(verify_client_proof(&sk, &android_nonce2, &rust_nonce, &cp).is_err());
    }

    #[test]
    fn ct_eq_basic() {
        assert!(ct_eq(&[1, 2, 3], &[1, 2, 3]));
        assert!(!ct_eq(&[1, 2, 3], &[1, 2, 4]));
        assert!(!ct_eq(&[1, 2, 3], &[1, 2]));
    }
}
