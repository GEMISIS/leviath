//! The real `lev daemon convert-runs`, the child a daemon converts its old
//! runs in, driven the way the daemon drives it: its progress read from its
//! stdout, its questions answered on its stdin, its log on its stderr.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../leviath-legacy-runs/tests/fixtures")
        .join(name)
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let to = dst.join(entry.file_name());
        match entry.file_type().unwrap().is_dir() {
            true => copy_dir(&entry.path(), &to),
            false => {
                std::fs::copy(entry.path(), &to).unwrap();
            }
        }
    }
}

/// `lev daemon convert-runs` for the runs under `home`, isolated from the
/// developer's own home.
fn convert_runs(home: &Path, build: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lev"));
    cmd.args(["daemon", "convert-runs", "--runs-dir"])
        .arg(home.join("runs"))
        .arg("--agents-dir")
        .arg(fixture("agents"))
        .args(["--build", build])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("LEVIATH_HOME", home)
        .env("LEVIATH_SKIP_DOTENV", "1")
        .current_dir(home);
    cmd
}

/// The child converts every old run, says how far along it is after each,
/// asks the daemon about each stage of the unfinished one, builds the run
/// index, and says when it has finished; its log goes to stderr, its stdout
/// carries only messages.
#[test]
fn the_child_converts_every_old_run_for_its_daemon() {
    let home = tempfile::tempdir().unwrap();
    let runs = home.path().join("runs");
    copy_dir(&fixture("finished"), &runs.join("a-done"));
    copy_dir(&fixture("mid-tool-batch"), &runs.join("b-old"));
    let mut child = convert_runs(home.path(), leviath_cli::daemon::setup::CURRENT_BUILD)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut answers = child.stdin.take().unwrap();
    let said = BufReader::new(child.stdout.take().unwrap());
    let mut kinds = Vec::new();
    let mut finished = None;
    for line in said.lines().map_while(Result::ok) {
        let message: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout carries only messages: {line:?} ({e})"));
        let kind = message
            .as_object()
            .and_then(|o| o.keys().next())
            .cloned()
            .unwrap_or_else(|| message.as_str().unwrap_or_default().to_string());
        let answer = match kind.as_str() {
            "model" => Some(r#"{"model":{"plan":{"Err":"no provider here"}}}"#),
            "tools" => Some(r#"{"tools":{"tools":{"Err":"no tools here"}}}"#),
            "max_depth" => Some(r#"{"max_depth":{"depth":3}}"#),
            "connect" => Some(r#""connected""#),
            _ => None,
        };
        if let Some(answer) = answer {
            writeln!(answers, "{answer}").unwrap();
        }
        if kind == "finished" {
            finished = Some(message["finished"]["so_far"].clone());
        }
        kinds.push(kind);
    }
    drop(answers);
    let status = child.wait().unwrap();
    let mut log = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut log).unwrap();
    assert!(status.success(), "{log}");
    let so_far = finished.expect("the child said it finished");
    assert_eq!(so_far["converted"], 2, "{so_far}");
    assert_eq!(
        kinds.first().map(String::as_str),
        Some("begin"),
        "{kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| *k == "done").count(),
        2,
        "{kinds:?}"
    );
    assert!(kinds.iter().any(|k| k == "model"), "{kinds:?}");
    for run in ["a-done", "b-old"] {
        let file = runs.join(run).join("run.lvr");
        let reader = leviath_runtime::runfile::RunFileReader::open(&file).unwrap();
        let old = leviath_legacy_runs::meta(&runs.join(run).join("legacy")).unwrap();
        assert_eq!(reader.spec().run_id.as_str(), old.run_id);
    }
    assert!(log.contains("old run directories"), "{log}");
    assert!(home.path().join("backups").is_dir());
    assert!(home.path().join("runs.index").is_file());
}

/// A child of another build refuses before it touches a run, and the
/// subcommand is not in `lev daemon --help`: only a daemon runs it.
#[test]
fn the_child_is_internal_and_serves_only_its_own_build() {
    let home = tempfile::tempdir().unwrap();
    let runs = home.path().join("runs");
    copy_dir(&fixture("finished"), &runs.join("a-done"));
    let out = convert_runs(home.path(), "another-build")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(leviath_legacy_runs::is_legacy(&runs.join("a-done")));

    let help = Command::new(env!("CARGO_BIN_EXE_lev"))
        .args(["daemon", "--help"])
        .env("LEVIATH_HOME", home.path())
        .env("LEVIATH_SKIP_DOTENV", "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("uninstall"), "{text}");
    assert!(!text.contains("convert-runs"), "{text}");
}
