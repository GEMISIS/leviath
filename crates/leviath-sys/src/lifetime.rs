//! Keeping a command a run started from outliving the daemon that started it.
//!
//! A shell command runs in its own process group so a timeout or a cancel can
//! stop everything it started. The same detachment means a daemon killed with
//! `kill -9` leaves the command running on its own, still changing files after
//! the run has moved on. Two layers stop that.
//!
//! 1. **A tie to this process.** [`ChildGuard::adopt`] ties the command to the
//!    daemon where the OS allows it. On Windows that is a kill-on-close job
//!    object; on Unix it is a small watchdog that kills the group when a pipe
//!    from the daemon closes, which the kernel does however the daemon ends.
//!    `PR_SET_PDEATHSIG` is not used: the reasons are in the Unix module.
//! 2. **A record the next daemon reads.** While a [`Ledger`] is installed
//!    (the daemon installs one at start with [`take_over`]), every adopted
//!    command is written to a small file for as long as it runs. A daemon
//!    starting after a crash reads what its predecessor left and stops each
//!    command whose identity still matches, before any run is recovered.
//!
//! The identity is the process's start time. A pid the OS has since handed to
//! an unrelated process started at a different time, so it is left alone:
//! nothing is killed on a pid alone.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// How far apart the recorded and reported start times may be and still name
/// the same process.
///
/// The record holds the time just after `spawn` returned; the OS reports the
/// process's own start, which on Unix comes from `ps` in whole seconds. Two
/// seconds covers both. A pid reused within two seconds of the first process
/// starting would need the OS to cycle through every other pid in between.
const START_TOLERANCE_MS: u64 = 2_000;

/// The version tag every record starts with, so a later layout can be told
/// apart rather than misread.
const RECORD_VERSION: &str = "1";

/// The extension a record file carries; anything else in the directory is not
/// ours to read or remove.
const RECORD_EXTENSION: &str = "cmd";

/// The ledger the daemon installed, if any. Set once, by [`take_over`].
static INSTALLED: OnceLock<Ledger> = OnceLock::new();

/// Milliseconds since the Unix epoch, by the wall clock.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// One running command, as a record file holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandRecord {
    /// The command's process: the shell the tool started.
    pub pid: u32,
    /// The process group it leads. On Windows, which has none, the pid again.
    pub pgid: u32,
    /// When it started, in milliseconds since the Unix epoch.
    pub started_ms: u64,
}

impl CommandRecord {
    /// The record's file contents: one line, versioned.
    fn encode(&self) -> String {
        format!(
            "{RECORD_VERSION} {} {} {}\n",
            self.pid, self.pgid, self.started_ms
        )
    }

    /// Read a record back. `None` for a file in another layout, or one cut
    /// short by a crash mid-write.
    fn decode(text: &str) -> Option<Self> {
        let mut fields = text.split_whitespace();
        let version = fields.next();
        let pid = fields.next().and_then(|f| f.parse().ok());
        let pgid = fields.next().and_then(|f| f.parse().ok());
        let started_ms = fields.next().and_then(|f| f.parse().ok());
        match (version, pid, pgid, started_ms, fields.next()) {
            (Some(RECORD_VERSION), Some(pid), Some(pgid), Some(started_ms), None) => Some(Self {
                pid,
                pgid,
                started_ms,
            }),
            _ => None,
        }
    }

    /// Whether the process this record names is still the one it recorded:
    /// the pid is alive, started when the record says, and still leads the
    /// recorded group.
    fn identify(&self) -> Identity {
        match crate::platform::process_started_ms(self.pid) {
            None => Identity::Gone,
            Some(started)
                if started.abs_diff(self.started_ms) <= START_TOLERANCE_MS
                    && crate::platform::process_group_of(self.pid) == Some(self.pgid) =>
            {
                Identity::Same
            }
            Some(_) => Identity::Other,
        }
    }
}

/// What [`CommandRecord::identify`] found.
enum Identity {
    /// The recorded process, still running.
    Same,
    /// A live process with the recorded pid that is not the recorded one.
    Other,
    /// No process with the recorded pid.
    Gone,
}

/// What the start-up sweep did with one record a previous daemon left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Swept {
    /// The command was still running and has been stopped.
    Stopped(CommandRecord),
    /// The pid is alive but belongs to a different process now, so it was left
    /// alone.
    Reused(CommandRecord),
    /// The command had already ended.
    Gone(CommandRecord),
    /// A file that does not hold a record this build can read. Removed, since
    /// nothing could ever act on it.
    Unreadable(PathBuf),
}

/// The directory of record files, one per running command.
#[derive(Debug, Clone)]
pub struct Ledger {
    dir: PathBuf,
}

impl Ledger {
    /// A ledger keeping its records in `dir`, which is created on the first
    /// write.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Where the records live.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The ledger the daemon installed with [`take_over`], if any. Only the
    /// daemon installs one, so a test or a one-off command records nothing.
    pub fn installed() -> Option<&'static Ledger> {
        INSTALLED.get()
    }

    /// Write `record`, returning the file it went to.
    pub fn write(&self, record: &CommandRecord) -> std::io::Result<PathBuf> {
        let path = self.dir.join(format!("{}.{RECORD_EXTENSION}", record.pid));
        crate::perms::create_private_dir_all(&self.dir)
            .and_then(|()| crate::perms::write_atomic(&path, record.encode().as_bytes(), None))
            .map(|()| path)
    }

    /// Record a command that has just been spawned.
    ///
    /// A failed write is logged and costs only this layer: the tie still
    /// covers the command while this daemon lives.
    fn record_spawned(&self, pid: u32) -> Option<RecordFile> {
        let record = CommandRecord {
            pid,
            pgid: crate::platform::process_group_of(pid).unwrap_or(pid),
            started_ms: now_ms(),
        };
        match self.write(&record) {
            Ok(path) => Some(RecordFile(path)),
            Err(e) => {
                tracing::warn!(pid, dir = %self.dir.display(), error = %e, "could not record a running command");
                None
            }
        }
    }

    /// Stop every command a previous daemon recorded and never cleared, and
    /// remove the records.
    ///
    /// Call this only while no other daemon is using the same directory: a
    /// live daemon's records name commands it is still running. The daemon
    /// sweeps after winning its single-instance bind, which guarantees that.
    /// A missing directory (a home an older daemon last ran in) has nothing to
    /// sweep.
    pub fn sweep(&self) -> Vec<Swept> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut swept = Vec::new();
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.extension().and_then(|e| e.to_str()) != Some(RECORD_EXTENSION) {
                continue;
            }
            let outcome = match std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| CommandRecord::decode(&text))
            {
                Some(record) => sweep_one(record),
                None => Swept::Unreadable(path.clone()),
            };
            let _ = std::fs::remove_file(&path);
            swept.push(outcome);
        }
        swept
    }
}

/// Stop the command `record` names if it is still the one recorded.
fn sweep_one(record: CommandRecord) -> Swept {
    match record.identify() {
        Identity::Same => {
            // A failure here means the group ended between the check and the
            // kill, which leaves nothing to do but say so.
            let killed = crate::platform::kill_tree(record.pid, record.pgid);
            tracing::warn!(
                pid = record.pid,
                pgid = record.pgid,
                error = ?killed.err(),
                "stopped a command a previous daemon left running"
            );
            Swept::Stopped(record)
        }
        Identity::Other => Swept::Reused(record),
        Identity::Gone => Swept::Gone(record),
    }
}

/// Become the daemon that owns the records in `dir`: stop whatever a crashed
/// predecessor left running there, then install the ledger so every command
/// this process adopts is recorded in the same place.
///
/// Called once, at daemon start, after the single-instance bind and before any
/// run is recovered. A second call in the same process sweeps again but keeps
/// the first ledger.
pub fn take_over(dir: impl Into<PathBuf>) -> Vec<Swept> {
    let ledger = Ledger::at(dir);
    let swept = ledger.sweep();
    let _ = INSTALLED.set(ledger);
    swept
}

/// A record file, removed when the command it names is done with.
struct RecordFile(PathBuf);

impl Drop for RecordFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Everything that keeps one spawned command from outliving its welcome.
///
/// Hold it for exactly as long as the command may run. Dropping it, on any
/// path (the command finished, timed out, or the run was cancelled and the
/// future dropped), kills the command's whole process group, releases the tie
/// and removes the record. If the daemon dies first, the tie stops the command
/// at once, and the record lets the next daemon stop it if the tie could not.
pub struct ChildGuard {
    /// The process group the child leads; `None` for a child already reaped.
    pgid: Option<u32>,
    /// Layer 1. Released after the group is killed.
    tie: Option<crate::platform::Tie>,
    /// Layer 2. Removed last, once nothing of the command is left.
    record: Option<RecordFile>,
}

impl ChildGuard {
    /// Guard `child`, recording it in the daemon's ledger if one is installed.
    ///
    /// The child must lead its own process group (see
    /// [`crate::configure_detached`]), so its pid is the group id. Call this
    /// straight after `spawn`, before waiting on the child.
    pub fn adopt(child: &tokio::process::Child) -> Self {
        Self::adopt_into(child, Ledger::installed())
    }

    /// [`ChildGuard::adopt`] with the ledger named rather than looked up.
    pub fn adopt_into(child: &tokio::process::Child, ledger: Option<&Ledger>) -> Self {
        let pgid = child.id();
        // Recorded before the tie, so a daemon that dies between the two still
        // leaves the record behind.
        let record = pgid
            .zip(ledger)
            .and_then(|(pid, ledger)| ledger.record_spawned(pid));
        Self {
            pgid,
            tie: crate::platform::tie_child(child),
            record,
        }
    }

    /// Whether this command is recorded in a ledger.
    pub fn is_recorded(&self) -> bool {
        self.record.is_some()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // The group is usually already gone; failing to signal it is expected.
        if let Some(pgid) = self.pgid {
            let _ = crate::process::kill_process_group(pgid);
        }
        drop(self.tie.take());
        drop(self.record.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips() {
        let record = CommandRecord {
            pid: 41,
            pgid: 41,
            started_ms: 1_700_000_000_123,
        };
        assert_eq!(record.encode(), "1 41 41 1700000000123\n");
        assert_eq!(CommandRecord::decode(&record.encode()), Some(record));
    }

    #[test]
    fn a_record_in_another_layout_is_not_read() {
        for text in [
            "",
            "2 41 41 1",
            "1 41 41",
            "1 41 41 1 extra",
            "1 x 41 1",
            "1 41 x 1",
            "1 41 41 x",
        ] {
            assert_eq!(CommandRecord::decode(text), None, "{text:?}");
        }
    }

    #[test]
    fn writing_a_record_creates_the_directory() {
        let home = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(home.path().join("running"));
        let record = CommandRecord {
            pid: 7,
            pgid: 7,
            started_ms: 9,
        };
        let path = ledger.write(&record).expect("written");
        assert_eq!(path, ledger.dir().join("7.cmd"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1 7 7 9\n");
    }

    #[test]
    fn a_ledger_that_cannot_write_records_nothing() {
        let home = tempfile::tempdir().unwrap();
        // A file where the directory should be.
        let blocked = home.path().join("running");
        std::fs::write(&blocked, b"").unwrap();
        assert!(Ledger::at(&blocked).record_spawned(7).is_none());
    }

    /// A pid with no process: whatever the record says, it is gone.
    fn unused_pid() -> u32 {
        0x7FFF_FFF0
    }

    #[test]
    fn sweeping_a_missing_directory_finds_nothing() {
        let home = tempfile::tempdir().unwrap();
        assert!(Ledger::at(home.path().join("never")).sweep().is_empty());
    }

    #[test]
    fn sweeping_removes_records_of_commands_that_already_ended() {
        let _env = env_guard();
        let home = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(home.path());
        let record = CommandRecord {
            pid: unused_pid(),
            pgid: unused_pid(),
            started_ms: now_ms(),
        };
        let path = ledger.write(&record).unwrap();
        std::fs::write(home.path().join("garbled.cmd"), b"not a record").unwrap();
        std::fs::write(home.path().join("notes.txt"), b"not ours").unwrap();

        let mut swept = ledger.sweep();
        swept.sort_by_key(|s| matches!(s, Swept::Unreadable(_)));
        assert_eq!(
            swept,
            vec![
                Swept::Gone(record),
                Swept::Unreadable(home.path().join("garbled.cmd"))
            ]
        );
        assert!(!path.exists());
        assert!(!home.path().join("garbled.cmd").exists());
        assert!(
            home.path().join("notes.txt").exists(),
            "only records are ours"
        );
    }

    /// A long-running command in its own group, as the shell tool starts one.
    #[cfg(unix)]
    fn long_command() -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("60").process_group(0).kill_on_drop(true);
        cmd
    }

    /// The Windows twin: `cmd.exe` running `ping`, so the command has a child
    /// of its own for the job to stop.
    #[cfg(windows)]
    fn long_command() -> tokio::process::Command {
        let mut cmd = crate::child_command_async("cmd");
        cmd.args(["/C", "ping -n 60 127.0.0.1 >NUL"])
            .kill_on_drop(true);
        cmd
    }

    /// The tests below that ask Windows about a process spawn PowerShell from
    /// the system directory, which another test moves under a lock.
    #[cfg(windows)]
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::platform::ENV_LOCK.lock().expect("env lock")
    }

    /// Unix reads no environment to answer, so the lock is one nothing else
    /// takes.
    #[cfg(unix)]
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static UNSHARED: std::sync::Mutex<()> = std::sync::Mutex::new(());
        UNSHARED.lock().expect("env lock")
    }

    async fn exits_soon(child: &mut tokio::process::Child) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(20), child.wait())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn the_guard_records_the_command_and_clears_up_after_it() {
        let home = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(home.path());
        let mut child = long_command().spawn().expect("starts");
        let pid = child.id().unwrap();
        let guard = ChildGuard::adopt_into(&child, Some(&ledger));
        assert!(guard.is_recorded());
        let text = std::fs::read_to_string(home.path().join(format!("{pid}.cmd"))).unwrap();
        let record = CommandRecord::decode(&text).expect("a record");
        assert_eq!((record.pid, record.pgid), (pid, pid));

        drop(guard);
        assert!(exits_soon(&mut child).await, "dropping the guard stops it");
        assert!(!home.path().join(format!("{pid}.cmd")).exists());
    }

    #[tokio::test]
    async fn without_a_ledger_nothing_is_recorded() {
        let mut child = long_command().spawn().expect("starts");
        let guard = ChildGuard::adopt(&child);
        assert!(!guard.is_recorded(), "no daemon installed a ledger here");
        drop(guard);
        assert!(exits_soon(&mut child).await);
    }

    #[tokio::test]
    async fn a_child_already_reaped_has_nothing_to_guard() {
        let home = tempfile::tempdir().unwrap();
        let mut child = long_command().spawn().expect("starts");
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        let guard = ChildGuard::adopt_into(&child, Some(&Ledger::at(home.path())));
        assert!(!guard.is_recorded());
        drop(guard);
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn sweeping_stops_a_matching_command_and_spares_a_reused_pid() {
        let home = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(home.path());
        let mut orphan = long_command().spawn().expect("starts");
        let mut bystander = long_command().spawn().expect("starts");
        let orphan_record = CommandRecord {
            pid: orphan.id().unwrap(),
            pgid: orphan.id().unwrap(),
            started_ms: now_ms(),
        };
        // The same pid, recorded as a process that started an hour earlier:
        // what a record looks like once the OS has handed its pid to another.
        let reused_record = CommandRecord {
            pid: bystander.id().unwrap(),
            pgid: bystander.id().unwrap(),
            started_ms: now_ms() - 3_600_000,
        };
        ledger.write(&orphan_record).unwrap();
        ledger.write(&reused_record).unwrap();

        // Scoped so the lock is not held across the awaits below.
        let mut swept = {
            let _env = env_guard();
            ledger.sweep()
        };
        swept.sort_by_key(|s| matches!(s, Swept::Reused(_)));
        assert_eq!(
            swept,
            vec![Swept::Stopped(orphan_record), Swept::Reused(reused_record)]
        );
        assert!(exits_soon(&mut orphan).await, "the orphan was stopped");
        assert!(
            bystander.try_wait().unwrap().is_none(),
            "the reused pid was left alone"
        );
        bystander.start_kill().unwrap();
        bystander.wait().await.unwrap();
    }
}
