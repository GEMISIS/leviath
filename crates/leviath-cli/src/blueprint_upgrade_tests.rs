use super::*;

/// The probe blueprint a 0.6.4 release ran, as an `agent.leviath`.
fn old_probe() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../leviath-legacy-runs/tests/fixtures/agents/probe/agent.leviath");
    std::fs::read_to_string(path).unwrap()
}

/// What a blueprint that migrates with nothing to say comes to.
fn migrated_clean() -> Outcome {
    Outcome::Migrated {
        notes: Vec::new(),
        dropped: Vec::new(),
    }
}

/// The backup of the home at `home`.
fn backup(home: &Path) -> crate::home_backup::Backup {
    crate::home_backup::Backup::of_home(home)
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

    let done = upgrade_all(
        Some(&agents),
        &[],
        &backup(home.path()),
        &StartupBoard::default(),
    );
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].name(), "probe");
    assert_eq!(done[0].outcome, migrated_clean());
    assert!(leviath_blueprint::load(&probe).is_ok());
    assert!(probe.join("legacy/agent.leviath").is_file());
    assert!(!probe.join(OLD_MANIFEST).exists());
    assert!(probe.join("tools/x.rhai").is_file());
    // The directory as it was is in the home's backup.
    let saved = backup(home.path()).dir().join("agents/probe");
    assert_eq!(
        std::fs::read_to_string(saved.join(OLD_MANIFEST)).unwrap(),
        old_probe()
    );
    assert!(saved.join("tools/x.rhai").is_file());
    assert!(!saved.join(leviath_blueprint::FILE_NAME).exists());
    assert!(
        upgrade_all(
            Some(&agents),
            &[],
            &backup(home.path()),
            &StartupBoard::default()
        )
        .is_empty()
    );
}

/// A blueprint that cannot be backed up first is not changed.
#[test]
fn a_blueprint_that_cannot_be_backed_up_is_left_alone() {
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    let dir = agents.join(crate::bundled::BUNDLED_AGENTS[0].name);
    old_blueprint(&dir, b"old");
    std::fs::write(home.path().join(crate::home_backup::BACKUPS_DIR), "a file").unwrap();

    let done = upgrade_all(
        Some(&agents),
        &[],
        &backup(home.path()),
        &StartupBoard::default(),
    );
    let Outcome::Failed(problems) = &done[0].outcome else {
        panic!("{:?}", done[0].outcome);
    };
    assert!(
        problems[0].contains("could not be backed up"),
        "{problems:?}"
    );
    assert_eq!(std::fs::read(dir.join(OLD_MANIFEST)).unwrap(), b"old");
    assert!(!dir.join(LEGACY_DIR).exists());
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

    let done = upgrade_all(
        Some(&agents),
        &[],
        &backup(home.path()),
        &StartupBoard::default(),
    );
    assert_eq!(done[0].outcome, Outcome::Reinstalled);
    assert!(dir.join("legacy/agent.leviath").is_file());
    assert!(dir.join("legacy/tools/x.rhai").is_file());
    let plan = crate::bundled::plan_agent_actions(&agents);
    let action = plan.iter().find(|(a, _)| a.name == bundled.name).unwrap();
    assert_eq!(action.1, crate::bundled::AgentAction::UpToDate);
    let leftovers: Vec<_> = std::fs::read_dir(&agents)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
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
    let staging = format!(".{}.upgrading-{}", bundled.name, std::process::id());
    std::fs::write(agents.join(staging), b"x").unwrap();

    let done = upgrade_all(
        Some(&agents),
        &[],
        &backup(home.path()),
        &StartupBoard::default(),
    );
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

    let done = upgrade_all(
        None,
        &[mine.clone(), lone.clone()],
        &backup(home.path()),
        &StartupBoard::default(),
    );
    let outcome = |dir: &Path| {
        done.iter()
            .find(|d| d.dir == dir)
            .map(|d| d.outcome.clone())
            .unwrap()
    };
    for dir in [&garbled, &blocked] {
        let Outcome::Failed(problems) = outcome(dir) else {
            panic!("{} was migrated", dir.display());
        };
        assert!(!problems.is_empty());
        assert!(dir.join(OLD_MANIFEST).is_file());
        assert!(!dir.join(leviath_blueprint::FILE_NAME).exists());
    }
    assert_eq!(outcome(&named), migrated_clean());
    assert_eq!(outcome(&lone), migrated_clean());
}

/// A key the old parser read nothing from (`max_stored` on a region, as the
/// old docs taught) does not stop the upgrade: the new file leaves it out,
/// the outcome warns about it with its value, and the warning is kept beside
/// the blueprint for `lev list` and `lev validate`.
#[cfg(feature = "legacy-runs")]
#[test]
fn a_key_the_old_parser_ignored_is_dropped_with_a_warning() {
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    let probe = agents.join("probe");
    let text = old_probe().replace("seed = \"task\" }", "seed = \"task\", max_stored = 4 }");
    old_blueprint(&probe, text.as_bytes());
    old_blueprint(&agents.join("logged"), text.as_bytes());
    // One whose note cannot be kept is still upgraded; the log says so.
    let stuck = agents.join("stuck");
    old_blueprint(&stuck, text.as_bytes());
    std::fs::create_dir_all(crate::upgrade_warnings::path(&stuck)).unwrap();
    let done = crate::test_support::with_tracing(|| {
        upgrade_all(
            Some(&agents),
            &[],
            &backup(home.path()),
            &StartupBoard::default(),
        )
    });
    let dropped: Vec<&String> = done
        .iter()
        .flat_map(|d| match &d.outcome {
            Outcome::Migrated { dropped, .. } => dropped.iter(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(dropped.len(), 3, "{dropped:?}");
    assert!(stuck.join(leviath_blueprint::FILE_NAME).is_file());
    assert!(crate::upgrade_warnings::read(&stuck).is_empty());
    assert!(
        dropped
            .iter()
            .all(|n| n.contains("`max_stored = 4` was dropped") && n.contains("never read it")),
        "{dropped:?}"
    );
    assert_eq!(
        crate::upgrade_warnings::read(&probe),
        vec![dropped[1].clone()]
    );
    let new = std::fs::read_to_string(probe.join(leviath_blueprint::FILE_NAME)).unwrap();
    assert!(!new.contains("max_stored"), "{new}");
    // The daemon's log names each note.
    let logged = home.path().join("again");
    old_blueprint(&logged.join("probe"), text.as_bytes());
    let mut upgrade = Upgrade::default();
    crate::test_support::with_tracing(|| {
        upgrade_logged(
            Some(&logged),
            &[],
            &backup(home.path()),
            &StartupBoard::default(),
            &mut upgrade,
        )
    });
    assert_eq!(upgrade.blueprints, 1);
    assert_eq!(upgrade.warnings.len(), 1, "{:?}", upgrade.warnings);
    assert!(
        upgrade.warnings[0]
            .starts_with("blueprint 'probe': region 'task': `max_stored = 4` was dropped"),
        "{:?}",
        upgrade.warnings
    );
    assert!(
        logged
            .join("probe")
            .join(leviath_blueprint::FILE_NAME)
            .is_file()
    );
    assert!(leviath_blueprint::load(&probe).is_ok());
}

/// The daemon logs every outcome, and a command only names what is waiting,
/// leaving every directory as it was.
#[test]
fn the_daemon_upgrades_and_a_command_only_says_so() {
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    old_blueprint(&agents.join(crate::bundled::BUNDLED_AGENTS[0].name), b"old");
    old_blueprint(&agents.join("broken"), b"not toml [");
    old_blueprint(&agents.join("probe"), old_probe().as_bytes());
    let mut upgrade = Upgrade::default();
    let board = StartupBoard::default();
    crate::test_support::with_tracing(|| {
        upgrade_logged(
            Some(&agents),
            &[],
            &backup(home.path()),
            &board,
            &mut upgrade,
        )
    });
    // Three found and done, each counted by what came of it.
    let now = board.current();
    assert_eq!(
        (now.step.as_str(), now.done, now.total),
        ("upgrading blueprints", 3, 3)
    );
    let saving = now.detail.unwrap_or_default();
    assert!(saving.contains(crate::home_backup::BACKUPS_DIR), "{saving}");
    assert_eq!((upgrade.blueprints, upgrade.blueprints_failed), (2, 1));
    assert!(
        upgrade.warnings[0].starts_with("blueprint 'broken' at "),
        "{:?}",
        upgrade.warnings
    );

    let waiting = agents.join("waiting");
    old_blueprint(&waiting, b"not toml [");
    let fine = agents.join("fine");
    old_blueprint(&fine, old_probe().as_bytes());
    let lines = pending_lines(Some(&agents), &[]);
    let named = |name: &str| {
        lines
            .iter()
            .find(|l| l.contains(&format!("'{name}'")))
            .cloned()
            .unwrap_or_default()
    };
    // One the daemon will upgrade says so; one it cannot says why, since a
    // restart would leave it as it is.
    let line = named("fine");
    assert!(line.contains("lev daemon restart"), "{line}");
    assert!(line.contains("lev blueprint migrate"), "{line}");
    // The directory is named in the command that converts it, not before.
    let at = fine.display().to_string();
    assert_eq!(line.matches(&at).count(), 2, "{line}");
    let line = named("waiting");
    assert!(line.contains("could not be upgraded"), "{line}");
    assert!(!line.contains("lev daemon restart"), "{line}");
    assert!(line.contains("lev blueprint migrate"), "{line}");
    assert_eq!(
        std::fs::read(waiting.join(OLD_MANIFEST)).unwrap(),
        b"not toml ["
    );
    assert!(!waiting.join(leviath_blueprint::FILE_NAME).exists());
    assert_eq!(name_of(Path::new("/")), "");
}

/// A daemon over its home's own runs upgrades; one over any other runs
/// directory (a test's) leaves the blueprints alone.
#[test]
fn only_a_daemon_over_its_homes_runs_upgrades() {
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    let runs = home.path().join("runs");
    let probe = agents.join(crate::bundled::BUNDLED_AGENTS[0].name);
    old_blueprint(&probe, b"old");

    let board = StartupBoard::default();
    let mut upgrade = Upgrade::default();
    let elsewhere = home.path().join("elsewhere");
    upgrade_at_start(
        &elsewhere,
        &runs,
        (Some(&agents), &[]),
        &board,
        &mut upgrade,
    );
    assert!(probe.join(OLD_MANIFEST).is_file());

    upgrade_at_start(&runs, &runs, (Some(&agents), &[]), &board, &mut upgrade);
    assert!(probe.join("legacy").join(OLD_MANIFEST).is_file());
    assert_eq!(upgrade.blueprints, 1);
}

/// A rename stand-in that fails on the calls whose numbers (from 1) are in
/// `fail_on`, and does the others for real.
fn renames(fail_on: &'static [usize]) -> impl Fn(&Path, &Path) -> std::io::Result<()> {
    let calls = std::cell::Cell::new(0);
    move |from, to| {
        calls.set(calls.get() + 1);
        match fail_on.contains(&calls.get()) {
            true => Err(std::io::Error::other(format!(
                "rename {} failed",
                calls.get()
            ))),
            false => std::fs::rename(from, to),
        }
    }
}

/// Whatever step of a reinstall fails, the user's old files are never
/// deleted: they are back where they were, or named where they are.
#[test]
fn a_failed_reinstall_never_loses_the_old_files() {
    let bundled = &crate::bundled::BUNDLED_AGENTS[0];
    let setup = || {
        let home = tempfile::tempdir().unwrap();
        let agents = home.path().join("agents");
        let dir = agents.join(bundled.name);
        old_blueprint(&dir, b"mine");
        (home, agents, dir)
    };
    let failed = |r: Result<Outcome, Vec<String>>| r.unwrap_err().join("; ");

    // Moving the old one aside fails: it is untouched.
    let (_home, agents, dir) = setup();
    let e = failed(reinstall_with(&agents, &dir, bundled, &renames(&[1])));
    assert!(e.contains("rename 1 failed"), "{e}");
    assert_eq!(std::fs::read(dir.join(OLD_MANIFEST)).unwrap(), b"mine");

    // Putting the bundled one in place fails: the old one is put back.
    let (_home, agents, dir) = setup();
    let e = failed(reinstall_with(&agents, &dir, bundled, &renames(&[2])));
    assert!(e.contains("rename 2 failed"), "{e}");
    assert_eq!(std::fs::read(dir.join(OLD_MANIFEST)).unwrap(), b"mine");

    // ... and if putting it back fails too, the error says where it is.
    let (_home, agents, dir) = setup();
    let e = failed(reinstall_with(&agents, &dir, bundled, &renames(&[2, 3])));
    let aside = aside_of(&agents, bundled);
    assert!(e.contains(&aside.display().to_string()), "{e}");
    assert_eq!(std::fs::read(aside.join(OLD_MANIFEST)).unwrap(), b"mine");

    // Filing the old one under legacy/ fails: the bundled one is in place
    // and the old files are named where they are.
    let (_home, agents, dir) = setup();
    let e = failed(reinstall_with(&agents, &dir, bundled, &renames(&[3])));
    assert!(e.contains("the bundled one is in place"), "{e}");
    assert!(dir.join(leviath_blueprint::FILE_NAME).is_file());
    assert_eq!(
        std::fs::read(aside_of(&agents, bundled).join(OLD_MANIFEST)).unwrap(),
        b"mine"
    );
}

/// Where a reinstall in this process moves an old directory aside.
fn aside_of(agents: &Path, bundled: &crate::bundled::BundledAgent) -> PathBuf {
    agents.join(format!(".{}.old-{}", bundled.name, std::process::id()))
}
