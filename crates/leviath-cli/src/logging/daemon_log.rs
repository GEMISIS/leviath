//! A long-lived process's own log file: the daemon's `<data_dir>/daemon.log`,
//! a server's `<data_dir>/serve-<name>.log`, size-capped, rolled once to
//! `<name>.1`.
//!
//! A daemon the CLI starts for you has no terminal, and its stderr goes to the
//! null device, so without this file its log lines went nowhere; a server
//! under `nohup` or a supervisor is in the same position. The files are what
//! `lev rage` packs into a bug report, and what you read when a run never
//! spawned. The sibling `runstate/dashboard_log.rs` applies the same naming
//! rule to the dashboard's activity log.
//!
//! One handle is held open and the length tracked in-process, so a log line
//! costs one `write(2)` rather than a stat, an open and a close each. The cap
//! is an atomic because `[observability]` reloads while the daemon runs.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

/// Where a daemon writes its log: `daemon.log` under the data directory, or
/// `None` when no home directory resolves.
pub fn daemon_log_path() -> Option<PathBuf> {
    leviath_core::paths::data_dir().map(|dir| dir.join("daemon.log"))
}

/// Where a `lev serve` writes its log: `serve-<name>.log` under the data
/// directory, one per server. `name` is `--name`, or the port when none was
/// given, so two servers side by side never share a file and a restart on
/// the same port keeps rolling the same one.
pub fn serve_log_path(name: &str) -> Option<PathBuf> {
    leviath_core::paths::data_dir().map(|dir| dir.join(format!("serve-{name}.log")))
}

/// The rolled (previous-generation) file: `<path>.1`.
pub(crate) fn rolled_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// The open handle and how long the live file is.
struct Inner {
    /// `None` until the first line, and again after a roll.
    file: Option<File>,
    /// Bytes in the live file, seeded from its length at construction.
    len: u64,
}

/// One capped, rolling log file.
///
/// Every append takes the mutex, so lines from tokio worker threads never
/// interleave and every thread sees a roll.
pub(crate) struct DaemonLog {
    path: PathBuf,
    rolled: PathBuf,
    cap: AtomicU64,
    inner: Mutex<Inner>,
}

impl DaemonLog {
    /// A log at `path` that rolls once the live file reaches `cap` bytes.
    /// `0` never rolls. A file a previous daemon left behind counts from its
    /// current length, so a restart never grows it past the cap.
    pub(crate) fn new(path: PathBuf, cap: u64) -> Self {
        let rolled = rolled_path(&path);
        let len = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        Self {
            path,
            rolled,
            cap: AtomicU64::new(cap),
            inner: Mutex::new(Inner { file: None, len }),
        }
    }

    /// Raise or lower the cap; the next append applies it.
    pub(crate) fn set_cap(&self, bytes: u64) {
        self.cap.store(bytes, Ordering::Relaxed);
    }

    /// Append `buf` whole, rolling first when the live file has reached the
    /// cap. The one error is a file that cannot be opened, such as a
    /// directory sitting where the log should be.
    pub(crate) fn append(&self, buf: &[u8]) -> io::Result<()> {
        // A poisoned lock means a thread panicked mid-append. The file is
        // still a file, so the guard is taken back: a logging call is the
        // worst place to raise a second panic.
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let cap = self.cap.load(Ordering::Relaxed);
        if cap > 0 && inner.len >= cap {
            // Closed before the rename: Windows will not move an open file.
            // A failed rename is best-effort, and the file keeps growing
            // rather than the daemon losing its log.
            inner.file = None;
            let _ = std::fs::rename(&self.path, &self.rolled);
            inner.len = 0;
        }
        if inner.file.is_none() {
            inner.file = Some(leviath_sys::open_private_append(&self.path)?);
        }
        let Inner { file, len } = &mut *inner;
        let file = file.as_mut().expect("opened just above");
        file.write_all(buf).map(|()| *len += buf.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn append_creates_the_file_and_keeps_appending() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("daemon.log");
        let log = DaemonLog::new(path.clone(), 1024);
        log.append(b"one\n").expect("first line");
        log.append(b"two\n").expect("second line");
        assert_eq!(read(&path), "one\ntwo\n");
        assert!(!rolled_path(&path).exists(), "nothing rolled under the cap");
    }

    #[test]
    fn a_write_past_the_cap_rolls_once_to_dot_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("daemon.log");
        let log = DaemonLog::new(path.clone(), 8);
        log.append(b"12345678\n")
            .expect("lands, then the file is at the cap");
        log.append(b"next\n").expect("rolls first");
        assert_eq!(read(&rolled_path(&path)), "12345678\n");
        assert_eq!(read(&path), "next\n");
    }

    #[test]
    fn rolling_replaces_an_existing_dot_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("daemon.log");
        std::fs::write(rolled_path(&path), "stale\n").expect("a previous generation");
        let log = DaemonLog::new(path.clone(), 4);
        log.append(b"aaaa\n").expect("at the cap");
        log.append(b"b\n").expect("rolls");
        assert_eq!(read(&rolled_path(&path)), "aaaa\n");
    }

    #[test]
    fn a_zero_cap_never_rolls() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("daemon.log");
        let log = DaemonLog::new(path.clone(), 0);
        for _ in 0..8 {
            log.append(b"line\n").expect("appends");
        }
        assert_eq!(read(&path).len(), 40);
        assert!(!rolled_path(&path).exists());
    }

    #[test]
    fn the_cap_can_be_raised_after_construction() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("daemon.log");
        let log = DaemonLog::new(path.clone(), 4);
        log.set_cap(1024);
        log.append(b"aaaa\n").expect("at the old cap");
        log.append(b"b\n").expect("no roll under the new one");
        assert_eq!(read(&path), "aaaa\nb\n");
        assert!(!rolled_path(&path).exists());
    }

    #[test]
    fn a_path_that_cannot_be_opened_is_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // A directory where the file should be: `open` fails on every OS.
        // Cap 0, because a directory has a size of its own (4096 bytes on
        // ext4), and a cap below it would roll the directory aside first and
        // then open a fresh file where it stood.
        let path = dir.path().join("daemon.log");
        std::fs::create_dir(&path).expect("a directory in the way");
        let log = DaemonLog::new(path, 0);
        assert!(log.append(b"x").is_err());
    }

    #[test]
    fn the_length_is_seeded_from_an_existing_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, "already here\n").expect("a previous daemon's lines");
        let log = DaemonLog::new(path.clone(), 8);
        // 13 bytes are on disk, over the cap of 8, so the first append rolls
        // even though this process has written nothing yet.
        log.append(b"new\n").expect("appends");
        assert_eq!(read(&rolled_path(&path)), "already here\n");
        assert_eq!(read(&path), "new\n");
    }

    #[test]
    fn rolled_path_appends_dot_one() {
        assert_eq!(
            rolled_path(Path::new("/x/daemon.log")),
            PathBuf::from("/x/daemon.log.1")
        );
    }

    #[test]
    fn a_server_log_is_named_for_its_server() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || serve_log_path("3000"));
        assert_eq!(
            path,
            Some(dir.path().join(".leviath").join("serve-3000.log"))
        );
    }

    #[test]
    fn daemon_log_path_lives_under_the_data_dir() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = temp_env::with_var("LEVIATH_HOME", Some(dir.path()), daemon_log_path);
        assert_eq!(path, Some(dir.path().join(".leviath").join("daemon.log")));
    }
}
