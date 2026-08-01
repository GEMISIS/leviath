//! Errors from building or driving an embedded world.

/// Why an [`AgentWorld`](super::AgentWorld) could not be built or a request to
/// it could not be served.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EmbedError {
    /// The builder was given no providers: no credentials and no custom
    /// provider registrations. A world with nothing to infer against can only
    /// error, so this fails at build time instead.
    NoProviders,
    /// No Tokio runtime was found. `build()` must run inside a Tokio runtime
    /// (or be given a handle via
    /// [`runtime`](super::AgentWorldBuilder::runtime)).
    NoRuntime,
    /// The blueprint could not be loaded, parsed, or validated.
    Blueprint(String),
    /// The spawn was rejected (bad workdir, unresolvable seeds, and so on).
    Spawn(String),
    /// The world's serve loop is gone (already shut down), so the request
    /// could not be delivered or answered.
    ChannelClosed,
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::NoProviders => {
                write!(f, "no providers configured: add credentials or a provider")
            }
            EmbedError::NoRuntime => {
                write!(f, "no tokio runtime: build inside one or pass a handle")
            }
            EmbedError::Blueprint(msg) => write!(f, "blueprint error: {msg}"),
            EmbedError::Spawn(msg) => write!(f, "spawn error: {msg}"),
            EmbedError::ChannelClosed => write!(f, "the world has shut down"),
        }
    }
}

impl std::error::Error for EmbedError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_covers_every_variant() {
        let cases = [
            (EmbedError::NoProviders, "no providers"),
            (EmbedError::NoRuntime, "no tokio runtime"),
            (
                EmbedError::Blueprint("bad".to_string()),
                "blueprint error: bad",
            ),
            (EmbedError::Spawn("nope".to_string()), "spawn error: nope"),
            (EmbedError::ChannelClosed, "shut down"),
        ];
        for (err, needle) in cases {
            assert!(err.to_string().contains(needle));
        }
    }
}
