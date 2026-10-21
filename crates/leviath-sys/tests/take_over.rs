//! A daemon starting after a crash stops what its predecessor left running.
//!
//! Its own test binary because [`take_over`] installs a process-wide ledger,
//! and every command adopted afterwards in the same process is recorded in it.
//! Here that is the point; in the crate's unit tests it would leak into every
//! other test that spawns a command.

use leviath_sys::ChildGuard;
use leviath_sys::lifetime::{CommandRecord, Ledger, Swept, take_over};
use std::time::Duration;

/// A long-running command leading its own process group, as the shell tool
/// starts one.
#[cfg(unix)]
fn long_command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sleep");
    cmd.arg("60").process_group(0).kill_on_drop(true);
    cmd
}

/// The Windows twin: `cmd.exe` running `ping`, so the command has a child of
/// its own.
#[cfg(windows)]
fn long_command() -> tokio::process::Command {
    let mut cmd = leviath_sys::child_command_async("cmd");
    cmd.args(["/C", "ping -n 60 127.0.0.1 >NUL"])
        .kill_on_drop(true);
    cmd
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis() as u64
}

async fn exits_soon(child: &mut tokio::process::Child) -> bool {
    tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .is_ok()
}

#[tokio::test]
async fn a_new_daemon_stops_its_predecessors_orphans_and_records_its_own() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("running");

    // What a daemon killed mid-command leaves: a command still running, and
    // its record. Written through a ledger of its own, as the dead daemon's
    // would have been.
    let previous = Ledger::at(&dir);
    let mut orphan = long_command().spawn().expect("starts");
    let orphan_pid = orphan.id().unwrap();
    let orphan_record = CommandRecord {
        pid: orphan_pid,
        pgid: orphan_pid,
        started_ms: now_ms(),
    };
    previous.write(&orphan_record).unwrap();

    // A record whose pid the OS has since given to an unrelated process: the
    // pid is alive, but that process started long after the record says.
    let mut bystander = long_command().spawn().expect("starts");
    let bystander_pid = bystander.id().unwrap();
    let reused_record = CommandRecord {
        pid: bystander_pid,
        pgid: bystander_pid,
        started_ms: now_ms() - 3_600_000,
    };
    previous.write(&reused_record).unwrap();

    let mut swept = take_over(&dir);
    swept.sort_by_key(|s| matches!(s, Swept::Reused(_)));
    assert_eq!(
        swept,
        vec![Swept::Stopped(orphan_record), Swept::Reused(reused_record)]
    );
    assert!(exits_soon(&mut orphan).await, "the orphan was stopped");
    assert!(
        bystander.try_wait().unwrap().is_none(),
        "the process now holding a recorded pid was left alone"
    );
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "records cleared"
    );

    // From here on this process is the daemon: what it adopts is recorded in
    // the same place, and cleared once done with.
    assert_eq!(Ledger::installed().map(Ledger::dir), Some(dir.as_path()));
    let mut own = long_command().spawn().expect("starts");
    let own_pid = own.id().unwrap();
    let guard = ChildGuard::adopt(&own);
    assert!(guard.is_recorded());
    assert!(dir.join(format!("{own_pid}.cmd")).exists());
    drop(guard);
    assert!(exits_soon(&mut own).await);
    assert!(!dir.join(format!("{own_pid}.cmd")).exists());

    bystander.start_kill().unwrap();
    bystander.wait().await.unwrap();
}
