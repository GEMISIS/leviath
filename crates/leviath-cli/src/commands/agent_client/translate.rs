//! Turning a Leviath run's live progress into Agent Client Protocol output.
//!
//! Two pure pieces the async prompt loop in [`super`] leans on:
//!
//! * [`StageTail`] - an incremental, byte-offset reader over a run's per-stage
//!   `output.log` files, so agent output streams to the host as it is written
//!   rather than in one lump at the end.
//! * [`split_chunks`] - splits a blob of new output into frames no larger than
//!   [`leviath_agent_client::MAX_FRAME_BYTES`], on char boundaries, so a single
//!   `session/update` never exceeds a host's line-read limit.

use std::path::{Path, PathBuf};

use leviath_agent_client::MAX_FRAME_BYTES;
use leviath_core::floor_char_boundary;

/// Incremental reader over a run's readable output, across all of its stages.
///
/// Agent output lands in `<runs_dir>/<run_id>/stages/<idx>/output.log`, appended
/// line-by-line by the persistence lane as the run progresses. [`StageTail`]
/// remembers how many bytes of each stage it has already surfaced, so
/// [`pump`](StageTail::pump) returns only what is new since the previous call -
/// in stage order, completed stages before the current one.
///
/// It reads whole files and slices at the recorded byte offset. Because the
/// persistence lane only ever appends complete `\n`-terminated lines, every
/// offset lands on a line boundary and never splits a multi-byte character;
/// [`String::from_utf8_lossy`] guards the theoretical torn-read anyway.
#[derive(Debug, Default)]
pub(crate) struct StageTail {
    /// Bytes already surfaced from stage `i`, indexed by stage.
    offsets: Vec<usize>,
}

impl StageTail {
    /// A tail positioned at the start of a run with no stages seen yet.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Return output appended since the last call, across every stage that has
    /// produced any, in stage order.
    ///
    /// `runs_dir` is the runs root (injected so tests point at a temp dir);
    /// production passes [`crate::runstate::runs_dir`]. Stages are created lazily
    /// as output first arrives, in ascending order, so scanning stops at the
    /// first stage index that neither exists on disk nor has a recorded offset.
    pub(crate) fn pump(&mut self, runs_dir: &Path, run_id: &str) -> String {
        let mut out = String::new();
        let mut idx = 0;
        loop {
            let path = stage_output_path(runs_dir, run_id, idx);
            let exists = path.exists();
            if !exists && idx >= self.offsets.len() {
                // Neither this stage nor any beyond it has appeared yet.
                break;
            }
            let bytes = std::fs::read(&path).unwrap_or_default();
            let seen = self.offsets.get(idx).copied().unwrap_or(0);
            if bytes.len() > seen {
                out.push_str(&String::from_utf8_lossy(&bytes[seen..]));
            }
            if idx < self.offsets.len() {
                self.offsets[idx] = bytes.len();
            } else {
                self.offsets.push(bytes.len());
            }
            idx += 1;
        }
        out
    }
}

/// Path to stage `idx`'s readable output log under `runs_dir`.
fn stage_output_path(runs_dir: &Path, run_id: &str, idx: usize) -> PathBuf {
    runs_dir
        .join(run_id)
        .join("stages")
        .join(idx.to_string())
        .join("output.log")
}

/// Split `text` into consecutive slices each at most [`MAX_FRAME_BYTES`] bytes,
/// breaking only on `char` boundaries so no frame contains a torn UTF-8
/// sequence.
///
/// Returns an empty vec for empty input (the caller emits nothing rather than a
/// zero-length chunk). The common case - output well under one frame - returns a
/// single slice.
pub(crate) fn split_chunks(text: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        // The cut is never 0, so `rest` always shrinks and the loop terminates:
        // a whole char begins at byte 0 of `rest` and `MAX_FRAME_BYTES` is far
        // wider than the 4-byte maximum char, so the walk-back cannot reach the
        // start.
        let (chunk, tail) = rest.split_at(floor_char_boundary(rest, MAX_FRAME_BYTES));
        chunks.push(chunk);
        rest = tail;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `text` to stage `idx`'s output log under `root`, creating dirs.
    fn write_stage(root: &Path, run_id: &str, idx: usize, text: &str) {
        let path = stage_output_path(root, run_id, idx);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Append `text` to stage `idx`'s existing output log.
    fn append_stage(root: &Path, run_id: &str, idx: usize, text: &str) {
        use std::io::Write;
        let path = stage_output_path(root, run_id, idx);
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    // ─── StageTail ───────────────────────────────────────────────────────────

    #[test]
    fn pump_of_a_run_with_no_output_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut tail = StageTail::new();
        assert_eq!(tail.pump(dir.path(), "run-1"), "");
    }

    #[test]
    fn pump_returns_only_bytes_appended_since_last_call() {
        let dir = tempfile::tempdir().unwrap();
        let mut tail = StageTail::new();
        write_stage(dir.path(), "run-1", 0, "hello\n");
        assert_eq!(tail.pump(dir.path(), "run-1"), "hello\n");
        // No new bytes → nothing.
        assert_eq!(tail.pump(dir.path(), "run-1"), "");
        // Appended bytes only.
        append_stage(dir.path(), "run-1", 0, "world\n");
        assert_eq!(tail.pump(dir.path(), "run-1"), "world\n");
    }

    #[test]
    fn pump_streams_stages_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut tail = StageTail::new();
        write_stage(dir.path(), "run-1", 0, "stage zero\n");
        assert_eq!(tail.pump(dir.path(), "run-1"), "stage zero\n");
        // A second stage appears; only its new content comes back.
        write_stage(dir.path(), "run-1", 1, "stage one\n");
        assert_eq!(tail.pump(dir.path(), "run-1"), "stage one\n");
    }

    #[test]
    fn pump_drains_a_completed_stage_and_the_current_one_together() {
        let dir = tempfile::tempdir().unwrap();
        let mut tail = StageTail::new();
        // Both stages already hold output the tail has never seen.
        write_stage(dir.path(), "run-1", 0, "zero\n");
        write_stage(dir.path(), "run-1", 1, "one\n");
        assert_eq!(tail.pump(dir.path(), "run-1"), "zero\none\n");
    }

    #[test]
    fn pump_after_a_stage_slot_is_tracked_rereads_a_late_appended_stage() {
        let dir = tempfile::tempdir().unwrap();
        let mut tail = StageTail::new();
        write_stage(dir.path(), "run-1", 0, "zero\n");
        assert_eq!(tail.pump(dir.path(), "run-1"), "zero\n");
        // Stage 0 grows AND a new stage 1 appears in the same interval: the
        // tracked slot 0 re-reads its delta, and slot 1 is discovered.
        append_stage(dir.path(), "run-1", 0, "zero-more\n");
        write_stage(dir.path(), "run-1", 1, "one\n");
        assert_eq!(tail.pump(dir.path(), "run-1"), "zero-more\none\n");
    }

    #[test]
    fn pump_tolerates_invalid_utf8_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = stage_output_path(dir.path(), "run-1", 0);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, [b'o', b'k', 0xff, b'\n']).unwrap();
        let mut tail = StageTail::new();
        // The lone 0xff becomes the replacement char rather than panicking.
        assert!(tail.pump(dir.path(), "run-1").starts_with("ok"));
    }

    // ─── split_chunks ────────────────────────────────────────────────────────

    #[test]
    fn split_of_empty_is_no_chunks() {
        assert!(split_chunks("").is_empty());
    }

    #[test]
    fn split_of_small_text_is_one_chunk() {
        assert_eq!(split_chunks("hello world"), vec!["hello world"]);
    }

    #[test]
    fn split_breaks_large_text_into_frame_sized_pieces() {
        let big = "a".repeat(MAX_FRAME_BYTES * 2 + 5);
        let chunks = split_chunks(&big);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), MAX_FRAME_BYTES);
        assert_eq!(chunks[1].len(), MAX_FRAME_BYTES);
        assert_eq!(chunks[2].len(), 5);
        assert_eq!(chunks.concat(), big);
    }

    #[test]
    fn split_never_tears_a_multibyte_char() {
        // '✓' is 3 bytes; pack the boundary so a naive byte split would land
        // mid-character.
        let s = "✓".repeat(MAX_FRAME_BYTES);
        let chunks = split_chunks(&s);
        // Every chunk must itself be valid UTF-8 (it is a &str, so this is
        // guaranteed) and no chunk exceeds the frame size.
        assert!(chunks.iter().all(|c| c.len() <= MAX_FRAME_BYTES));
        assert!(chunks.len() >= 2);
        assert_eq!(chunks.concat(), s);
    }
}
