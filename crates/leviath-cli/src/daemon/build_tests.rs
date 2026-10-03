use super::*;

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
/// is older, and two builds of one commit count as older so a rebuilt `lev`
/// replaces its daemon.
#[test]
fn a_daemon_stands_older_or_newer_by_version_then_commit_time() {
    let cli = build("bbbb", "0.6.4", 200);
    assert_eq!(cli.standing(&cli), Standing::Same);
    assert_eq!(Build::parse(RELEASED).standing(&cli), Standing::Older);
    assert_eq!(build("aaaa", "0.6.4", 100).standing(&cli), Standing::Older);
    assert_eq!(build("cccc", "0.6.4", 300).standing(&cli), Standing::Newer);
    assert_eq!(build("cccc", "0.6.4", 200).standing(&cli), Standing::Older);
    // The version outranks the commit time, and 0.6.10 is after 0.6.9.
    assert_eq!(build("cccc", "0.6.3", 900).standing(&cli), Standing::Older);
    assert_eq!(build("cccc", "0.6.10", 1).standing(&cli), Standing::Newer);
    assert_eq!(
        build("cccc", "0.6.5-beta.1", 1).standing(&cli),
        Standing::Newer
    );
    // One that recorded no commit time cannot be newer by it.
    let untimed = Build {
        committed_at: None,
        ..build("cccc", "0.6.4", 0)
    };
    assert_eq!(untimed.standing(&cli), Standing::Older);
    assert_eq!(untimed.marker(), "cccc 0.6.4");
}

/// A spawn replaces only an older daemon, and says which is which while it
/// does; the same build and a newer one are left running.
#[test]
fn only_an_older_daemon_is_replaced() {
    let cli = build("bbbb", "0.6.4", 200);
    let same = replace(Some(&cli.marker()), &cli);
    assert_eq!((same.replace, same.say), (false, None));
    let newer = replace(Some(&build("cccc", "0.6.5", 1).marker()), &cli);
    assert_eq!((newer.replace, newer.say), (false, None));
    let older = replace(Some(RELEASED), &cli);
    assert!(older.replace);
    let said = older.say.unwrap_or_default();
    assert!(
        said.contains(
            "an earlier release (build 839f0344), older than this lev (0.6.4 (build bbbb))"
        ),
        "{said}"
    );
    assert!(replace(None, &cli).replace, "a daemon that wrote no marker");
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
/// this `lev`, which is newer, and what to do; nothing while they match or
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
    let home = tempfile::tempdir().unwrap();
    temp_env::with_var("LEVIATH_HOME", Some(home.path()), || {
        assert_eq!(status_line_here(false), None);
    });
}
