use super::*;
use crate::daemon::mode::DaemonMode;

/// A build of `version`, committed at `at`.
fn build(id: &str, version: &str, at: u64) -> Build {
    Build {
        id: id.to_string(),
        version: Some(version.to_string()),
        committed_at: Some(at),
    }
}

/// What 0.6.4 and every release before it wrote: the build id alone.
const RELEASED: &str = "839f0344";

#[test]
fn a_marker_round_trips_and_an_old_one_is_the_id_alone() {
    let new = build("1298d88c", "0.6.4", 1_791_000_000);
    assert_eq!(new.marker(), "1298d88c 0.6.4 1791000000");
    assert_eq!(Build::parse(&new.marker()), new);
    let old = Build::parse(RELEASED);
    assert_eq!(
        old,
        Build {
            id: RELEASED.to_string(),
            version: None,
            committed_at: None,
        }
    );
    assert_eq!(old.marker(), RELEASED);
    assert_eq!(old.describe(), "an earlier release (build 839f0344)");
    assert_eq!(new.describe(), "0.6.4 (build 1298d88c)");
    // No marker at all is a daemon that predates them.
    assert_eq!(Build::parse("").id, "unknown");
    // This binary knows its version and when its commit was made.
    let current = Build::current();
    assert_eq!(current.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(current.committed_at.is_some(), !COMMITTED_AT.is_empty());
}

/// Versions decide, then commit times; a daemon that did not say its version
/// is older. Two builds of one version that tie on commit time, or where
/// either lacks one, cannot be put in order.
#[test]
fn a_daemon_stands_older_or_newer_by_version_then_commit_time() {
    let cli = build("bbbb", "0.6.4", 200);
    assert_eq!(cli.standing(&cli), Standing::Same);
    assert_eq!(Build::parse(RELEASED).standing(&cli), Standing::Older);
    assert_eq!(build("aaaa", "0.6.4", 100).standing(&cli), Standing::Older);
    assert_eq!(build("cccc", "0.6.4", 300).standing(&cli), Standing::Newer);
    // A dirty rebuild of one commit: the same version and commit time.
    assert_eq!(
        build("cccc", "0.6.4", 200).standing(&cli),
        Standing::Unordered
    );
    // The version outranks the commit time, and 0.6.10 is after 0.6.9.
    assert_eq!(build("cccc", "0.6.3", 900).standing(&cli), Standing::Older);
    assert_eq!(build("cccc", "0.6.10", 1).standing(&cli), Standing::Newer);
    assert_eq!(
        build("cccc", "0.6.5-beta.1", 1).standing(&cli),
        Standing::Newer
    );
    // One that recorded no commit time cannot be put in order by it.
    let untimed = Build {
        committed_at: None,
        ..build("cccc", "0.6.4", 0)
    };
    assert_eq!(untimed.standing(&cli), Standing::Unordered);
    assert_eq!(cli.standing(&untimed), Standing::Unordered);
    assert_eq!(untimed.marker(), "cccc 0.6.4");
}

/// Versions order as semver says: by number, and a pre-release before its
/// release. Build metadata is ignored, a missing part is 0, and pre-release
/// parts order numbers by value, before words.
#[test]
fn versions_order_as_semver_says() {
    let ordered = [
        "0.6.9",
        "0.6.10",
        "0.7.0-alpha",
        "0.7.0-alpha.1",
        "0.7.0-alpha.beta",
        "0.7.0-beta.1",
        "0.7.0-beta.2",
        "0.7.0-beta.11",
        "0.7.0-rc.1",
        "0.7.0",
        "0.7.1",
        "1.0.0",
    ];
    let out_of_order = ordered
        .windows(2)
        .filter(|pair| Release::parse(pair[0]) >= Release::parse(pair[1]))
        .count();
    assert_eq!(out_of_order, 0);
    assert_eq!(Release::parse("0.7.0+abc"), Release::parse("0.7.0"));
    assert_eq!(Release::parse("0.7"), Release::parse("0.7.0"));
    let cli = build("bbbb", "0.7.0", 1);
    assert_eq!(
        build("cccc", "0.7.0-beta.1", 9).standing(&cli),
        Standing::Older
    );
    assert_eq!(
        cli.standing(&build("cccc", "0.7.0-beta.1", 9)),
        Standing::Newer
    );
}

/// Each standing reads as a relation in a message.
#[test]
fn each_standing_reads_as_a_relation() {
    let words = [
        Standing::Same,
        Standing::Older,
        Standing::Newer,
        Standing::Unordered,
    ]
    .map(Standing::relation);
    assert_eq!(
        words,
        [
            "the same build as",
            "older than",
            "newer than",
            "another build of the same version as"
        ]
    );
}

/// The default mode replaces only an older daemon, and says which is which
/// while it does; the same build, a newer one, and one it cannot put in order
/// are left running.
#[test]
fn only_an_older_daemon_is_replaced() {
    let cli = build("bbbb", "0.6.4", 200);
    let ensure = DaemonMode::Ensure;
    let same = replace(Some(&cli.marker()), &cli, ensure);
    assert_eq!((same.replace, same.say), (false, None));
    let newer = replace(Some(&build("cccc", "0.6.5", 1).marker()), &cli, ensure);
    assert_eq!((newer.replace, newer.say), (false, None));
    let tie = replace(Some(&build("cccc", "0.6.4", 200).marker()), &cli, ensure);
    assert_eq!((tie.replace, tie.say), (false, None));
    let older = replace(Some(RELEASED), &cli, ensure);
    assert!(older.replace);
    let said = older.say.unwrap_or_default();
    assert!(
        said.contains(
            "an earlier release (build 839f0344), older than this lev (0.6.4 (build bbbb))"
        ),
        "{said}"
    );
    assert!(
        replace(None, &cli, ensure).replace,
        "a daemon that wrote no marker"
    );
}

/// A git build and a build made without git (id `v<version>`, no commit
/// time) of one version leave each other running, in either order, so two
/// installs of `lev` do not restart the daemon back and forth.
#[test]
fn a_git_build_and_a_versioned_build_do_not_replace_each_other() {
    let git = build("aaaa", "0.6.4", 100);
    let crates_io = Build {
        id: "v0.6.4".to_string(),
        version: Some("0.6.4".to_string()),
        committed_at: None,
    };
    let ensure = DaemonMode::Ensure;
    let git_daemon = replace(Some(&git.marker()), &crates_io, ensure).replace;
    let versioned_daemon = replace(Some(&crates_io.marker()), &git, ensure).replace;
    assert_eq!((git_daemon, versioned_daemon), (false, false));
}

/// `--daemon replace` keeps the rule that replaces any daemon it cannot show
/// is the same build or newer: a dirty rebuild of one commit replaces the
/// daemon it rebuilt, and says so.
#[test]
fn replace_mode_also_replaces_a_rebuild_of_the_same_commit() {
    let cli = build("bbbb", "0.6.4", 200);
    let rebuilt = replace(
        Some(&build("cccc", "0.6.4", 200).marker()),
        &cli,
        DaemonMode::Replace,
    );
    assert!(rebuilt.replace);
    let said = rebuilt.say.unwrap_or_default();
    assert!(
        said.contains("0.6.4 (build cccc), another build of the same version as this lev"),
        "{said}"
    );
    let newer = replace(
        Some(&build("cccc", "0.6.5", 1).marker()),
        &cli,
        DaemonMode::Replace,
    );
    assert!(!newer.replace);
}

/// `--daemon attach` replaces nothing, however old.
#[test]
fn attach_mode_replaces_nothing() {
    let cli = build("bbbb", "0.6.4", 200);
    let older = replace(Some(RELEASED), &cli, DaemonMode::Attach);
    assert_eq!((older.replace, older.say), (false, None));
}

/// Every command says when the daemon answering is another build, which one
/// is newer, and what to do; nothing while they match or none is running.
#[test]
fn every_command_says_when_the_daemon_is_another_build() {
    let cli = build("bbbb", "0.6.4", 200);
    assert_eq!(mixed_notice(true, Some(&cli.marker()), &cli), None);
    assert_eq!(mixed_notice(false, Some(RELEASED), &cli), None);
    let older = mixed_notice(true, Some(RELEASED), &cli).unwrap_or_default();
    assert!(
        older.contains("this daemon is an earlier release (build 839f0344), older than this lev"),
        "{older}"
    );
    assert!(older.contains("`lev daemon restart`"), "{older}");
    let newer = build("cccc", "0.6.5", 1).marker();
    let newer = mixed_notice(true, Some(&newer), &cli).unwrap_or_default();
    assert!(
        newer.contains("this daemon is 0.6.5 (build cccc), newer than this lev"),
        "{newer}"
    );
    assert!(newer.contains("Use the newer lev"), "{newer}");
    let tie = build("cccc", "0.6.4", 200).marker();
    let tie = mixed_notice(true, Some(&tie), &cli).unwrap_or_default();
    assert!(
        tie.contains("0.6.4 (build cccc), another build of the same version as this lev"),
        "{tie}"
    );
}

/// With no daemon in a home there is nothing to say.
#[test]
fn a_home_with_no_daemon_has_nothing_to_say() {
    let home = tempfile::tempdir().unwrap();
    temp_env::with_var("LEVIATH_HOME", Some(home.path()), || {
        assert_eq!(mixed_notice_here(), None);
    });
}

/// `lev daemon status` says when the running daemon is another build than
/// this `lev`, how they stand, and what to do; nothing while they match or
/// none runs.
#[test]
fn daemon_status_says_how_the_builds_stand() {
    let cli = build("bbbb", "0.6.4", 200);
    assert_eq!(status_line(true, Some(&cli.marker()), &cli), None);
    assert_eq!(status_line(false, Some(RELEASED), &cli), None);
    let older = status_line(true, Some(RELEASED), &cli).unwrap_or_default();
    assert!(
        older.starts_with("build: an earlier release (build 839f0344), older than this lev"),
        "{older}"
    );
    assert!(older.contains("`lev daemon restart`"), "{older}");
    let newer = build("cccc", "0.6.5", 1).marker();
    let newer = status_line(true, Some(&newer), &cli).unwrap_or_default();
    assert!(
        newer.starts_with("build: 0.6.5 (build cccc), newer than this lev"),
        "{newer}"
    );
    let tie = build("cccc", "0.6.4", 200).marker();
    let tie = status_line(true, Some(&tie), &cli).unwrap_or_default();
    assert!(
        tie.starts_with("build: 0.6.4 (build cccc), another build of the same version as"),
        "{tie}"
    );
    assert!(tie.contains("`lev daemon restart` replaces it"), "{tie}");
    let home = tempfile::tempdir().unwrap();
    temp_env::with_var("LEVIATH_HOME", Some(home.path()), || {
        assert_eq!(status_line_here(false), None);
    });
}
