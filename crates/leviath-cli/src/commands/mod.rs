//! CLI command implementations.

#[cfg(test)]
thread_local! {
    /// Test-only toggle letting a test force [`resolve_cwd`]'s `Err` arm
    /// deterministically on every platform, as a companion to
    /// `list`'s `execute_falls_back_to_default_cwd_when_current_dir_is_gone`
    /// genuine Unix-only filesystem reproduction (real `remove_dir_all` of the
    /// live CWD is a sharing violation on Windows, not a success, so that same
    /// trick isn't available there).
    static FORCE_CWD_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Real CWD lookup, with a test-only failure-injection toggle so its `Err`
/// arm can be forced deterministically (see [`force_cwd_error`]) without
/// changing what production actually calls.
///
/// Shared: `list` and `validate` both need the directory a command was run
/// from, `validate` because it is the workdir a `lev run` would default to and
/// therefore what relative `[read_paths]` entries resolve against.
pub(crate) fn resolve_cwd() -> std::io::Result<std::path::PathBuf> {
    #[cfg(test)]
    if FORCE_CWD_ERROR.with(|f| f.get()) {
        return Err(std::io::Error::other("forced CWD error for testing"));
    }
    std::env::current_dir()
}

/// Force (or release) [`resolve_cwd`]'s failure path for the current thread.
#[cfg(test)]
pub(crate) fn force_cwd_error(on: bool) {
    FORCE_CWD_ERROR.with(|f| f.set(on));
}

pub(crate) mod add;
pub mod agent_client;
pub(crate) mod approvals;
pub mod auth;
pub(crate) mod context;
pub(crate) mod create;
pub mod ctl;
pub mod daemon;
pub mod daemon_service;
pub mod dashboard;
pub mod doctor;
pub mod integrate;
pub(crate) mod list;
pub mod mcp;
pub(crate) mod models;
pub(crate) mod pack;
pub(crate) mod policy;
pub mod providers;
pub mod ps;
pub(crate) mod remove;
pub(crate) mod result;
pub mod run;
pub mod serve;
pub mod setup;
pub(crate) mod stages;
pub(crate) mod test;
pub(crate) mod timeline;
pub(crate) mod tools;
pub mod update;
pub(crate) mod validate;
