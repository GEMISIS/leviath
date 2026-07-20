//! Unix-domain-socket transport for the control channel.
//!
//! A [`ControlId`] is a filesystem path; the socket's access is governed by
//! ordinary file permissions, and it is never reachable off the machine.

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
    /// Accept the next control connection.
    pub async fn accept(&mut self) -> std::io::Result<ServerStream> {
        self.0.accept().await.map(|(stream, _addr)| stream)
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
    Ok(ControlListener(tokio::net::UnixListener::bind(id)?))
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
