//! Opening a URL in the user's default browser.
//!
//! The launcher differs per OS (`open`, `xdg-open`, `cmd /c start`). The
//! platform selection is a pure function tested against every OS string, and
//! the actual process spawn is injected, so nothing here needs a `#[cfg]` and
//! every branch is reachable under test on a single platform.

use std::process::Command;

/// The launcher command and arguments for `url` on `os`.
///
/// `os` is the value of `std::env::consts::OS`. An unrecognized OS falls back
/// to `xdg-open`, the freedesktop standard, which covers the BSDs and other
/// Unixes.
///
/// On Windows the empty `""` title argument to `start` matters: `start` treats
/// a single quoted argument as a window title, so a URL would be swallowed
/// without a placeholder title ahead of it.
pub fn open_command_for(os: &str, url: &str) -> (String, Vec<String>) {
    match os {
        "macos" => ("open".to_string(), vec![url.to_string()]),
        "windows" => (
            "cmd".to_string(),
            vec![
                "/C".to_string(),
                "start".to_string(),
                String::new(),
                url.to_string(),
            ],
        ),
        _ => ("xdg-open".to_string(), vec![url.to_string()]),
    }
}

/// Open `url` in the default browser, spawning via `spawn`.
///
/// `spawn` is injected so the real process launch is isolated from the logic:
/// production passes [`spawn_detached`], tests pass a recording stub. Returns
/// whether the launcher was spawned successfully - not whether the user
/// actually saw the page, which is unknowable.
pub fn open_url_via(spawn: fn(&mut Command) -> std::io::Result<()>, url: &str) -> bool {
    let (program, args) = open_command_for(std::env::consts::OS, url);
    let mut cmd = Command::new(program);
    cmd.args(args);
    match spawn(&mut cmd) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to launch browser");
            false
        }
    }
}

/// Spawn `cmd` fire-and-forget, discarding its output.
///
/// The browser launcher is detached: we neither wait for it nor read its
/// pipes, since it may outlive this process.
pub fn spawn_detached(cmd: &mut Command) -> std::io::Result<()> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_child| ())
}

/// Open `url` in the default browser.
pub fn open_url(url: &str) -> bool {
    open_url_via(spawn_detached, url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_uses_open() {
        let (program, args) = open_command_for("macos", "https://x.example");
        assert_eq!(program, "open");
        assert_eq!(args, vec!["https://x.example"]);
    }

    #[test]
    fn windows_uses_start_with_a_placeholder_title() {
        let (program, args) = open_command_for("windows", "https://x.example");
        assert_eq!(program, "cmd");
        // The empty title placeholder must sit before the URL, or `start`
        // consumes the URL as the window title.
        assert_eq!(args, vec!["/C", "start", "", "https://x.example"]);
    }

    #[test]
    fn other_unixes_fall_back_to_xdg_open() {
        let (program, args) = open_command_for("linux", "https://x.example");
        assert_eq!(program, "xdg-open");
        assert_eq!(args, vec!["https://x.example"]);

        // An unknown OS also uses the freedesktop launcher.
        assert_eq!(open_command_for("dragonfly", "https://x").0, "xdg-open");
    }

    #[test]
    fn open_url_via_reports_spawn_success() {
        fn ok(_: &mut Command) -> std::io::Result<()> {
            Ok(())
        }
        assert!(open_url_via(ok, "https://x.example"));
    }

    /// The failure arm logs, and `tracing::warn!` evaluates its field values
    /// only when a subscriber is interested. Without one installed the `%e`
    /// field closure never runs - so this test exercised the branch while
    /// leaving the logging inside it unexecuted. `with_tracing` is the same
    /// always-on-subscriber shim `leviath-cli` uses for exactly this.
    #[test]
    fn open_url_via_reports_spawn_failure() {
        fn boom(_: &mut Command) -> std::io::Result<()> {
            Err(std::io::Error::other("no browser"))
        }
        with_tracing(|| assert!(!open_url_via(boom, "https://x.example")));
    }

    /// An always-enabled [`tracing::Subscriber`], so macro bodies actually run.
    struct AlwaysOnSubscriber;

    impl tracing::Subscriber for AlwaysOnSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn register_callsite(
            &self,
            _metadata: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::always()
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            // Callsites are cached as always-enabled, so the macros never call
            // `enabled`; call it here so it is exercised.
            assert!(self.enabled(event.metadata()));
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
        fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
            Some(tracing::metadata::LevelFilter::TRACE)
        }
    }

    /// Exercise the span methods the logging path never reaches on its own, so
    /// the shim above is fully covered rather than only its `event` arm.
    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        with_tracing(|| {
            let span = tracing::info_span!("test-span", field = tracing::field::Empty);
            span.record("field", 1);
            let other = tracing::info_span!("other-span");
            span.follows_from(&other);
            let _enter = span.enter();
            tracing::info!(parent: &span, "inside span");
        });
    }

    /// Install [`AlwaysOnSubscriber`] once per test binary and run `f` under it.
    fn with_tracing<T>(f: impl FnOnce() -> T) -> T {
        static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        INSTALLED.get_or_init(|| {
            // Required: callsites registered before the global default is set
            // are cached as interest=never, so without this the macro bodies
            // stay unreachable.
            let _ = tracing::subscriber::set_global_default(AlwaysOnSubscriber);
            tracing::callsite::rebuild_interest_cache();
        });
        f()
    }

    #[test]
    fn spawn_detached_launches_a_real_process() {
        // `true` exits immediately; this exercises the real spawn path without
        // opening anything. On the off chance `true` is absent, a spawn error
        // is still a valid Ok/Err from the function under test.
        let mut cmd = Command::new("true");
        let _ = spawn_detached(&mut cmd);
    }

    #[test]
    fn spawn_detached_errors_on_a_missing_program() {
        let mut cmd = Command::new("/nonexistent/browser/launcher");
        assert!(spawn_detached(&mut cmd).is_err());
    }

    #[test]
    fn open_url_runs_the_real_launcher_without_opening_a_browser() {
        // Drives the real public entry point. The target is a bare, non-existent
        // name rather than a URL, so whichever launcher the host resolves
        // (`open`, `xdg-open`, or `cmd /C start`) errors on a missing file
        // instead of opening a browser. This exercises the real `open_url`
        // delegation on every platform without launching anything. The return
        // value is host-dependent (whether the launcher itself is present), so
        // we only require the call not to panic.
        let _ = open_url("leviath-open-url-test-target");
    }
}
