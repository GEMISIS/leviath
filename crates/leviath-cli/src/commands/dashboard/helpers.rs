//! Pure utility functions used across the dashboard.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// Fit `s` into `max` terminal columns, ending in an ellipsis when it had to
/// be cut. The ellipsis is part of the budget, so the result never draws wider
/// than `max`.
///
/// Measured in display columns, not bytes: counting bytes cuts a title full of
/// long dashes or emoji at a third of the room it was given, and a wide
/// character counts for the two cells it occupies.
pub(super) fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let keep = max - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > keep {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// The fewest columns a shrinkable part keeps before the next one is touched.
pub(super) const FIT_FLOOR: usize = 8;

/// Fit a line made of `parts` into `width` columns.
///
/// Returned unchanged when the whole line fits. Otherwise the parts named in
/// `shrink_order` (least important first) are cut with [`truncate`], each down
/// to [`FIT_FLOOR`] columns before the next one is touched, and the walk stops
/// as soon as the line fits. Parts not named never change, so a caller decides
/// exactly what gives way and in what order. When every shrinkable part sits
/// at the floor and the line is still too long, what remains is left to the
/// widget to clip.
pub(super) fn fit_parts(parts: &[String], width: usize, shrink_order: &[usize]) -> Vec<String> {
    let mut out: Vec<String> = parts.to_vec();
    for &idx in shrink_order {
        let total: usize = out.iter().map(|p| p.width()).sum();
        if total <= width {
            break;
        }
        let excess = total - width;
        let current = out[idx].width();
        let target = current.saturating_sub(excess).max(FIT_FLOOR.min(current));
        out[idx] = truncate(&out[idx], target);
    }
    out
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
    use std::process::Stdio;

    // Try native clipboard tools first - most reliable
    for (cmd, args) in clipboard_cmds {
        let mut command = leviath_sys::child_command(cmd);
        command
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // The dashboard is a full-screen TUI; a console window popping over it
        // on every yank would be the worst place for one.
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
        assert_eq!(result, "hell…");
        assert_eq!(result.width(), 5);
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
        assert_eq!(result, "ab…");
    }

    #[test]
    fn test_truncate_multibyte_counts_columns_not_bytes() {
        // The en dash is 3 bytes but one column. Byte counting cut this line
        // at "Research the histo" when asked for 22; column counting keeps
        // 21 characters plus the ellipsis.
        let s = "Research the history – covering all topics";
        assert_eq!(truncate(s, 22), "Research the history …");
        assert_eq!(truncate(s, 21), "Research the history…");
        assert_eq!(truncate(s, 24), "Research the history – …");
        assert_eq!(truncate(s, 24).width(), 24);
    }

    #[test]
    fn test_truncate_emoji_is_two_columns() {
        // 🔥 is 4 bytes and 2 columns. The budget is spent in columns: with 7
        // there is room for "Hello " (6) but not the 2-wide emoji, and with 9
        // the emoji fits and the ellipsis follows it.
        let s = "Hello 🔥 world";
        assert_eq!(truncate(s, 7), "Hello …");
        assert_eq!(truncate(s, 9), "Hello 🔥…");
        assert_eq!(truncate(s, 9).width(), 9);
        // Wholly multi-byte text: 13 columns wide, cut to 6.
        assert_eq!(truncate(&"…".repeat(13), 6), "…".repeat(6));
    }

    #[test]
    fn test_truncate_zero_width_is_empty() {
        // No room for even the ellipsis: draw nothing rather than one column
        // more than was offered.
        assert_eq!(truncate("hello", 0), "");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn test_truncate_one_column_is_the_ellipsis() {
        assert_eq!(truncate("hello", 1), "…");
    }

    fn parts(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fit_parts_leaves_a_fitting_line_alone() {
        let p = parts(&[
            "dir ",
            "~/projects/leviath",
            " · ",
            "openrouter/z-ai/glm-5.3",
        ]);
        assert_eq!(fit_parts(&p, 80, &[3, 1]), p);
        // Exactly full is still fitting.
        let total: usize = p.iter().map(|s| s.width()).sum();
        assert_eq!(fit_parts(&p, total, &[3, 1]), p);
    }

    #[test]
    fn fit_parts_shrinks_the_first_named_part_only_when_it_is_enough() {
        let p = parts(&[
            "dir ",
            "~/projects/leviath",
            " · ",
            "openrouter/z-ai/glm-5.3",
        ]);
        // 4 + 18 + 3 + 23 = 48; ten columns over, so the model loses ten.
        let out = fit_parts(&p, 38, &[3, 1]);
        assert_eq!(out[0], "dir ");
        assert_eq!(out[1], "~/projects/leviath");
        assert_eq!(out[2], " · ");
        assert_eq!(out[3], "openrouter/z…");
        assert_eq!(out.iter().map(|s| s.width()).sum::<usize>(), 38);
    }

    #[test]
    fn fit_parts_moves_to_the_second_part_after_the_first_hits_the_floor() {
        let p = parts(&[
            "dir ",
            "~/projects/leviath",
            " · ",
            "openrouter/z-ai/glm-5.3",
        ]);
        // 48 wide, 28 asked: the model can only give 15 (down to 8), so the
        // workdir gives the remaining 5.
        let out = fit_parts(&p, 28, &[3, 1]);
        assert_eq!(out[3], "openrou…");
        assert_eq!(out[3].width(), FIT_FLOOR);
        assert_eq!(out[1], "~/projects/l…");
        assert_eq!(out.iter().map(|s| s.width()).sum::<usize>(), 28);
    }

    #[test]
    fn fit_parts_stops_at_the_floor_and_never_touches_unnamed_parts() {
        let p = parts(&[
            "dir ",
            "~/projects/leviath",
            " · ",
            "openrouter/z-ai/glm-5.3",
        ]);
        // Far too narrow: both named parts sit at the floor, the fixed parts
        // are intact, and the line is simply still too long.
        let out = fit_parts(&p, 10, &[3, 1]);
        assert_eq!(out[0], "dir ");
        assert_eq!(out[2], " · ");
        assert_eq!(out[1].width(), FIT_FLOOR);
        assert_eq!(out[3].width(), FIT_FLOOR);
    }

    #[test]
    fn fit_parts_floor_does_not_grow_a_part_shorter_than_it() {
        // A part already narrower than the floor is left as it is rather than
        // padded, and the excess moves on to the next named part.
        let p = parts(&["ab", "0123456789abcdef"]);
        let out = fit_parts(&p, 12, &[0, 1]);
        assert_eq!(out[0], "ab");
        assert_eq!(out[1], "012345678…");
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
