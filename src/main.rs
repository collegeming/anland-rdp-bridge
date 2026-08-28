// anland-rdp-bridge: the anland / Linux / Android RDP server. The macOS fork
// is gone; this entry point drives the anland Linux RDP server (platform +
// anland_bridge + server) once.

#[cfg(test)]
use ironrdp_pdu::rdp::capability_sets::{BitmapCodecs, Codec, CodecProperty, NsCodec, RemoteFxContainer};

#[cfg(test)]
mod conn_test;

mod anland_bridge;
mod platform;
mod server;

/// Codecs advertised to the client (kept for the conn_test protocol checks;
/// anland's RAF/raster path isn't driven by them, but the negotiation starts
/// from this advertised set).
#[cfg(test)]
fn bitmap_codecs() -> BitmapCodecs {
    BitmapCodecs(vec![
        Codec {
            id: 0,
            property: CodecProperty::NsCodec(NsCodec {
                is_dynamic_fidelity_allowed: false,
                is_subsampling_allowed: false,
                color_loss_level: 3,
            }),
        },
        Codec {
            id: 1,
            property: CodecProperty::RemoteFx(RemoteFxContainer::ServerContainer(1)),
        },
        Codec {
            id: 2,
            property: CodecProperty::ImageRemoteFx(RemoteFxContainer::ServerContainer(1)),
        },
        Codec {
            id: 3,
            property: CodecProperty::Qoi,
        },
        Codec {
            id: 4,
            property: CodecProperty::QoiZ,
        },
    ])
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    anland_entry::run().await
}

mod anland_entry {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use anyhow::{Context, Result};
    use tracing::{error, info, warn};
    use tracing_subscriber::EnvFilter;

    use crate::anland_bridge::transport::BridgeEndpoint;
    use crate::server::{AnlandRdpServer, AnlandServerConfig};

    /// Default cert/key location: ~/.local/share/anland-rdp/.
    fn default_cert_dir() -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home).join(".local/share/anland-rdp"))
    }

    /// Generate a self-signed cert + key (rcgen, ECDSA P-256) if the files are
    /// absent; otherwise load the existing pair. Returns (cert_path, key_path).
    fn ensure_cert() -> Result<(PathBuf, PathBuf)> {
        let dir = default_cert_dir()?;
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        if cert_path.exists() && key_path.exists() {
            return Ok((cert_path, key_path));
        }
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create cert dir {}", dir.display()))?;
        let ck = rcgen::generate_simple_self_signed(vec!["anland-rdp".to_string()])?;
        std::fs::write(&cert_path, ck.cert.pem())?;
        std::fs::write(&key_path, ck.key_pair.serialize_pem())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
        info!(cert = %cert_path.display(), "generated self-signed TLS certificate");
        Ok((cert_path, key_path))
    }

    /// 32-hex token → 16 bytes. Unset → generate one and print it so the
    /// operator can mirror it to the Android consumer.
    fn ensure_bridge_token() -> Result<Vec<u8>> {
        if let Ok(hex) = std::env::var("ANLAND_BRIDGE_TOKEN") {
            let hex = hex.trim().to_ascii_lowercase();
            anyhow::ensure!(
                hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit()),
                "ANLAND_BRIDGE_TOKEN must be 32 lowercase hex chars"
            );
            let mut bytes = Vec::with_capacity(16);
            for i in (0..hex.len()).step_by(2) {
                bytes.push(u8::from_str_radix(&hex[i..i + 2], 16)?);
            }
            return Ok(bytes);
        }
        let mut buf = [0u8; 16];
        getrandom::getrandom(&mut buf).map_err(|e| anyhow::anyhow!("token RNG: {e}"))?;
        let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        let dir = default_cert_dir()?;
        std::fs::create_dir_all(&dir).ok();
        let _ = std::fs::write(dir.join("bridge.token"), &hex);
        warn!("generated new bridge token (16 bytes): {hex}");
        warn!("mirror this token to the Android anland consumer");
        Ok(buf.to_vec())
    }

    pub async fn run() -> Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .init();

        let listen_addr: SocketAddr = std::env::var("ANLAND_LISTEN")
            .unwrap_or_else(|_| "127.0.0.1:3389".to_string())
            .parse()
            .context("ANLAND_LISTEN must be a socket address")?;

        let endpoint_str = std::env::var("ANLAND_BRIDGE_ENDPOINT")
            .unwrap_or_else(|_| "/run/anland-rdp/bridge.sock".to_string());
        let bridge_endpoint = BridgeEndpoint::parse(&endpoint_str)
            .with_context(|| format!("parse bridge endpoint {endpoint_str:?}"))?;

        let width: u16 = std::env::var("ANLAND_WIDTH")
            .unwrap_or_else(|_| "1280".into())
            .parse()
            .context("ANLAND_WIDTH")?;
        let height: u16 = std::env::var("ANLAND_HEIGHT")
            .unwrap_or_else(|_| "720".into())
            .parse()
            .context("ANLAND_HEIGHT")?;
        let fps: u8 = std::env::var("ANLAND_FPS")
            .unwrap_or_else(|_| "30".into())
            .parse()
            .context("ANLAND_FPS")?;
        // Upper bound for a client-requested MS-RDPEDISP resize (niri modes top
        // out at 4096x2160 by default).
        let max_width: u16 = std::env::var("ANLAND_MAX_WIDTH")
            .unwrap_or_else(|_| "4096".into())
            .parse()
            .context("ANLAND_MAX_WIDTH")?;
        let max_height: u16 = std::env::var("ANLAND_MAX_HEIGHT")
            .unwrap_or_else(|_| "2160".into())
            .parse()
            .context("ANLAND_MAX_HEIGHT")?;

        let (cert_path, key_path) = ensure_cert()?;
        let bridge_token = ensure_bridge_token()?;

        let config = AnlandServerConfig {
            listen_addr,
            cert_path,
            key_path,
            bridge_endpoint,
            bridge_token,
            width,
            height,
            fps,
            max_width,
            max_height,
        };

        let mut server = AnlandRdpServer::new(&config).context("init anland RDP server")?;
        let shutdown = server.shutdown_sender();

        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            warn!("Ctrl-C received; shutting down");
            let _ = shutdown.send(());
        });

        info!(addr = %config.listen_addr, "anland-rdp-bridge running; connect with mstsc");
        server.run().await.map_err(|e| {
            error!("anland RDP server exited: {e:#}");
            e
        })?;
        Ok(())
    }
}
