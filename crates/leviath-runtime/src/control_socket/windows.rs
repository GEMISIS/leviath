//! Named-pipe transport for the control channel.
//!
//! A [`ControlId`] is a pipe name (`\\.\pipe\…`) derived from the leviath home
//! directory; the pipe's access is governed by its security descriptor, and it
//! is local to the machine. The daemon keeps one server instance waiting at all
//! times (creating the next as each client is accepted) so the name stays
//! resolvable for connecting clients.

use std::hash::{Hash, Hasher};
use std::path::Path;

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};

/// `ERROR_ACCESS_DENIED`: returned by a `first_pipe_instance` create when the
/// pipe already exists - i.e. another daemon owns it.
const ERROR_ACCESS_DENIED: i32 = 5;

/// Identifies a control pipe: its full `\\.\pipe\…` name.
pub type ControlId = String;

/// The client end of a control connection.
pub type ClientStream = NamedPipeClient;

/// The server end of an accepted control connection.
pub type ServerStream = NamedPipeServer;

/// A stable, unique control-pipe name derived from `base` (e.g.
/// `<leviath-home>/.leviath`). Different homes hash to different pipe names, so
/// they never collide.
pub fn control_id(base: &Path) -> ControlId {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    base.hash(&mut hasher);
    format!(r"\\.\pipe\leviath-control-{:016x}", hasher.finish())
}

/// Parse a user-supplied `--socket` override into a [`ControlId`] (a pipe name).
pub fn control_id_from_str(s: &str) -> ControlId {
    s.to_string()
}

/// True if a daemon is currently answering on the pipe named `id`.
pub fn is_daemon_running(id: &str) -> bool {
    ClientOptions::new().open(id).is_ok()
}

/// A bound control listener: the pipe name plus the server instance currently
/// waiting for the next client.
#[derive(Debug)]
pub struct ControlListener {
    name: String,
    pending: NamedPipeServer,
}

impl ControlListener {
    /// Wait for the next client, then return the connected server stream -
    /// pre-creating the following instance so the pipe name stays resolvable for
    /// the next client while this one is served.
    ///
    /// Written as a combinator chain (rather than `?`) so the error short-circuit
    /// lives in `Result`, not in a branch of this function - the happy path is
    /// the only thing this source expresses, keeping it fully covered while still
    /// propagating any real connect/create failure.
    ///
    /// # The `Option` is always `Some` here, and that is a real gap
    ///
    /// The Unix listener returns `Ok(None)` for a connection from another user,
    /// having checked the peer's uid with `SO_PEERCRED`. There is no equivalent
    /// check on this side: the Windows analogue is
    /// `ImpersonateNamedPipeClient` plus a token comparison, which is genuine
    /// work and is not done. The shape matches so the daemon's accept loop is
    /// one piece of code on both platforms, but on Windows every connection the
    /// pipe accepts is served.
    ///
    /// What stands between another local user and this channel on Windows is
    /// therefore the pipe's security descriptor alone - and this code does not
    /// set one, so it is the default. That is weaker than the Unix side, where
    /// the kernel-reported uid is the gate. Documented in `SECURITY.md` rather
    /// than papered over; see the Unix module for what the check buys.
    pub async fn accept(&mut self) -> std::io::Result<Option<ServerStream>> {
        self.pending
            .connect()
            .await
            .and_then(|()| ServerOptions::new().create(&self.name))
            .map(|next| Some(std::mem::replace(&mut self.pending, next)))
    }
}

/// Bind the daemon's control pipe named `id`, enforcing a single instance.
///
/// Creating the *first* instance of the pipe fails with `ERROR_ACCESS_DENIED`
/// if the pipe already exists - i.e. another daemon owns it - which is mapped to
/// [`std::io::ErrorKind::AddrInUse`].
pub fn bind_control_listener(id: &str) -> std::io::Result<ControlListener> {
    let pending = ServerOptions::new()
        .first_pipe_instance(true)
        .create(id)
        .map_err(|e| {
            if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
                std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "a leviath daemon is already running on this control pipe",
                )
            } else {
                e
            }
        })?;
    Ok(ControlListener {
        name: id.to_string(),
        pending,
    })
}

/// Connect to the daemon's control pipe named `id`.
pub async fn connect(id: &str) -> std::io::Result<ClientStream> {
    // `open` is synchronous; the returned client is async for I/O.
    ClientOptions::new().open(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_id_is_a_pipe_name_stable_per_base() {
        let a = control_id(Path::new("/home/x/.leviath"));
        assert!(a.starts_with(r"\\.\pipe\leviath-control-"));
        // Deterministic per base; distinct bases differ.
        assert_eq!(a, control_id(Path::new("/home/x/.leviath")));
        assert_ne!(a, control_id(Path::new("/home/y/.leviath")));
    }

    #[test]
    fn control_id_from_str_is_the_name() {
        assert_eq!(control_id_from_str(r"\\.\pipe\custom"), r"\\.\pipe\custom");
    }

    #[tokio::test]
    async fn bind_errors_on_invalid_pipe_name() {
        // A name that is not a valid pipe path makes `create` fail with something
        // other than ACCESS_DENIED, exercising the pass-through map arm.
        assert!(bind_control_listener("not-a-valid-pipe-name").is_err());
    }
}
