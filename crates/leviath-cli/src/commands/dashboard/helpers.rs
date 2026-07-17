//! Pure utility functions used across the dashboard.

use crate::runstate;

/// Format a Unix timestamp as a relative time string ("just now", "2m ago", "1h ago").
pub(super) fn relative_time(ts: i64) -> String {
    if ts == 0 {
        return "—".to_string();
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

/// After SIGTERMing a background worker, immediately write Cancelled to meta.json
/// so the next sync tick doesn't revert the status.
pub(super) fn kill_write_cancelled(run_id: &str) {
    if let Ok(mut meta) = runstate::read_meta(run_id) {
        meta.status = runstate::RunStatus::Cancelled;
        meta.touch();
        let _ = runstate::write_meta(&meta);
    }
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        s.to_string()
    } else {
        // Find the nearest char boundary at or before `max` to avoid
        // panicking on multi-byte UTF-8 characters (e.g. em-dashes).
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
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
        return "—".to_string();
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
        return "—".to_string();
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

/// Copy `text` to the system clipboard.  Returns `true` on success.
///
/// Strategy (in order):
/// 1. `pbcopy` (macOS)
/// 2. `xclip -selection clipboard` (Linux X11)
/// 3. `wl-copy` (Linux Wayland)
/// 4. OSC52 via /dev/tty → stdout fallback
pub(super) fn yank_to_clipboard(text: &str) -> bool {
    yank_to_clipboard_via(text, osc52_fallback)
}

/// OSC52 clipboard fallback, delegating to the real terminal write in
/// `leviath_sys`.
///
/// COVERAGE-EXCLUDED: this is a cli-local safety twin, NOT the OS mechanism
/// itself (that lives, fully tested, in `leviath_sys::tty`). It exists only
/// because `leviath_sys` compiles as a *non-test* dependency even in this
/// crate's test build, so a cli test that reached the real `osc52_yank` would
/// write OSC escape bytes to the terminal running `cargo test`. The
/// `#[cfg(test)]` twin below keeps those tests (and `input.rs`'s `key('y')`
/// handler tests) from ever touching a real terminal.
#[cfg(not(test))]
fn osc52_fallback(text: &str) -> bool {
    leviath_sys::osc52_yank(text)
}

#[cfg(test)]
fn osc52_fallback(_text: &str) -> bool {
    true
}

/// The native clipboard tools tried, in order, before falling back to OSC52.
const NATIVE_CLIPBOARD_CMDS: &[(&str, &[&str])] = &[
    ("pbcopy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("wl-copy", &[]),
];

/// Core `yank_to_clipboard` logic, parameterized over the OSC52 fallback so
/// tests that force this path (e.g. by starving `PATH`) can inject a fake
/// that never touches the real TTY.
fn yank_to_clipboard_via(text: &str, osc52_fallback: fn(&str) -> bool) -> bool {
    yank_to_clipboard_with(text, NATIVE_CLIPBOARD_CMDS, osc52_fallback)
}

/// [`yank_to_clipboard_via`] with the native-tool command list injected, so a
/// test can drive the spawn-success and non-zero-exit branches with a program
/// guaranteed present on the host (the real `pbcopy`/`xclip`/`wl-copy` names
/// don't exist on Windows, so a fake `#!/bin/sh` script on `PATH` -- the prior
/// approach -- couldn't exercise these branches there).
fn yank_to_clipboard_with(
    text: &str,
    clipboard_cmds: &[(&str, &[&str])],
    osc52_fallback: fn(&str) -> bool,
) -> bool {
    use std::io::Write as IoWrite;
    use std::process::{Command, Stdio};

    // Try native clipboard tools first — most reliable
    for (cmd, args) in clipboard_cmds {
        if let Ok(mut child) = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            // `child.stdin` is guaranteed `Some` here because the child was
            // spawned with `.stdin(Stdio::piped())` above -- an `if let`
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

    // ─── PATH env var save/restore helper for clipboard tests ────────────────
    //
    // Several tests below shadow `PATH` with a directory of fake clipboard
    // binaries and must restore the original value afterwards -- or, if
    // `PATH` was unset beforehand, remove it again rather than setting it to
    // an empty string. Shared here (both branches covered: the `Some` arm by
    // every call site below, the `None` arm by
    // `test_restore_path_removes_path_when_originally_unset`) so the "was
    // originally unset" branch only needs covering once instead of at every
    // call site.
    fn restore_path(original: Option<std::ffi::OsString>) {
        unsafe {
            match original {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn test_restore_path_removes_path_when_originally_unset() {
        let _lock = crate::config::PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let real_original_path = std::env::var_os("PATH");

        restore_path(None);
        assert!(std::env::var_os("PATH").is_none());

        // Put the real PATH back so no other test in this process is affected.
        restore_path(real_original_path);
    }

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
        assert_eq!(relative_time(0), "—");
    }

    #[test]
    fn test_elapsed_str_zero() {
        assert_eq!(elapsed_str(0), "—");
    }

    #[test]
    fn test_elapsed_str_until_zero() {
        assert_eq!(elapsed_str_until(0, 100), "—");
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
        assert_eq!(elapsed_str_until(0, 60), "—");
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
    // A previous version of this test ("test_yank_to_clipboard_small_text")
    // called `yank_to_clipboard_via` with the ambient, untouched `PATH` and
    // relied on whichever native tool happened to be installed -- which made
    // whether its nested fallback closure ever actually ran (and thus
    // whether it was ever covered) depend on the machine `cargo test`
    // happened to run on. Both the "native tool succeeds" and "falls back to
    // OSC52" branches of `yank_to_clipboard_via` are already covered
    // deterministically below (`..._native_tool_success_returns_true_...`,
    // `..._nonzero_exit_falls_through_to_fallback`,
    // `..._falls_back_to_osc52_when_no_native_tool_on_path`), so that smoke
    // test added no unique coverage -- it's removed rather than made
    // deterministic by also starving `PATH`, to avoid growing the number of
    // `PATH`-mutation windows tests not holding `PATH_ENV_LOCK` (e.g. the
    // dashboard's real `key('y')` handlers in `input.rs`) could race with.

    #[test]
    fn test_yank_to_clipboard_empty() {
        // Starves `PATH` so the injected fallback is reached deterministically
        // regardless of which native clipboard tools happen to be installed
        // on the machine `cargo test` runs on (see the removed
        // `test_yank_to_clipboard_small_text`/`..._returns_true` tests'
        // replacement comments above for why ambient-`PATH` smoke tests are
        // avoided here).
        let _lock = crate::config::PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fn fake_osc52_fallback(_text: &str) -> bool {
            true
        }
        let original_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", "/lev-definitely-empty-path-dir");
        }
        let result = yank_to_clipboard_via("", fake_osc52_fallback);
        restore_path(original_path);
        assert!(result);
    }

    // ── yank_to_clipboard_via: native-tool success branch ────────────────────

    // Module-scoped (rather than nested inside the test below) so its body
    // can also be exercised directly by
    // `test_unreachable_osc52_fallback_panics_if_ever_invoked` -- both
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
    /// stdin and exit deterministically), replacing the prior Unix-only
    /// `#!/bin/sh` fake scripts.
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
        // Holds PATH_ENV_LOCK because this depends on an intact `PATH` (to
        // resolve `true`/`cmd`) and must not race a PATH-starving test.
        let _lock = crate::config::PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (cmd, args) = exit_cmd(true);
        let result = yank_to_clipboard_with(
            "native tool success test",
            &[(cmd, args)],
            unreachable_osc52_fallback,
        );
        assert!(result);
    }

    #[test]
    fn test_yank_to_clipboard_native_tool_nonzero_exit_falls_through_to_fallback() {
        // A guaranteed-present command that exits non-zero makes
        // `child.wait().map(|s| s.success())` false, so the loop falls through
        // to the OSC52 fallback -- the branch the success test doesn't reach.
        let _lock = crate::config::PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fn fallback_reached(_text: &str) -> bool {
            true
        }
        let (cmd, args) = exit_cmd(false);
        let result = yank_to_clipboard_with("nonzero exit test", &[(cmd, args)], fallback_reached);
        // The native tool "ran" but failed, so control reached the fallback.
        assert!(result);
    }

    // ─── kill_write_cancelled ───────────────────────────────────────────────

    #[test]
    fn test_kill_write_cancelled_updates_existing_meta() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_kill_write_cancelled_updates_existing_meta",
        );
        let run_id = "test-kill-write-cancelled";
        let meta = runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        kill_write_cancelled(run_id);

        let after = runstate::read_meta(run_id).unwrap();
        assert_eq!(after.status, runstate::RunStatus::Cancelled);

        let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
    }

    #[test]
    fn test_kill_write_cancelled_missing_run_is_noop() {
        // No meta.json exists for this run_id — read_meta fails, the `if let
        // Ok` guard should just skip without panicking.
        kill_write_cancelled("test-kill-write-cancelled-nonexistent");
    }

    // ─── yank_to_clipboard ──────────────────────────────────────────────────
    //
    // A previous version of this test ("test_yank_to_clipboard_returns_true")
    // called `yank_to_clipboard_via` with the ambient, untouched `PATH`,
    // relying on the real `pbcopy` this dev machine happens to have
    // installed. Its nested fallback closure only got covered on runs where
    // a concurrently-running `PATH`-mutating test happened to race with it --
    // nondeterministic, and the exact kind of flakiness this file's
    // `PATH_ENV_LOCK`-guarded tests exist to avoid. `yank_to_clipboard`
    // itself (the public, un-suffixed wrapper this test also indirectly
    // covered) is still exercised for real by the dashboard's `key('y')`
    // handler tests in `input.rs`; the native-success and OSC52-fallback
    // branches of `yank_to_clipboard_via` it delegates to are covered
    // deterministically by the tests immediately below.

    #[test]
    fn test_yank_to_clipboard_falls_back_to_osc52_when_no_native_tool_on_path() {
        // `PATH` is process-global; `crate::config::PATH_ENV_LOCK` serializes
        // against `commands/run/session.rs`'s PATH-mutating `launch_editor`
        // tests too (previously each side only held its own file-local lock,
        // which doesn't actually serialize across files despite the shared
        // name).
        let _lock = crate::config::PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Starve PATH so `Command::new("pbcopy"/"xclip"/"wl-copy").spawn()`
        // fails with NotFound for all three -- the `if let Ok(mut child) = `
        // guard is simply false each time (no external process is ever
        // launched), falling through to the OSC52 fallback. That fallback is
        // injected as a fn pointer that never touches the real TTY (unlike
        // the real `osc52_yank_raw`, which would open `/dev/tty` here).
        fn fake_osc52_fallback(_text: &str) -> bool {
            true
        }
        let original_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", "/lev-definitely-empty-path-dir");
        }
        let result = yank_to_clipboard_via("fallback path test content", fake_osc52_fallback);
        restore_path(original_path);
        // The point of this test is proving the native-tool loop was skipped
        // entirely and control reached the injected OSC52 fallback -- not
        // exercising the real `osc52_yank_raw` (see its own tests above).
        assert!(result);
    }

    // ─── yank_to_clipboard (the real, un-suffixed wrapper) ───────────────────

    #[test]
    fn test_yank_to_clipboard_delegates_to_osc52_fallback_twin() {
        // Calls the real, un-suffixed `yank_to_clipboard` (unlike the tests
        // above, which call `yank_to_clipboard_via` with an injected fake) so
        // its one-line delegation to `osc52_fallback` is covered. Safe because
        // `osc52_fallback` is `#[cfg(test)]`-twinned to a no-op that returns
        // `true` without touching a real terminal — the real OSC52 write and
        // all of its branches are tested in `leviath_sys::tty`. Starving `PATH`
        // skips the native-tool loop so control reaches the fallback.
        let _lock = crate::config::PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", "/lev-definitely-empty-path-dir");
        }
        let result = yank_to_clipboard("real wrapper test content");
        restore_path(original_path);
        assert!(result);
    }
}
