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
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }

    // Fall back to OSC52
    osc52_yank_raw(text)
}

/// Yank via the OSC52 terminal escape sequence — last-resort fallback.
pub(super) fn osc52_yank_raw(text: &str) -> bool {
    use std::io::Write;
    // Base64-encode the content
    let encoded = {
        use std::fmt::Write as FmtWrite;
        let bytes = text.as_bytes();
        let mut out = String::with_capacity((bytes.len() * 4 / 3) + 8);
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i + 3 <= bytes.len() {
            let b0 = bytes[i] as usize;
            let b1 = bytes[i + 1] as usize;
            let b2 = bytes[i + 2] as usize;
            let _ = FmtWrite::write_char(&mut out, TABLE[b0 >> 2] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[((b0 & 3) << 4) | (b1 >> 4)] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[((b1 & 0xf) << 2) | (b2 >> 6)] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[b2 & 0x3f] as char);
            i += 3;
        }
        let rem = bytes.len() - i;
        if rem == 1 {
            let b0 = bytes[i] as usize;
            let _ = FmtWrite::write_char(&mut out, TABLE[b0 >> 2] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[(b0 & 3) << 4] as char);
            out.push_str("==");
        } else if rem == 2 {
            let b0 = bytes[i] as usize;
            let b1 = bytes[i + 1] as usize;
            let _ = FmtWrite::write_char(&mut out, TABLE[b0 >> 2] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[((b0 & 3) << 4) | (b1 >> 4)] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[(b1 & 0xf) << 2] as char);
            out.push('=');
        }
        out
    };
    let osc = format!("\x1b]52;c;{}\x07", encoded);
    // Write directly to /dev/tty to bypass ratatui's raw mode stdout handling.
    #[cfg(unix)]
    {
        if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
            let _ = tty.write_all(osc.as_bytes());
            let _ = tty.flush();
            return true;
        }
    }
    // Fallback: write to stdout
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(osc.as_bytes());
    let _ = stdout.flush();
    true
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
}
