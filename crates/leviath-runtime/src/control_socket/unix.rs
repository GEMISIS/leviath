//! Unix-domain-socket transport for the control channel.
//!
//! A [`ControlId`] is a filesystem path; the socket is never reachable off the
//! machine, and access to it is governed by ordinary file permissions —
//! permissions [`bind_control_listener`] now actually sets. It previously only
//! asserted that in this comment: neither the socket nor the directory it
//! creates was ever `chmod`ed, so both landed at the process umask (typically
//! 0755, and group-writable under a 0002 umask). Anyone who can connect can
//! spawn a tool-executing agent and answer its approval prompts.

use std::path::{Path, PathBuf};

/// Identifies a control socket: the filesystem path of the Unix-domain socket.
pub type ControlId = PathBuf;

/// The client end of a control connection.
pub type ClientStream = tokio::net::UnixStream;

/// The server end of an accepted control connection.
pub type ServerStream = tokio::net::UnixStream;

/// The control socket path under `base` (e.g. `<leviath-home>/.leviath`).
pub fn control_id(base: &Path) -> ControlId {
    base.join("control.sock")
}

/// Parse a user-supplied `--socket` override into a [`ControlId`] (a path).
pub fn control_id_from_str(s: &str) -> ControlId {
    PathBuf::from(s)
}

/// True if a daemon is currently answering on the socket at `id`.
pub fn is_daemon_running(id: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(id).is_ok()
}

/// A bound control listener wrapping a Unix-domain socket.
#[derive(Debug)]
pub struct ControlListener(tokio::net::UnixListener);

impl ControlListener {
    /// Accept the next control connection **from this user**.
    ///
    /// A connection from another uid is closed and skipped rather than returned,
    /// so the daemon loop above never sees it and no `handle_connection` runs
    /// for it. There was no authentication here at all before, and the ops this
    /// channel accepts spawn tool-executing agents and answer their approval
    /// prompts.
    ///
    /// The socket's 0600 mode is not sufficient on its own: on macOS and the
    /// BSDs, permissions on a Unix socket are not consulted at `connect` time.
    /// The peer's uid comes from the kernel, so it is the check that actually
    /// holds on every platform.
    ///
    /// A peer whose uid cannot be determined is refused. An unidentifiable
    /// caller is not an authorized one, and failing open here would undo the
    /// whole point on any platform where the lookup is unavailable.
    pub async fn accept(&mut self) -> std::io::Result<ServerStream> {
        loop {
            let (stream, _addr) = self.0.accept().await?;
            let ours = leviath_sys::current_uid();
            match leviath_sys::peer_uid(&stream) {
                Some(peer) if peer == ours => return Ok(stream),
                Some(peer) => {
                    tracing::warn!(
                        peer_uid = peer,
                        daemon_uid = ours,
                        "refused a control connection from another user"
                    );
                }
                None => {
                    tracing::warn!(
                        "refused a control connection whose peer uid could not be determined"
                    );
                }
            }
            // `stream` drops here, closing the connection.
        }
    }
}

/// Bind the daemon's control socket at `id`, enforcing a single instance.
///
/// If a socket already exists there and a daemon answers, one is already running
/// ([`std::io::ErrorKind::AddrInUse`]); a leftover file with no listener is
/// **stale** and is removed before binding. The parent directory is created if
/// needed.
pub fn bind_control_listener(id: &Path) -> std::io::Result<ControlListener> {
    if id.exists() {
        if std::os::unix::net::UnixStream::connect(id).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "a leviath daemon is already running on this control socket",
            ));
        }
        // Nothing is listening — the socket file is stale; clear it. A failed
        // remove just means the bind below reports the problem instead.
        let _ = std::fs::remove_file(id);
    }
    // A control-socket path always has a parent directory.
    let parent = id.parent().expect("control socket path has a parent");
    std::fs::create_dir_all(parent)?;
    // Lock the directory down before binding. `create_dir_all` uses the ambient
    // umask, so on a 0002 umask this was a group-writable directory holding the
    // daemon's control channel. Best-effort: the directory may already exist and
    // be owned by someone else, in which case the socket's own mode below is
    // what carries the guarantee.
    let _ = leviath_sys::secure_dir_perms(parent);

    let listener = tokio::net::UnixListener::bind(id)?;
    // Owner-only on the socket itself. On Linux the mode is enforced on
    // `connect`; on macOS and the BSDs it historically is not, which is why the
    // per-connection peer check in `accept_authorized` is the real gate and this
    // is defence in depth rather than the whole story.
    leviath_sys::secure_file_perms(id)?;
    Ok(ControlListener(listener))
}

/// Connect to the daemon's control socket at `id`.
pub async fn connect(id: &Path) -> std::io::Result<ClientStream> {
    tokio::net::UnixStream::connect(id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_id_is_under_base() {
        assert_eq!(
            control_id(Path::new("/x/.leviath")),
            Path::new("/x/.leviath/control.sock")
        );
    }

    #[test]
    fn control_id_from_str_is_the_path() {
        assert_eq!(
            control_id_from_str("/tmp/my.sock"),
            PathBuf::from("/tmp/my.sock")
        );
    }

    /// The module doc claimed "access is governed by ordinary file permissions"
    /// while setting none of them — both the socket and the directory landed at
    /// the process umask. Anyone who could connect could spawn a tool-executing
    /// agent.
    #[tokio::test]
    async fn bind_locks_down_the_socket_and_its_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        // A nested path so `bind_control_listener` is the one creating the
        // directory, which is the case that inherited the umask.
        let id = dir.path().join("nested").join("control.sock");
        let _listener = bind_control_listener(&id).unwrap();

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&id), 0o600, "socket must be owner-only");
        assert_eq!(
            mode(id.parent().unwrap()),
            0o700,
            "socket directory must be owner-only"
        );
    }

    /// A connection from this same user is accepted — the peer check must not
    /// lock the daemon out of its own socket.
    #[tokio::test]
    async fn accept_admits_a_connection_from_the_same_user() {
        let dir = tempfile::tempdir().unwrap();
        let id = dir.path().join("control.sock");
        let mut listener = bind_control_listener(&id).unwrap();

        let client = tokio::spawn(async move { connect(&id).await });
        let accepted = listener.accept().await;
        assert!(accepted.is_ok(), "same-uid peer must be admitted");
        client.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn bind_removes_a_stale_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        // A leftover regular file where the socket goes: nothing is listening, so
        // it's stale and must be cleared before binding succeeds.
        std::fs::write(&id, b"stale").unwrap();
        let listener = bind_control_listener(&id).unwrap();
        assert!(id.exists());
        drop(listener);
    }

    #[tokio::test]
    async fn bind_errors_when_parent_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file where a parent directory would need to be created.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let id = blocker.join("control.sock"); // parent "blocker" is a file
        assert!(bind_control_listener(&id).is_err());
    }

    #[tokio::test]
    async fn bind_errors_when_target_path_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let id = dir.path().join("control.sock");
        std::fs::create_dir(&id).unwrap(); // a directory can't be bound as a socket
        assert!(bind_control_listener(&id).is_err());
    }
}
