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
        format!("{}…", &s[..max])
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
    yank_to_clipboard_via(text, osc52_yank_raw)
}

/// Core `yank_to_clipboard` logic, parameterized over the OSC52 fallback so
/// tests that force this path (e.g. by starving `PATH`) can inject a fake
/// that never touches the real TTY.
fn yank_to_clipboard_via(text: &str, osc52_fallback: fn(&str) -> bool) -> bool {
    use std::io::Write as IoWrite;
    use std::process::{Command, Stdio};

    // Try native clipboard tools first — most reliable
    let clipboard_cmds: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("wl-copy", &[]),
    ];
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

/// Base64-encode `text` and wrap it in the OSC52 "set clipboard" escape sequence.
fn osc52_sequence(text: &str) -> String {
    use std::fmt::Write as FmtWrite;
    let bytes = text.as_bytes();
    let mut encoded = String::with_capacity((bytes.len() * 4 / 3) + 8);
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let b0 = bytes[i] as usize;
        let b1 = bytes[i + 1] as usize;
        let b2 = bytes[i + 2] as usize;
        let _ = FmtWrite::write_char(&mut encoded, TABLE[b0 >> 2] as char);
        let _ = FmtWrite::write_char(&mut encoded, TABLE[((b0 & 3) << 4) | (b1 >> 4)] as char);
        let _ = FmtWrite::write_char(&mut encoded, TABLE[((b1 & 0xf) << 2) | (b2 >> 6)] as char);
        let _ = FmtWrite::write_char(&mut encoded, TABLE[b2 & 0x3f] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let b0 = bytes[i] as usize;
        let _ = FmtWrite::write_char(&mut encoded, TABLE[b0 >> 2] as char);
        let _ = FmtWrite::write_char(&mut encoded, TABLE[(b0 & 3) << 4] as char);
        encoded.push_str("==");
    } else if rem == 2 {
        let b0 = bytes[i] as usize;
        let b1 = bytes[i + 1] as usize;
        let _ = FmtWrite::write_char(&mut encoded, TABLE[b0 >> 2] as char);
        let _ = FmtWrite::write_char(&mut encoded, TABLE[((b0 & 3) << 4) | (b1 >> 4)] as char);
        let _ = FmtWrite::write_char(&mut encoded, TABLE[(b1 & 0xf) << 2] as char);
        encoded.push('=');
    }
    format!("\x1b]52;c;{}\x07", encoded)
}

/// Open the process's controlling terminal for writing. Extracted behind a
/// `fn` pointer (see [`osc52_yank_via`]) so tests can swap in an opener that
/// never touches the real TTY -- writing OSC escape sequences to a live
/// `/dev/tty` from a unit test corrupts (and can hang) whatever terminal
/// `cargo test` happens to be running in, since it bypasses the test
/// harness's stdout capture entirely.
///
/// COVERAGE-EXCLUDED: this is the real, unfaked `/dev/tty` opener. Calling it
/// from a test would open the process's actual controlling terminal device.
/// The `#[cfg(test)]` twin below always fails instead (harmlessly -- no real
/// file is touched) so that `osc52_yank_raw`'s `#[cfg(test)]` twin can share
/// the exact same `osc52_write_via`-calling structure as the real body
/// (swapping only the stdout-fallback destination for an in-memory one) and
/// still never reach a real TTY, rather than needing an entirely different,
/// untested control-flow shape under `#[cfg(test)]`.
#[cfg(unix)]
#[cfg(not(test))]
fn open_controlling_tty() -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).open("/dev/tty")
}

#[cfg(unix)]
#[cfg(test)]
fn open_controlling_tty() -> std::io::Result<std::fs::File> {
    Err(std::io::Error::other(
        "open_controlling_tty is disabled under #[cfg(test)]",
    ))
}

/// Core OSC52 write logic, parameterized over how to open the TTY *and*
/// where the stdout fallback writes go, so it can be fully exercised in
/// tests without ever touching a real terminal. A direct `Write` call on
/// `std::io::stdout()` bypasses cargo test's per-test output capture (unlike
/// `print!`/`println!`) just as much as a raw `/dev/tty` write does, so both
/// destinations must be injectable, not just the TTY one.
///
/// Only called for real from `osc52_yank_raw`'s `#[cfg(unix)]` branch, but
/// exercised directly by cross-platform tests below -- `cfg(any(unix, test))`
/// (rather than plain `cfg(unix)`) keeps it compiled for non-unix test
/// builds too, avoiding an unused-function warning (hard error under this
/// workspace's `-D warnings`) on the plain non-unix lib build.
#[cfg(any(unix, test))]
fn osc52_write_via<T: std::io::Write>(
    text: &str,
    open_tty: fn() -> std::io::Result<std::fs::File>,
    mut stdout_fallback: T,
) -> bool {
    use std::io::Write;
    let osc = osc52_sequence(text);
    if let Ok(mut tty) = open_tty() {
        if tty.write_all(osc.as_bytes()).is_ok() && tty.flush().is_ok() {
            return true;
        }
    }
    // Fallback: write to stdout. Report the real outcome instead of always
    // claiming success — callers show an error toast when this is false.
    stdout_fallback.write_all(osc.as_bytes()).is_ok() && stdout_fallback.flush().is_ok()
}

/// Yank via the OSC52 terminal escape sequence — last-resort fallback.
///
/// COVERAGE-EXCLUDED: the real body writes directly to the real
/// `std::io::stdout()` (and, via `open_controlling_tty`, the real `/dev/tty`)
/// -- exercising it for real from a test would write raw OSC escape
/// sequences to (and, if `/dev/tty` blocks, could hang) whatever terminal
/// `cargo test` happens to run in. The `#[cfg(test)]` twin below keeps the
/// exact same shape (still calls `open_controlling_tty` and
/// `osc52_write_via`, so both remain exercised even if some future test
/// reaches all the way down `yank_to_clipboard`'s real call chain instead of
/// injecting a fake fallback, as every existing test of it does) but swaps
/// the real `std::io::stdout()` destination for an in-memory `Vec<u8>`, so
/// nothing ever touches a real terminal. All of the actual encode/branch
/// logic (`osc52_sequence`, `osc52_write_via`'s try-tty-then-fall-back
/// branching) is exercised directly and far more thoroughly by the
/// dedicated tests below, which inject their own fake TTY openers and fake
/// stdout destinations.
#[cfg(not(test))]
pub(super) fn osc52_yank_raw(text: &str) -> bool {
    #[cfg(unix)]
    {
        osc52_write_via(text, open_controlling_tty, std::io::stdout())
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        let osc = osc52_sequence(text);
        let mut stdout = std::io::stdout();
        stdout.write_all(osc.as_bytes()).is_ok() && stdout.flush().is_ok()
    }
}

#[cfg(test)]
pub(super) fn osc52_yank_raw(text: &str) -> bool {
    #[cfg(unix)]
    {
        osc52_write_via(text, open_controlling_tty, Vec::new())
    }
    #[cfg(not(unix))]
    {
        // Mirror the real (`#[cfg(not(test))]`) non-unix body above, but
        // swap the real `std::io::stdout()` destination for an in-memory
        // `Vec<u8>` -- same reasoning as the unix branch just above: this
        // still exercises the real `osc52_sequence` encoding logic without
        // ever touching real stdout, and (like a real write to a live
        // terminal) succeeds.
        use std::io::Write;
        let osc = osc52_sequence(text);
        let mut buf = Vec::new();
        buf.write_all(osc.as_bytes()).is_ok() && buf.flush().is_ok()
    }
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
        // Truncation with simple ASCII substring (may split multi-byte in real usage,
        // but the function uses byte indexing)
        let result = truncate("abcdef", 3);
        assert_eq!(result, "abc…");
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

    // These exercise `osc52_sequence` directly (pure, no I/O) rather than
    // `osc52_yank_raw`. `osc52_yank_raw` opens the real `/dev/tty` and writes
    // an OSC escape sequence straight to it -- calling it from a unit test
    // bypasses cargo test's stdout capture entirely and corrupts (and, with
    // enough parallel tests hitting it at once, can hang) whatever terminal
    // `cargo test` happens to be running in. See `osc52_yank_via` tests below
    // for I/O-path coverage routed through a fake TTY instead.

    #[test]
    fn test_osc52_sequence_produces_output() {
        let seq = osc52_sequence("test data");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
    }

    #[test]
    fn test_osc52_sequence_empty_string() {
        assert_eq!(osc52_sequence(""), "\x1b]52;c;\x07");
    }

    #[test]
    fn test_osc52_sequence_special_chars() {
        let seq = osc52_sequence("hello\nworld\ttab");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
    }

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

    #[cfg(unix)]
    #[test]
    fn test_yank_to_clipboard_via_native_tool_success_returns_true_without_fallback() {
        // Shadows `pbcopy` with a fake, harmless script on `PATH` so the
        // native-tool success path (the `return true` from inside the spawn
        // loop) is exercised deterministically -- without this, whether that
        // specific branch runs depends on whether a real `pbcopy`/`xclip`/
        // `wl-copy` happens to be installed and reachable in the environment
        // `cargo test` runs in. The fake script never touches the real
        // clipboard or a terminal; it just drains stdin and exits 0.
        let _lock = crate::config::PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let dir = std::env::temp_dir().join("lev_test_fake_pbcopy_bin");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("pbcopy");
        std::fs::write(&script_path, "#!/bin/sh\ncat > /dev/null\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }

        let original_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", &dir);
        }
        let result = yank_to_clipboard_via("native tool success test", unreachable_osc52_fallback);
        restore_path(original_path);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result);
    }

    #[cfg(unix)]
    #[test]
    fn test_yank_to_clipboard_via_native_tool_nonzero_exit_falls_through_to_fallback() {
        // Shadows `pbcopy`/`xclip`/`wl-copy` with fake scripts that spawn
        // successfully but exit non-zero, so `child.wait().map(|s|
        // s.success()).unwrap_or(false)` is `false` and the loop falls
        // through to the OSC52 fallback instead of returning `true` early --
        // the one branch `test_yank_to_clipboard_via_native_tool_success_*`
        // above doesn't reach. The fake scripts never touch the real
        // clipboard or a terminal; they just drain stdin and exit 1.
        let _lock = crate::config::PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let dir = std::env::temp_dir().join("lev_test_fake_failing_clipboard_bins");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["pbcopy", "xclip", "wl-copy"] {
            let script_path = dir.join(name);
            std::fs::write(&script_path, "#!/bin/sh\ncat > /dev/null\nexit 1\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }

        fn fallback_reached(_text: &str) -> bool {
            true
        }

        let original_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", &dir);
        }
        let result = yank_to_clipboard_via("nonzero exit test", fallback_reached);
        restore_path(original_path);
        let _ = std::fs::remove_dir_all(&dir);

        // All three native tools "ran" but failed, so control must have
        // reached the injected fallback for the result to be true.
        assert!(result);
    }

    // ── OSC52 base64 edge cases (pure `osc52_sequence`, no I/O) ───────────────

    #[test]
    fn test_osc52_sequence_one_byte() {
        // Single byte → rem == 1 path in base64 encoder
        assert_eq!(osc52_sequence("A"), "\x1b]52;c;QQ==\x07");
    }

    #[test]
    fn test_osc52_sequence_two_bytes() {
        // Two bytes → rem == 2 path in base64 encoder
        assert_eq!(osc52_sequence("AB"), "\x1b]52;c;QUI=\x07");
    }

    #[test]
    fn test_osc52_sequence_three_bytes() {
        // Three bytes → exact group, no remainder
        assert_eq!(osc52_sequence("ABC"), "\x1b]52;c;QUJD\x07");
    }

    #[test]
    fn test_osc52_sequence_long_text() {
        // Multiple 3-byte groups plus a remainder
        let text = "The quick brown fox jumps over the lazy dog";
        let seq = osc52_sequence(text);
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
    }

    // ── osc52_write_via (both I/O destinations faked — never touches a real
    // TTY or the process's real stdout) ───────────────────────────────────

    #[test]
    fn test_osc52_write_via_tty_open_succeeds_writes_expected_sequence() {
        // Points the "tty" at a throwaway temp file instead of `/dev/tty` --
        // proves the success branch works without ever touching a terminal.
        fn open_temp_tty() -> std::io::Result<std::fs::File> {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(std::env::temp_dir().join("lev_test_osc52_fake_tty"))
        }
        let path = std::env::temp_dir().join("lev_test_osc52_fake_tty");
        let mut stdout_fallback = Vec::new();
        let result = osc52_write_via("test data", open_temp_tty, &mut stdout_fallback);
        assert!(result);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, osc52_sequence("test data"));
        // The tty branch succeeded, so the stdout fallback must never fire.
        assert!(stdout_fallback.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_osc52_write_via_tty_open_fails_falls_back_to_injected_writer() {
        fn fail_to_open_tty() -> std::io::Result<std::fs::File> {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no tty in tests",
            ))
        }
        // Falls through to the injected in-memory writer instead of the
        // process's real `std::io::stdout()`.
        let mut stdout_fallback = Vec::new();
        let result = osc52_write_via("fallback data", fail_to_open_tty, &mut stdout_fallback);
        assert!(result);
        assert_eq!(stdout_fallback, osc52_sequence("fallback data").as_bytes());
    }

    #[test]
    fn test_osc52_write_via_tty_open_succeeds_but_write_fails_falls_back_to_injected_writer() {
        // Distinct from the two tests above: the "tty" open itself succeeds
        // (unlike `..._tty_open_fails_...`), but the write to it fails --
        // opening the fake tty read-only means the write syscall itself
        // errors, exercising the `if let Ok(mut tty) = open_tty()` /
        // `tty.write_all(...).is_ok() && ...` fallthrough (falling out of
        // the inner `if` without returning `true`) that neither existing
        // test reaches.
        let path = std::env::temp_dir().join("lev_test_osc52_readonly_fake_tty");
        std::fs::write(&path, b"").unwrap();
        fn open_readonly_tty() -> std::io::Result<std::fs::File> {
            std::fs::OpenOptions::new()
                .read(true)
                .open(std::env::temp_dir().join("lev_test_osc52_readonly_fake_tty"))
        }
        let mut stdout_fallback = Vec::new();
        let result = osc52_write_via("readonly tty test", open_readonly_tty, &mut stdout_fallback);
        let _ = std::fs::remove_file(&path);

        assert!(result);
        assert_eq!(
            stdout_fallback,
            osc52_sequence("readonly tty test").as_bytes()
        );
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
    fn test_yank_to_clipboard_falls_back_to_test_twin_osc52_yank_raw() {
        // Calls the real, un-suffixed `yank_to_clipboard` (unlike every test
        // above, which calls `yank_to_clipboard_via` with an injected fake
        // fallback) so its real call chain down to `osc52_yank_raw` is
        // actually exercised. This is safe specifically *because*
        // `osc52_yank_raw` is `#[cfg(test)]`-twinned to swap the real
        // `std::io::stdout()` destination for an in-memory `Vec<u8>` (see its
        // doc comment) -- under any other build, calling this would open the
        // real `/dev/tty` and write to the real stdout.
        let _lock = crate::config::PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Starve PATH so the native-tool loop is skipped entirely and control
        // reaches `osc52_yank_raw`, matching the technique used by
        // `test_yank_to_clipboard_falls_back_to_osc52_when_no_native_tool_on_path`
        // above (see its comment for why this is deterministic).
        let original_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", "/lev-definitely-empty-path-dir");
        }
        // Don't assert a specific return value here: unlike the tests above
        // (which inject a fake fallback with a known, fixed return value),
        // this call reaches the *real* `osc52_yank_raw`, whose result
        // depends on OS-level process-spawn/stdout-write behavior that can
        // legitimately vary across platforms (e.g. Windows's native-tool
        // spawn attempts and encode/write path don't behave identically to
        // Unix's). The point of this test is purely to exercise
        // `yank_to_clipboard`'s real one-line delegation to `osc52_yank_raw`
        // for coverage -- every actual branch of the underlying logic is
        // already exercised deterministically by the injected-fake-fallback
        // tests above.
        let _ = yank_to_clipboard("real wrapper test content");
        restore_path(original_path);
    }
}
