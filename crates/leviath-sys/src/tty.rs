//! Terminal clipboard support via the OSC52 escape sequence.
//!
//! OSC52 is a last-resort clipboard mechanism: it asks the *terminal emulator*
//! to set the system clipboard, so it works over SSH and without any native
//! clipboard tool. The bytes must reach the real terminal — the controlling
//! `/dev/tty` or stdout.
//!
//! This module holds the pure, fully-tested pieces: [`osc52_sequence`] (the
//! base64/escape encoding) and [`osc52_write_via`] (the tty-then-stdout branch
//! logic, parameterized over the tty opener and the fallback sink so every
//! branch is exercised via injected fakes). The genuinely-untestable real-I/O
//! leaves (opening `/dev/tty`, writing the real `stdout()`) are composed in the
//! CLI binary — see `real_yank` in `crates/leviath-cli/src/main.rs`.

use std::fs::File;
use std::io::{self, Write};

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

/// Core OSC52 write logic, parameterized over how to open the TTY and where the
/// stdout fallback writes, so every branch is exercisable without touching a
/// real terminal. `pub` so the CLI binary can compose it with the real
/// `/dev/tty` opener + real `stdout()` (the un-unit-testable leaves) — see
/// `real_yank` in `crates/leviath-cli/src/main.rs`.
///
/// The fallback is a `&mut dyn Write` (not a generic `T: Write`) deliberately:
/// a single vtable dispatch on this cold, human-driven path costs nothing, and
/// a non-generic function has exactly one coverage-mapping instance instead of
/// one per caller type — no phantom-uncovered monomorphization for llvm-cov.
pub fn osc52_write_via(
    text: &str,
    open_tty: fn() -> io::Result<File>,
    stdout_fallback: &mut dyn Write,
) -> bool {
    let osc = osc52_sequence(text);
    if let Ok(mut tty) = open_tty() {
        if tty.write_all(osc.as_bytes()).is_ok() && tty.flush().is_ok() {
            return true;
        }
    }
    // Fallback: write to stdout. Report the real outcome so callers can show an
    // error when even this fails.
    stdout_fallback.write_all(osc.as_bytes()).is_ok() && stdout_fallback.flush().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_sequence_encodes_all_remainder_lengths() {
        assert_eq!(osc52_sequence("foo"), "\x1b]52;c;Zm9v\x07"); // rem == 0
        assert_eq!(osc52_sequence("f"), "\x1b]52;c;Zg==\x07"); // rem == 1
        assert_eq!(osc52_sequence("fo"), "\x1b]52;c;Zm8=\x07"); // rem == 2
        assert!(osc52_sequence("").starts_with("\x1b]52;c;"));
    }

    fn open_fails() -> io::Result<File> {
        Err(io::Error::other("no tty"))
    }

    // Cross-platform stand-ins for a real TTY: a writable temp file (accepts
    // writes) and a read-only temp file (writes to it fail). Using a temp file
    // rather than `/dev/null` keeps these tests — and thus `osc52_write_via`'s
    // tty-success and tty-write-fails branches — covered on every OS, not just
    // Unix.
    fn open_temp_writable() -> io::Result<File> {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(std::env::temp_dir().join("lev_sys_osc52_fake_tty_w"))
    }

    fn open_temp_readonly() -> io::Result<File> {
        let path = std::env::temp_dir().join("lev_sys_osc52_fake_tty_ro");
        let _ = std::fs::write(&path, b"");
        std::fs::OpenOptions::new().read(true).open(&path)
    }

    /// Fails on `write` (so the `&&` short-circuits before `flush`).
    struct WriteFailWriter;
    impl Write for WriteFailWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write fail"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Accepts `write` but fails on `flush` (exercises the flush half of the `&&`).
    struct FlushFailWriter {
        buf: Vec<u8>,
    }
    impl Write for FlushFailWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.buf.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("flush fail"))
        }
    }

    #[test]
    fn write_via_returns_true_when_tty_write_succeeds() {
        // The writable temp file accepts the write: exercises the "tty write
        // succeeded, return early" branch with no real terminal involved.
        let mut sink = Vec::new();
        assert!(osc52_write_via("hi", open_temp_writable, &mut sink));
        assert!(
            sink.is_empty(),
            "fallback must not run when the tty write succeeds"
        );
    }

    #[test]
    fn write_via_falls_back_when_tty_opens_but_write_fails() {
        // The temp file opened read-only makes write_all fail, so control
        // falls through to the stdout fallback.
        let mut sink = Vec::new();
        assert!(osc52_write_via("hi", open_temp_readonly, &mut sink));
        assert!(
            !sink.is_empty(),
            "fallback should have written the sequence"
        );
    }

    #[test]
    fn write_via_falls_back_when_tty_open_fails() {
        let mut sink = Vec::new();
        assert!(osc52_write_via("hi", open_fails, &mut sink));
        assert!(!sink.is_empty());
    }

    #[test]
    fn write_via_returns_false_when_fallback_write_fails() {
        let mut sink = WriteFailWriter;
        assert!(!osc52_write_via("hi", open_fails, &mut sink));
        // `flush` is unreachable through `osc52_write_via` here (the failing
        // `write_all` short-circuits the `&&`), so call it directly to cover
        // its trivial body rather than leave a dead region.
        assert!(sink.flush().is_ok());
    }

    #[test]
    fn write_via_returns_false_when_fallback_flush_fails() {
        // write succeeds but flush fails — exercises the flush half of the
        // final `write_all(..).is_ok() && flush(..).is_ok()`.
        let mut sink = FlushFailWriter { buf: Vec::new() };
        assert!(!osc52_write_via("hi", open_fails, &mut sink));
        assert!(
            !sink.buf.is_empty(),
            "write should have run before flush failed"
        );
    }
}
