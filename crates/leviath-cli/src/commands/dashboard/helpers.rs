//! Pure utility functions used across the dashboard.

use leviath_core::truncate_at_boundary;

/// Format a Unix timestamp as a relative time string ("just now", "2m ago", "1h ago").
pub(super) fn relative_time(ts: i64) -> String {
    if ts == 0 {
        return "-".to_string();
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - ts).max(0) as u64;
    if secs < 10 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        let m = secs / 60;
        format!("{}m ago", m)
    } else if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{}h ago", h)
        } else {
            format!("{}h{}m ago", h, m)
        }
    } else {
        let d = secs / 86400;
        format!("{}d ago", d)
    }
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        s.to_string()
    } else {
        // Cut on a char boundary so multi-byte content (em-dashes, emoji in run
        // titles) shortens instead of panicking.
        format!("{}…", truncate_at_boundary(s, max))
    }
}

/// Format a token count in compact style: ≥1000 → "21k", else raw.
pub(super) fn format_tokens(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// Format elapsed seconds as a human-readable duration string.
pub(super) fn elapsed_str(started_at: i64) -> String {
    if started_at == 0 {
        return "-".to_string();
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - started_at).max(0) as u64;
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Format elapsed seconds from `started_at` up to `until` (not current time).
pub(super) fn elapsed_str_until(started_at: i64, until: i64) -> String {
    if started_at == 0 {
        return "-".to_string();
    }
    let secs = (until - started_at).max(0) as u64;
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// The native clipboard tools tried, in order, before falling back to OSC52.
const NATIVE_CLIPBOARD_CMDS: &[(&str, &[&str])] = &[
    ("pbcopy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("wl-copy", &[]),
];

/// Copy `text` to the system clipboard (returns `true` on success),
/// parameterized over the OSC52 fallback so tests can inject a fake that never
/// touches the real TTY. Strategy: native tool (`pbcopy`/`xclip`/`wl-copy`)
/// first, then the injected OSC52 fallback.
///
/// `pub` so the binary's `real_dashboard` can build the real dashboard
/// clipboard fn as `|t| yank_to_clipboard_via(t, leviath_sys::osc52_yank)` and
/// inject it - keeping the real `/dev/tty` write (which no unit test may
/// trigger) out of the coverage-measured library.
pub fn yank_to_clipboard_via(text: &str, osc52_fallback: fn(&str) -> bool) -> bool {
    yank_to_clipboard_with(text, NATIVE_CLIPBOARD_CMDS, osc52_fallback)
}

/// [`yank_to_clipboard_via`] with the native-tool command list injected, so a
/// test can drive the spawn-success and non-zero-exit branches with a program
/// guaranteed present on the host (the real `pbcopy`/`xclip`/`wl-copy` names
/// don't exist on Windows, so a fake `#!/bin/sh` script on `PATH` couldn't
/// exercise these branches there).
fn yank_to_clipboard_with(
    text: &str,
    clipboard_cmds: &[(&str, &[&str])],
    osc52_fallback: fn(&str) -> bool,
) -> bool {
    use std::io::Write as IoWrite;
    use std::process::{Command, Stdio};

    // Try native clipboard tools first - most reliable
    for (cmd, args) in clipboard_cmds {
        let mut command = Command::new(cmd);
        command
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // The dashboard is a full-screen TUI; a console window popping over it
        // on every yank would be the worst place for one.
        leviath_sys::hide_console_window(&mut command);
        if let Ok(mut child) = command.spawn() {
            // `child.stdin` is guaranteed `Some` here because the child was
            // spawned with `.stdin(Stdio::piped())` above - an `if let`
            // guard would introduce an implicit "pattern didn't match" branch
            // that could never actually be exercised, since that invariant
            // always holds.
            let _ = child
                .stdin
                .as_mut()
                .expect("child spawned with Stdio::piped() stdin")
                .write_all(text.as_bytes());
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }

    // Fall back to OSC52
    osc52_fallback(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_long() {
        let result = truncate("hello world", 5);
        assert_eq!(result, "hello…");
    }

    #[test]
    fn test_truncate_trims_whitespace() {
        assert_eq!(truncate("  hi  ", 10), "hi");
    }

    #[test]
    fn test_truncate_empty() {
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn test_format_tokens_small() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(42), "42");
        assert_eq!(format_tokens(999), "999");
    }

    #[test]
    fn test_format_tokens_thousands() {
        assert_eq!(format_tokens(1000), "1k");
        assert_eq!(format_tokens(1500), "1k");
        assert_eq!(format_tokens(21000), "21k");
        assert_eq!(format_tokens(999_999), "999k");
    }

    #[test]
    fn test_format_tokens_millions() {
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(2_500_000), "2M");
    }

    #[test]
    fn test_relative_time_zero() {
        assert_eq!(relative_time(0), "-");
    }

    #[test]
    fn test_elapsed_str_zero() {
        assert_eq!(elapsed_str(0), "-");
    }

    #[test]
    fn test_elapsed_str_until_zero() {
        assert_eq!(elapsed_str_until(0, 100), "-");
    }

    #[test]
    fn test_elapsed_str_until_seconds() {
        assert_eq!(elapsed_str_until(100, 145), "45s");
    }

    #[test]
    fn test_elapsed_str_until_minutes() {
        assert_eq!(elapsed_str_until(100, 225), "2m5s");
    }

    #[test]
    fn test_elapsed_str_until_hours() {
        assert_eq!(elapsed_str_until(100, 7400), "2h1m");
    }

    #[test]
    fn test_elapsed_str_until_negative_clamped() {
        // until < started_at → clamped to 0
        assert_eq!(elapsed_str_until(200, 100), "0s");
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_format_tokens_boundary_999() {
        assert_eq!(format_tokens(999), "999");
    }

    #[test]
    fn test_format_tokens_boundary_1000() {
        assert_eq!(format_tokens(1000), "1k");
    }

    #[test]
    fn test_format_tokens_boundary_999999() {
        assert_eq!(format_tokens(999_999), "999k");
    }

    #[test]
    fn test_format_tokens_boundary_1000000() {
        assert_eq!(format_tokens(1_000_000), "1M");
    }

    #[test]
    fn test_truncate_unicode() {
        let result = truncate("abcdef", 3);
        assert_eq!(result, "abc…");
    }

    #[test]
    fn test_truncate_multibyte_char_boundary() {
        // Em-dash (–) is 3 bytes (U+2013, bytes E2 80 93).
        // Slicing at a byte index inside the em-dash must not panic.
        let s = "Research the history – covering all topics";
        // "Research the history " is 21 bytes, then "–" is bytes 21..24.
        // Truncating at max=22 would land inside the em-dash without the fix.
        let result = truncate(s, 22);
        assert!(!result.is_empty());
        assert!(result.ends_with('…'));
        // Should back up to byte 21 (the space before the em-dash)
        assert_eq!(result, "Research the history …");

        // Also test truncating right at the start of the em-dash
        let result2 = truncate(s, 21);
        assert_eq!(result2, "Research the history …");

        // And right after the em-dash
        let result3 = truncate(s, 24);
        assert_eq!(result3, "Research the history –…");
    }

    #[test]
    fn test_truncate_emoji() {
        // Emoji like 🔥 is 4 bytes. Truncating in the middle must not panic.
        let s = "Hello 🔥 world";
        let result = truncate(s, 7); // lands inside the emoji (bytes 6..10)
        assert!(!result.is_empty());
        assert!(result.ends_with('…'));
        assert_eq!(result, "Hello …");
    }

    #[test]
    fn test_elapsed_str_until_exact_minute() {
        assert_eq!(elapsed_str_until(1000, 1060), "1m0s");
    }

    #[test]
    fn test_elapsed_str_until_exact_hour() {
        assert_eq!(elapsed_str_until(1000, 4600), "1h0m");
    }

    #[test]
    fn test_elapsed_str_until_same_time() {
        assert_eq!(elapsed_str_until(100, 100), "0s");
    }

    #[test]
    fn test_elapsed_str_until_zero_start_returns_dash() {
        assert_eq!(elapsed_str_until(0, 60), "-");
    }

    // OSC52 encoding and the /dev/tty write path now live in `leviath_sys::tty`
    // and are tested there; the dashboard only wires the fallback in.

    // ── relative_time branches ────────────────────────────────────────────────

    #[test]
    fn test_relative_time_just_now() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // 3 seconds ago → "just now"
        let result = relative_time(now - 3);
        assert_eq!(result, "just now");
    }

    #[test]
    fn test_relative_time_seconds_ago() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // 30 seconds ago → "30s ago"
        let result = relative_time(now - 30);
        assert!(result.ends_with("s ago"));
    }

    #[test]
    fn test_relative_time_minutes_ago() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // 5 minutes ago → "5m ago"
        let result = relative_time(now - 300);
        assert!(result.ends_with("m ago"));
    }

    #[test]
    fn test_relative_time_hours_ago_no_minutes() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Exactly 2 hours → "2h ago"
        let result = relative_time(now - 7200);
        assert_eq!(result, "2h ago");
    }

    #[test]
    fn test_relative_time_hours_ago_with_minutes() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // 1 hour 30 minutes → "1h30m ago"
        let result = relative_time(now - 5400);
        assert_eq!(result, "1h30m ago");
    }

    #[test]
    fn test_relative_time_days_ago() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // 3 days ago → "3d ago"
        let result = relative_time(now - 3 * 86400);
        assert_eq!(result, "3d ago");
    }

    // ── elapsed_str branches ──────────────────────────────────────────────────

    #[test]
    fn test_elapsed_str_seconds() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Started 45 seconds ago
        let result = elapsed_str(now - 45);
        assert!(result.ends_with('s'));
    }

    #[test]
    fn test_elapsed_str_minutes() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Started 3 min 15 sec ago
        let result = elapsed_str(now - 195);
        assert!(result.contains('m'));
    }

    #[test]
    fn test_elapsed_str_hours() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Started 2 hours 10 minutes ago
        let result = elapsed_str(now - 7800);
        assert!(result.contains('h'));
    }

    // ── yank_to_clipboard: at least exercises the code path ──────────────────
    //
    // Ambient-`PATH` smoke tests are avoided here. Both the "native tool
    // succeeds" and "falls back to OSC52" branches of `yank_to_clipboard_via`
    // are covered deterministically below (`..._native_tool_success_returns_true_...`,
    // `..._nonzero_exit_falls_through_to_fallback`,
    // `..._falls_back_to_osc52_when_no_native_tool_on_path`) by starving
    // `PATH`, so an ambient-`PATH` test adds no unique coverage while growing
    // the number of `PATH`-mutation windows that tests not holding
    // `PATH_ENV_LOCK` (e.g. the dashboard's real `key('y')` handlers in
    // `input.rs`) could race with.

    #[test]
    fn test_yank_to_clipboard_empty() {
        // Starves `PATH` so the injected fallback is reached deterministically
        // regardless of which native clipboard tools happen to be installed
        // on the machine `cargo test` runs on (see the ambient-`PATH`
        // rationale above for why ambient-`PATH` smoke tests are avoided here).
        fn fake_osc52_fallback(_text: &str) -> bool {
            true
        }
        let result = temp_env::with_var("PATH", Some("/lev-definitely-empty-path-dir"), || {
            yank_to_clipboard_via("", fake_osc52_fallback)
        });
        assert!(result);
    }

    // ── yank_to_clipboard_via: native-tool success branch ────────────────────

    // Module-scoped (rather than nested inside the test below) so its body
    // can also be exercised directly by
    // `test_unreachable_osc52_fallback_panics_if_ever_invoked` - both
    // branches (never called vs. called-and-panics) need coverage, and the
    // test below can only ever prove the "never called" side.
    fn unreachable_osc52_fallback(_text: &str) -> bool {
        panic!("OSC52 fallback must not run when the fake pbcopy succeeds");
    }

    #[test]
    #[should_panic(expected = "OSC52 fallback must not run when the fake pbcopy succeeds")]
    fn test_unreachable_osc52_fallback_panics_if_ever_invoked() {
        unreachable_osc52_fallback("anything");
    }

    /// A native-tool command that spawns and exits with the given status on the
    /// host, injected in place of `pbcopy`/`xclip`/`wl-copy` so the spawn-loop
    /// branches are exercised cross-platform. `true`/`false` exist on
    /// macOS/Linux; `cmd /C exit N` is the Windows equivalent (both drain/ignore
    /// stdin and exit deterministically).
    #[cfg(not(windows))]
    fn exit_cmd(success: bool) -> (&'static str, &'static [&'static str]) {
        if success {
            ("true", &[])
        } else {
            ("false", &[])
        }
    }
    #[cfg(windows)]
    fn exit_cmd(success: bool) -> (&'static str, &'static [&'static str]) {
        if success {
            ("cmd", &["/C", "exit 0"])
        } else {
            ("cmd", &["/C", "exit 1"])
        }
    }

    #[test]
    fn test_yank_to_clipboard_native_tool_success_returns_true_without_fallback() {
        // A guaranteed-present command that exits 0 exercises the native-tool
        // success path (the `return true` inside the spawn loop) on every
        // platform, without depending on a real clipboard tool being installed.
        // This only *reads* `PATH` (to resolve `true`/`cmd`), so it takes
        // temp-env's exclusive lock without changing any var - serializing it
        // against the PATH-starving tests so it never observes a starved PATH.
        let (cmd, args) = exit_cmd(true);
        let result = temp_env::with_vars_unset(Vec::<&str>::new(), || {
            yank_to_clipboard_with(
                "native tool success test",
                &[(cmd, args)],
                unreachable_osc52_fallback,
            )
        });
        assert!(result);
    }

    #[test]
    fn test_yank_to_clipboard_native_tool_nonzero_exit_falls_through_to_fallback() {
        // A guaranteed-present command that exits non-zero makes
        // `child.wait().map(|s| s.success())` false, so the loop falls through
        // to the OSC52 fallback - the branch the success test doesn't reach.
        // Reads `PATH` only, so serialize via temp-env's lock without mutating.
        fn fallback_reached(_text: &str) -> bool {
            true
        }
        let (cmd, args) = exit_cmd(false);
        let result = temp_env::with_vars_unset(Vec::<&str>::new(), || {
            yank_to_clipboard_with("nonzero exit test", &[(cmd, args)], fallback_reached)
        });
        // The native tool "ran" but failed, so control reached the fallback.
        assert!(result);
    }

    // ─── yank_to_clipboard ──────────────────────────────────────────────────
    //
    // Ambient-`PATH` smoke tests are avoided here: their nested fallback
    // closure only gets covered when a concurrently-running `PATH`-mutating
    // test races with them - nondeterministic, the exact kind of flakiness
    // this file's `PATH_ENV_LOCK`-guarded tests exist to avoid.
    // `yank_to_clipboard` itself (the public, un-suffixed wrapper) is
    // exercised for real by the dashboard's `key('y')` handler tests in
    // `input.rs`; the native-success and OSC52-fallback branches of
    // `yank_to_clipboard_via` it delegates to are covered deterministically by
    // the tests immediately below.

    #[test]
    fn test_yank_to_clipboard_falls_back_to_osc52_when_no_native_tool_on_path() {
        // Starve PATH so `Command::new("pbcopy"/"xclip"/"wl-copy").spawn()`
        // fails with NotFound for all three - the `if let Ok(mut child) = `
        // guard is simply false each time (no external process is ever
        // launched), falling through to the OSC52 fallback. That fallback is
        // injected as a fn pointer that never touches the real TTY (unlike
        // the real `osc52_yank_raw`, which would open `/dev/tty` here).
        // `temp_env::with_var` (serialized process-wide, then restored) also
        // serializes against `commands/run/session.rs`'s PATH-mutating
        // `launch_editor` tests, which share the same global temp-env lock.
        fn fake_osc52_fallback(_text: &str) -> bool {
            true
        }
        let result = temp_env::with_var("PATH", Some("/lev-definitely-empty-path-dir"), || {
            yank_to_clipboard_via("fallback path test content", fake_osc52_fallback)
        });
        // The point of this test is proving the native-tool loop was skipped
        // entirely and control reached the injected OSC52 fallback - not
        // exercising the real `osc52_yank_raw` (see its own tests above).
        assert!(result);
    }

    // The real dashboard clipboard fn (native tools → real OSC52 `/dev/tty`
    // write) is now composed in the binary's `real_dashboard` as
    // `|t| yank_to_clipboard_via(t, leviath_sys::osc52_yank)` and injected into
    // `Dashboard`; the native-tool and fallback branches of
    // `yank_to_clipboard_via`/`_with` are covered above with injected fakes, and
    // the real OSC52 write is tested in `leviath_sys::tty`.
}
