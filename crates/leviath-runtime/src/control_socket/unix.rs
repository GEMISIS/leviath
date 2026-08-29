//! Unix-domain-socket transport for the control channel.
//!
//! A [`ControlId`] is a filesystem path; the socket is never reachable off the
//! machine, and access to it is governed by ordinary file permissions - which
//! [`bind_control_listener`] must set explicitly, never assume. An un-`chmod`ed
//! socket and directory land at the process umask (typically 0755, and
//! group-writable under a 0002 umask), and anyone who can connect can spawn a
//! tool-executing agent and answer its approval prompts.

use std::path::{Path, PathBuf};

/// Identifies a control socket: the filesystem path of the Unix-domain socket.
pub type ControlId = PathBuf;

/// The client end of a control connection.
pub(crate) type ClientStream = tokio::net::UnixStream;

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
    /// for it. This uid check is the channel's authentication, and it cannot be
    /// skipped: the ops this channel accepts spawn tool-executing agents and
    /// answer their approval prompts.
    ///
    /// The socket's 0600 mode is not sufficient on its own: on macOS and the
    /// BSDs, permissions on a Unix socket are not consulted at `connect` time.
    /// The peer's uid comes from the kernel, so it is the check that actually
    /// holds on every platform.
    ///
    /// A peer whose uid cannot be determined is refused. An unidentifiable
    /// caller is not an authorized one, and failing open here would undo the
    /// whole point on any platform where the lookup is unavailable.
    pub async fn accept(&mut self) -> std::io::Result<Option<ServerStream>> {
        self.accept_with(leviath_sys::peer_uid).await
    }

    /// Core of [`accept`](Self::accept) with the peer lookup injected.
    ///
    /// A `fn` pointer (not `impl Fn`) so there is one monomorphization. The seam
    /// exists because the refusal arms cannot be reached otherwise: producing a
    /// connection from a *different* uid needs a second user account or root,
    /// which no CI runner has. Injecting the lookup lets both refusals - a
    /// foreign uid and an undeterminable one - be driven against a real socket.
    async fn accept_with(
        &mut self,
        peer_uid: fn(&ServerStream) -> Option<u32>,
    ) -> std::io::Result<Option<ServerStream>> {
        // One expression, no `?` and no retry loop: the caller's accept loop is
        // already a loop, so `Ok(None)` ("someone connected, but not us") slots
        // into it as a `continue`. Retrying in here would add a branch that no
        // test can reach, in the one function where the refusal *is* the point.
        self.0.accept().await.map(|(stream, _addr)| {
            peer_is_ours(peer_uid(&stream), leviath_sys::current_uid()).then_some(stream)
        })
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
        // Nothing is listening - the socket file is stale; clear it. A failed
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
    // Best-effort, like the directory above: `chmod` on a socket we just created
    // and own does not realistically fail, and the peer check in `accept` - not
    // this mode - is what actually holds on macOS and the BSDs, where socket
    // permissions are not consulted at `connect` time.
    let _ = leviath_sys::secure_file_perms(id);
    Ok(ControlListener(listener))
}

/// Whether an accepted connection's peer is this same user.
///
/// `None` - the platform could not report a uid - is refused. An unidentifiable
/// caller is not an authorized one, and failing open here would undo the whole
/// point on any platform where the lookup is unavailable.
fn peer_is_ours(peer: Option<u32>, ours: u32) -> bool {
    match peer {
        Some(uid) if uid == ours => true,
        Some(uid) => {
            tracing::warn!(
                peer_uid = uid,
                daemon_uid = ours,
                "refused a control connection from another user"
            );
            false
        }
        None => {
            tracing::warn!("refused a control connection whose peer uid could not be determined");
            false
        }
    }
}

/// Connect to the daemon's control socket at `id`.
pub(crate) async fn connect(id: &Path) -> std::io::Result<ClientStream> {
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
    /// while setting none of them - both the socket and the directory landed at
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

    /// Both refusal arms, driven against a real socket with the peer lookup
    /// injected. A connection from a *different* uid needs a second user account
    /// or root, which no CI runner has - so the lookup is the seam rather than
    /// the socket.
    #[tokio::test]
    async fn accept_skips_connections_that_are_not_ours() {
        fn foreign_uid(_: &ServerStream) -> Option<u32> {
            Some(leviath_sys::current_uid().wrapping_add(1))
        }
        fn unknown_uid(_: &ServerStream) -> Option<u32> {
            None
        }

        for (label, lookup) in [
            (
                "another user",
                foreign_uid as fn(&ServerStream) -> Option<u32>,
            ),
            ("an undeterminable uid", unknown_uid),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let id = dir.path().join("control.sock");
            let mut listener = bind_control_listener(&id).unwrap();

            // Connect first and keep the client alive: a Unix-socket `connect`
            // completes as soon as the listener has queued it, so no spawned
            // task is needed - and no abort, whose half-run future would read as
            // a partially covered region.
            let _client = connect(&id).await.expect("connecting succeeds");
            let accepted = listener
                .accept_with(lookup)
                .await
                .expect("accepting itself succeeds");
            assert!(
                accepted.is_none(),
                "a connection from {label} must not be handed to the daemon"
            );
        }
    }

    /// The decision itself: our own uid passes, anything else does not.
    #[test]
    fn peer_is_ours_admits_only_this_user() {
        assert!(peer_is_ours(Some(1000), 1000));
        assert!(!peer_is_ours(Some(1001), 1000));
        assert!(!peer_is_ours(None, 1000), "an unknown peer fails closed");
    }

    /// A connection from this same user is accepted - the peer check must not
    /// lock the daemon out of its own socket.
    #[tokio::test]
    async fn accept_admits_a_connection_from_the_same_user() {
        let dir = tempfile::tempdir().unwrap();
        let id = dir.path().join("control.sock");
        let mut listener = bind_control_listener(&id).unwrap();

        let client = tokio::spawn(async move { connect(&id).await });
        let accepted = listener.accept().await.expect("accept succeeds");
        assert!(accepted.is_some(), "same-uid peer must be admitted");
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
