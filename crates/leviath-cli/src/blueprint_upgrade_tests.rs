use super::*;

/// The probe blueprint a 0.6.4 release ran, as an `agent.leviath`.
fn old_probe() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../leviath-legacy-runs/tests/fixtures/agents/probe/agent.leviath");
    std::fs::read_to_string(path).unwrap()
}

/// An old blueprint directory at `dir`, holding `manifest` and a script.
fn old_blueprint(dir: &Path, manifest: &[u8]) {
    std::fs::create_dir_all(dir.join("tools")).unwrap();
    std::fs::write(dir.join(OLD_MANIFEST), manifest).unwrap();
    std::fs::write(dir.join("tools/x.rhai"), "fn run(a) { a }").unwrap();
}

/// An old blueprint of the user's own becomes an `agent.toml` beside its
/// scripts, which this build reads; the old manifest is kept under
/// `legacy/`, and a second pass finds nothing to do.
#[cfg(feature = "legacy-runs")]
#[test]
fn an_old_blueprint_is_migrated_beside_its_files() {
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    let probe = agents.join("probe");
    old_blueprint(&probe, old_probe().as_bytes());

    let done = upgrade_all(Some(&agents), &[]);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].name(), "probe");
    assert_eq!(done[0].outcome, Outcome::Migrated);
    assert!(leviath_blueprint::load(&probe).is_ok());
    assert!(probe.join("legacy/agent.leviath").is_file());
    assert!(!probe.join(OLD_MANIFEST).exists());
    assert!(probe.join("tools/x.rhai").is_file());
    assert!(upgrade_all(Some(&agents), &[]).is_empty());
}

/// An old install of a blueprint this build ships is replaced by the bundled
/// one, which then reads as up to date; the whole old directory is kept.
#[test]
fn an_old_install_of_a_bundled_blueprint_is_replaced() {
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    let bundled = &crate::bundled::BUNDLED_AGENTS[0];
    let dir = agents.join(bundled.name);
    old_blueprint(&dir, b"[agent]\nname = \"whatever\"\n");

    let done = upgrade_all(Some(&agents), &[]);
    assert_eq!(done[0].outcome, Outcome::Reinstalled);
    assert!(dir.join("legacy/agent.leviath").is_file());
    assert!(dir.join("legacy/tools/x.rhai").is_file());
    let plan = crate::bundled::plan_agent_actions(&agents);
    let action = plan.iter().find(|(a, _)| a.name == bundled.name).unwrap();
    assert_eq!(action.1, crate::bundled::AgentAction::UpToDate);
    assert!(!agents.join(format!(".{}.upgrading", bundled.name)).exists());
}

/// A bundled blueprint whose replacement cannot be written is left as it was.
#[test]
fn a_bundled_blueprint_that_cannot_be_replaced_is_left_alone() {
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    let bundled = &crate::bundled::BUNDLED_AGENTS[0];
    let dir = agents.join(bundled.name);
    old_blueprint(&dir, b"old");
    // Where the replacement is staged is taken by a file.
    std::fs::write(agents.join(format!(".{}.upgrading", bundled.name)), b"x").unwrap();

    let done = upgrade_all(Some(&agents), &[]);
    let Outcome::Failed(problems) = &done[0].outcome else {
        panic!("the replacement could not be staged");
    };
    assert!(problems[0].contains("could not replace it"));
    assert_eq!(std::fs::read(dir.join(OLD_MANIFEST)).unwrap(), b"old");
}

/// A blueprint that would lose a setting, does not read, or whose new file
/// cannot be written is left exactly as it was, with every problem named.
/// Outside the installed agents a blueprint named like a bundled one is
/// migrated, never replaced.
#[cfg(feature = "legacy-runs")]
#[test]
fn a_blueprint_that_cannot_be_migrated_is_left_as_it_was() {
    let home = tempfile::tempdir().unwrap();
    let mine = home.path().join("mine");
    let unread = mine.join("unread");
    old_blueprint(
        &unread,
        format!("{}\nmystery = 1\n", old_probe()).as_bytes(),
    );
    let garbled = mine.join("garbled");
    old_blueprint(&garbled, &[0xff, 0xfe]);
    let blocked = mine.join("blocked");
    old_blueprint(&blocked, old_probe().as_bytes());
    std::fs::write(blocked.join(LEGACY_DIR), b"in the way").unwrap();
    let named = mine.join(crate::bundled::BUNDLED_AGENTS[0].name);
    old_blueprint(&named, old_probe().as_bytes());
    // An `agent_paths` entry that is itself a blueprint directory.
    let lone = home.path().join("lone");
    old_blueprint(&lone, old_probe().as_bytes());

    let done = upgrade_all(None, &[mine.clone(), lone.clone()]);
    let outcome = |dir: &Path| {
        done.iter()
            .find(|d| d.dir == dir)
            .map(|d| d.outcome.clone())
            .unwrap()
    };
    for dir in [&unread, &garbled, &blocked] {
        let Outcome::Failed(problems) = outcome(dir) else {
            panic!("{} was migrated", dir.display());
        };
        assert!(!problems.is_empty());
        assert!(dir.join(OLD_MANIFEST).is_file());
        assert!(!dir.join(leviath_blueprint::FILE_NAME).exists());
    }
    assert_eq!(outcome(&named), Outcome::Migrated);
    assert_eq!(outcome(&lone), Outcome::Migrated);

    let lines = report_lines(&done);
    assert!(lines.iter().any(|l| l.contains("mystery")));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("migrated blueprint 'lone'"))
    );
}

/// What a command and the daemon say about each upgrade.
#[test]
fn every_outcome_is_said() {
    let done = vec![
        Upgraded {
            dir: PathBuf::from("/a/one"),
            outcome: Outcome::Migrated,
        },
        Upgraded {
            dir: PathBuf::from("/a/two"),
            outcome: Outcome::Reinstalled,
        },
        Upgraded {
            dir: PathBuf::from("/"),
            outcome: Outcome::Failed(vec!["bad key".into()]),
        },
    ];
    let lines = report_lines(&done);
    assert!(lines[0].starts_with("migrated blueprint 'one'"));
    assert!(lines[1].contains("bundled blueprint 'two'"));
    assert!(lines[2].contains("blueprint ''"));
    assert_eq!(lines[3], "  - bad key");

    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    old_blueprint(&agents.join(crate::bundled::BUNDLED_AGENTS[0].name), b"old");
    old_blueprint(&agents.join("broken"), b"not toml [");
    old_blueprint(&agents.join("probe"), old_probe().as_bytes());
    crate::test_support::with_tracing(|| upgrade_logged(Some(&agents), &[]));
    old_blueprint(&agents.join("broken2"), b"not toml [");
    upgrade_reported(Some(&agents), &[]);
    assert!(agents.join("broken2").join(OLD_MANIFEST).is_file());
}
