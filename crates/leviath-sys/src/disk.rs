//! How much room is left on the filesystem a path lives on.
//!
//! Leviath asks this before letting an agent write, because the failure it
//! guards against is not a bad write but a full disk: a run that filled `C:`
//! took the machine down with it, and every other process on it (issue #252).
//!
//! There is no free-space API in `std`, and this workspace forbids `unsafe`, so
//! the syscall is delegated to `fs4` - the same arrangement `nix` has here for
//! process and permission calls.

use std::path::Path;

/// Bytes still writable on the filesystem containing `path`, or `None` when the
/// question cannot be answered.
///
/// `None` is not "no space". It means the probe failed - an unmounted path, a
/// filesystem that does not report statistics, a permissions error - and a
/// caller must treat it as "unknown" rather than as either extreme. Refusing on
/// an unanswerable probe would block writes on any filesystem `fs4` cannot
/// read; allowing on it is what the caller does, because a guard that cannot
/// measure has nothing to say.
pub fn available_bytes(path: &Path) -> Option<u64> {
    fs4::available_space(path).ok()
}

/// Total size of the filesystem containing `path`, or `None` when the question
/// cannot be answered.
///
/// Only used to check [`available_bytes`] against something: a free-space probe
/// wired to the wrong syscall still returns a plausible number, and "is it less
/// than the disk it is on" is the cheapest question that catches that.
pub fn total_bytes(path: &Path) -> Option<u64> {
    fs4::total_space(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe answers for a real directory. Asserted as "some plausible
    /// number" rather than a value: the point is that the syscall works and is
    /// wired to the right path, and any concrete figure would be a test that
    /// fails when someone's disk fills.
    #[test]
    fn available_bytes_answers_for_a_real_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let free = available_bytes(dir.path()).expect("a temp dir reports its free space");
        // A machine that genuinely has under a megabyte free cannot have
        // created the tempdir above, so this is a wiring check, not a
        // disk-capacity assumption.
        assert!(free > 1024 * 1024, "implausible free space: {free}");
    }

    /// Free space cannot exceed the disk it is on.
    ///
    /// The cheapest assertion that catches a probe wired to the wrong syscall:
    /// a wrong one still returns a plausible-looking number, and "greater than
    /// a megabyte" would not notice. Runs on every platform in CI, which is the
    /// point - the three OSes reach three different system calls underneath.
    #[test]
    fn available_space_never_exceeds_the_filesystem_it_is_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let free = available_bytes(dir.path()).expect("free space");
        let total = total_bytes(dir.path()).expect("total space");
        assert!(total > 0, "a filesystem with no size");
        assert!(free <= total, "{free} free on a {total} filesystem");
    }

    /// A directory and a file inside it are on the same filesystem, so they
    /// report the same space. Catches a probe that answers about the *path*
    /// rather than the filesystem containing it, which would be a different
    /// (and wrong) question.
    ///
    /// Compared with a tolerance rather than for equality: the two calls are
    /// not atomic, and anything else on the machine may write between them.
    #[test]
    fn a_file_and_its_directory_report_the_same_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("probe.txt");
        std::fs::write(&file, b"probe").expect("write");

        let from_dir = available_bytes(dir.path()).expect("free space via the directory");
        let from_file = available_bytes(&file).expect("free space via the file");
        let drift = from_dir.abs_diff(from_file);
        // 64 MiB of slack: generous enough that an unrelated process writing
        // during the test cannot fail it, tight enough that two different
        // filesystems would not pass.
        assert!(
            drift < 64 * 1024 * 1024,
            "{from_dir} vs {from_file} differ by {drift}"
        );
    }

    /// A path that does not exist cannot be measured, and the caller has to be
    /// able to tell that apart from "measured, and it is zero".
    #[test]
    fn an_unmeasurable_path_answers_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("no").join("such").join("place");
        assert_eq!(available_bytes(&gone), None);
    }
}
