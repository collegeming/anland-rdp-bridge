//! Hardened Unix-socket listener for the anland bridge.
//!
//! The listener binds the configured endpoint and accepts one Android client
//! at a time, returning to `accept()` when it disconnects. The default
//! endpoint is a Unix domain socket (`/run/anland-rdp/bridge.sock`); an
//! optional `tcp://127.0.0.1:PORT` / `tcp://[::1]:PORT` compatibility mode
//! is loopback-only and rejected for non-loopback bind or peer.
//!
//! ## Hardening (Unix socket)
//!
//! - The full ancestor chain down to the socket's parent must contain only
//!   real directories (symlinks rejected).
//! - The final parent must be owned by the service euid with mode exactly
//!   `0700`; if missing and the ancestor chain is safe, it is created so.
//! - A mode-`0600` persistent `.lock` file beside the socket is opened with
//!   `O_NOFOLLOW|O_CLOEXEC|O_NONBLOCK|O_CREAT` and held by a nonblocking
//!   exclusive `flock(LOCK_EX|LOCK_NB)`, preventing a second listener.
//! - A stale socket is removed only after revalidation; a non-socket
//!   occupant (regular file or symlink) is rejected, never clobbered.
//! - After `bind` + `chmod 0600` the new socket is revalidated (owner, type,
//!   mode, device/inode). On shutdown the socket is removed only if its
//!   device/inode still match the object this process created.

use std::ffi::CString;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, ensure, Result};
use tokio::net::UnixListener;

/// A parsed bridge endpoint. Bare paths and `unix://` are Unix sockets;
/// `tcp://` is the loopback-only compatibility mode.
#[derive(Debug, Clone)]
pub enum BridgeEndpoint {
    Unix(PathBuf),
    Tcp(std::net::SocketAddr),
}

impl BridgeEndpoint {
    /// Parse an endpoint string. Rejects non-loopback TCP and wildcard binds
    /// (never `0.0.0.0` / `::` / a forwarded port).
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            bail!("anland bridge: empty endpoint");
        }
        if let Some(rest) = s.strip_prefix("unix://") {
            let p = PathBuf::from(rest);
            ensure!(p.is_absolute(), "anland bridge: unix endpoint must be absolute: {s}");
            return Ok(Self::Unix(p));
        }
        if let Some(rest) = s.strip_prefix("tcp://") {
            let addr: std::net::SocketAddr = rest
                .parse()
                .map_err(|e| anyhow!("anland bridge: bad tcp address {rest:?}: {e}"))?;
            ensure!(
                addr.ip().is_loopback(),
                "anland bridge: tcp endpoint must be loopback, got {addr}"
            );
            return Ok(Self::Tcp(addr));
        }
        // Bare path ⇒ Unix socket (must be absolute).
        let p = PathBuf::from(s);
        ensure!(p.is_absolute(), "anland bridge: endpoint must be absolute or tcp://: {s}");
        Ok(Self::Unix(p))
    }
}

/// Current effective uid.
fn euid() -> u32 {
    // Safety: geteuid is a pure query with no UB surface.
    unsafe { libc::geteuid() }
}

/// Validate that every ancestor of `parent` (strictly above it, down to and
/// including `/`) is a real directory (not a symlink, not a file). Returns
/// `true` if `parent` itself is missing and must be created; returns `false`
/// if `parent` exists and is already a real `0700` euid-owned directory.
/// A missing ancestor strictly above `parent` is an error (we never create
/// deep ancestors — only the final parent).
fn validate_listener_parent(parent: &Path) -> Result<bool> {
    ensure!(parent.is_absolute(), "anland bridge: socket parent must be absolute");
    // Ancestors strictly above `parent`.
    for ancestor in parent.ancestors().skip(1) {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                ensure!(
                    meta.is_dir() && !meta.file_type().is_symlink(),
                    "anland bridge: ancestor {ancestor:?} is not a real directory"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("anland bridge: missing required ancestor {ancestor:?}");
            }
            Err(e) => return Err(anyhow!("anland bridge: stat ancestor {ancestor:?}: {e}")),
        }
    }
    // The final parent itself.
    match fs::symlink_metadata(parent) {
        Ok(meta) => {
            ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "anland bridge: socket parent {parent:?} is not a real directory"
            );
            ensure!(
                meta.uid() == euid(),
                "anland bridge: socket parent {parent:?} owned by uid {}, not euid {}",
                meta.uid(),
                euid()
            );
            ensure!(
                meta.mode() & 0o777 == 0o700,
                "anland bridge: socket parent {parent:?} mode {:o}, must be 0700",
                meta.mode() & 0o777
            );
            Ok(false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(anyhow!("anland bridge: stat socket parent {parent:?}: {e}")),
    }
}

/// Create `parent` (a single missing directory) with mode `0700`, then
/// re-stat to verify ownership and mode (umask may tighten, never loosen).
fn create_listener_parent(parent: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(parent)
        .map_err(|e| anyhow!("anland bridge: failed to create listener dir {parent:?}: {e}"))?;
    let meta = fs::symlink_metadata(parent)?;
    ensure!(
        meta.is_dir() && meta.uid() == euid() && meta.mode() & 0o777 == 0o700,
        "anland bridge: created dir {parent:?} failed revalidation (mode {:o})",
        meta.mode() & 0o777
    );
    Ok(())
}

/// Path of the persistent `.lock` file beside a socket (same name, `.lock`
/// extension).
fn lock_path_for(socket_path: &Path) -> PathBuf {
    let mut p = socket_path.to_path_buf();
    p.set_extension("lock");
    p
}

/// A held exclusive `flock` on the `.lock` file. When dropped, the fd closes
/// and the lock releases. The `.lock` file itself persists (it is reused).
struct LockGuard {
    _fd: OwnedFd,
}

impl LockGuard {
    /// Open `<socket_path>.lock` with `O_NOFOLLOW|O_CLOEXEC|O_NONBLOCK|O_CREAT`
    /// and acquire a nonblocking exclusive flock. Rejects a symlink at the
    /// lock path; sets the file mode to `0600` (in case it pre-existed with
    /// looser bits).
    fn acquire(socket_path: &Path) -> Result<Self> {
        let lock_path = lock_path_for(socket_path);
        let cpath = CString::new(lock_path.as_os_str().as_encoded_bytes())
            .map_err(|_| anyhow!("anland bridge: lock path contains NUL"))?;
        const FLAGS: i32 =
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        // Safety: open(2) with a NUL-terminated path and a valid mode.
        let fd = unsafe { libc::open(cpath.as_ptr(), FLAGS, 0o600) };
        if fd < 0 {
            return Err(anyhow!(
                "anland bridge: open lock {lock_path:?} failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        // chmod 0600 in case the lock pre-existed with looser bits; only the
        // euid owner may do so.
        let meta = fs::metadata(&lock_path)?;
        if meta.mode() & 0o777 != 0o600 {
            ensure!(
                meta.uid() == euid(),
                "anland bridge: lock {lock_path:?} owned by uid {}, not euid",
                meta.uid()
            );
            fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))?;
        }
        // Nonblocking exclusive flock; LOCK_NB so a second listener fails fast.
        // Safety: flock(2) on a valid owned fd; the fd stays alive in `owned`.
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        ensure!(
            rc == 0,
            "anland bridge: another listener already holds the lock on {lock_path:?}"
        );
        Ok(Self { _fd: owned })
    }
}

/// Validate the existing occupant at `path` and, if it is a stale socket from
/// a previous run, remove it. Rejects a regular file or symlink (never
/// clobbered).
fn prepare_stale_socket(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow!("anland bridge: stat socket {path:?}: {e}")),
        Ok(meta) => {
            ensure!(
                !meta.file_type().is_symlink(),
                "anland bridge: refusing to bind over a symlink at {path:?}"
            );
            if meta.file_type().is_socket() {
                fs::remove_file(path)?;
                Ok(())
            } else {
                bail!("anland bridge: {path:?} is not a Unix socket; refusing to clobber a regular file")
            }
        }
    }
}

/// A bound, hardened Unix listener plus the lock guard keeping it exclusive.
pub struct BridgeListener {
    listener: UnixListener,
    socket_path: PathBuf,
    _lock: LockGuard,
    /// (device, inode) of the socket this process created, for shutdown
    /// revalidation.
    created_dev: u64,
    created_ino: u64,
}

impl BridgeListener {
    /// Bind a hardened Unix listener at `path`. Performs the full hardening
    /// (ancestor chain, `0700` parent, `.lock` + `flock`, stale-socket
    /// revalidation, post-bind `chmod 0600` + revalidation). Async because
    /// `tokio::net::UnixListener::bind` registers with the I/O reactor, which
    /// requires a runtime context.
    pub async fn bind_unix(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("anland bridge: socket path has no parent: {path:?}"))?;
        // 1. Ancestor chain + final parent (create only the final parent).
        if validate_listener_parent(parent)? {
            create_listener_parent(parent)?;
        }
        // 2. Acquire the exclusive .lock before touching the socket.
        let lock = LockGuard::acquire(path)?;
        // 3. Remove a stale socket (or refuse a non-socket occupant).
        prepare_stale_socket(path)?;
        // 4. Bind. std applies umask; chmod right after to force 0600.
        let listener = UnixListener::bind(path)
            .map_err(|e| anyhow!("anland bridge: bind {path:?} failed: {e}"))?;
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow!("anland bridge: chmod 0600 on {path:?} failed: {e}"))?;
        // 5. Revalidate the bound socket: owner, type, mode.
        let meta = fs::metadata(path)?;
        ensure!(
            meta.file_type().is_socket(),
            "anland bridge: bound path {path:?} is not a socket after bind"
        );
        ensure!(
            meta.uid() == euid(),
            "anland bridge: bound socket {path:?} owned by uid {}, not euid",
            meta.uid()
        );
        ensure!(
            meta.mode() & 0o777 == 0o600,
            "anland bridge: bound socket {path:?} mode {:o}, must be 0600",
            meta.mode() & 0o777
        );
        Ok(Self {
            listener,
            socket_path: path.to_path_buf(),
            _lock: lock,
            created_dev: meta.dev(),
            created_ino: meta.ino(),
        })
    }

    /// Accept one client connection.
    pub async fn accept(&self) -> Result<tokio::net::UnixStream> {
        let (stream, _) = self
            .listener
            .accept()
            .await
            .map_err(|e| anyhow!("anland bridge: accept failed: {e}"))?;
        Ok(stream)
    }

    /// On shutdown, remove the socket only if its device/inode still match
    /// the object this process created (a replacement listener may have
    /// already rebound a new inode).
    pub fn cleanup(&self) {
        if let Ok(meta) = fs::metadata(&self.socket_path) {
            if meta.dev() == self.created_dev && meta.ino() == self.created_ino {
                let _ = fs::remove_file(&self.socket_path);
            }
        }
    }
}

impl Drop for BridgeListener {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unix_bare_path() {
        match BridgeEndpoint::parse("/run/anland-rdp/bridge.sock").unwrap() {
            BridgeEndpoint::Unix(p) => assert_eq!(p, PathBuf::from("/run/anland-rdp/bridge.sock")),
            _ => panic!("expected Unix"),
        }
    }

    #[test]
    fn parse_unix_scheme() {
        let e = BridgeEndpoint::parse("unix:///run/anland-rdp/bridge.sock").unwrap();
        assert!(matches!(e, BridgeEndpoint::Unix(_)));
    }

    #[test]
    fn parse_tcp_loopback_v4() {
        let e = BridgeEndpoint::parse("tcp://127.0.0.1:33910").unwrap();
        assert!(matches!(e, BridgeEndpoint::Tcp(_)));
    }

    #[test]
    fn parse_tcp_loopback_v6() {
        let e = BridgeEndpoint::parse("tcp://[::1]:33910").unwrap();
        assert!(matches!(e, BridgeEndpoint::Tcp(_)));
    }

    #[test]
    fn parse_rejects_non_loopback_tcp() {
        assert!(BridgeEndpoint::parse("tcp://192.168.1.5:33910").is_err());
        assert!(BridgeEndpoint::parse("tcp://0.0.0.0:33910").is_err());
    }

    #[test]
    fn parse_rejects_empty_and_relative() {
        assert!(BridgeEndpoint::parse("").is_err());
        assert!(BridgeEndpoint::parse("relative/path").is_err());
    }

    /// End-to-end bind + revalidation + flock contention over a real Unix
    /// socket in a tmp dir.
    #[tokio::test]
    async fn bind_revalidates_and_flock_blocks_second_listener() {
        let dir = tempfile::tempdir().unwrap();
        // Make the temp parent 0700 + euid-owned so validate_listener_parent
        // passes for a socket one level deeper.
        fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let sock = dir.path().join("bridge.sock");
        let listener = BridgeListener::bind_unix(&sock).await.unwrap();
        // Bound socket must be 0600 + euid-owned + a real socket.
        let meta = fs::metadata(&sock).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.mode() & 0o777, 0o600);
        assert_eq!(meta.uid(), euid());
        // Lock path exists beside it.
        assert!(fs::metadata(lock_path_for(&sock)).is_ok());
        // A second listener on the same socket must fail (flock contention).
        assert!(BridgeListener::bind_unix(&sock).await.is_err());
        drop(listener);
        // After drop the socket is removed (device/inode matched).
        assert!(fs::metadata(&sock).is_err());
    }

    /// prepare_stale_socket refuses to clobber a regular file.
    #[test]
    fn stale_socket_refuses_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let bogus = dir.path().join("bridge.sock");
        fs::write(&bogus, b"not a socket").unwrap();
        assert!(prepare_stale_socket(&bogus).is_err());
    }

    /// A stale socket from a previous run is removed so bind can proceed.
    #[tokio::test]
    async fn stale_socket_is_removed_for_rebind() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let sock = dir.path().join("bridge.sock");
        // First listener creates + binds, then drops (removing the socket).
        {
            let _l = BridgeListener::bind_unix(&sock).await.unwrap();
        }
        assert!(fs::metadata(&sock).is_err());
        // Rebind over the (now-absent) socket; the .lock is released when the
        // previous listener dropped.
        let _l2 = BridgeListener::bind_unix(&sock).await.unwrap();
        drop(_l2);
        let _l3 = BridgeListener::bind_unix(&sock).await.unwrap();
    }
}
