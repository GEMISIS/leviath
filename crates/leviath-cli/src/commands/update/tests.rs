//! Tests for `lev update`.
//!
//! Nothing here spawns a process. The runner and the confirm prompt are both
//! injected, so every assertion is about the command that *would* run - which
//! is the whole point of the seam.

use std::sync::{Arc, Mutex};

use super::*;
use crate::bundled::BUNDLED_AGENTS;
use crate::test_support::with_tracing;

// ─── Fixtures ─────────────────────────────────────────────────────────────────

/// Records every argv the command hands it, and answers with a scripted result.
#[derive(Default)]
struct Recorder {
    ran: Mutex<Vec<Vec<String>>>,
    asked: Mutex<Vec<String>>,
}

impl Recorder {
    fn ran(&self) -> Vec<Vec<String>> {
        self.ran.lock().expect("not poisoned").clone()
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("not poisoned").clone()
    }
}

/// A scratch environment: a tempdir for the agents and the config, a recorder
/// for the two seams, and whatever answer the prompt should give.
struct Fixture {
    dir: tempfile::TempDir,
    recorder: Arc<Recorder>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("a temp dir"),
            recorder: Arc::new(Recorder::default()),
        }
    }

    /// An env whose binary lives at `exe`, whose prompt answers `answer`, and
    /// whose runner returns `runner_ok`.
    fn env(&self, exe: &str, answer: bool, runner_ok: bool) -> UpdateEnv {
        self.env_with(exe, answer, runner_ok, &[])
    }

    fn env_with(
        &self,
        exe: &str,
        answer: bool,
        runner_ok: bool,
        migrations: &'static [Migration],
    ) -> UpdateEnv {
        let ran = Arc::clone(&self.recorder);
        let asked = Arc::clone(&self.recorder);
        UpdateEnv {
            exe: PathBuf::from(exe),
            home: Some(PathBuf::from("/home/u")),
            brew_prefix: None,
            agents_dir: self.dir.path().join("agents"),
            config_path: self.dir.path().join("config.toml"),
            runner: Arc::new(move |argv: &[String]| {
                ran.ran.lock().expect("not poisoned").push(argv.to_vec());
                match runner_ok {
                    true => Ok(()),
                    false => anyhow::bail!("the upgrade command failed"),
                }
            }),
            confirm: Arc::new(move |question: &str| {
                asked
                    .asked
                    .lock()
                    .expect("not poisoned")
                    .push(question.to_string());
                answer
            }),
            migrations,
        }
    }
}

/// A plan built against a fixture, for the rendering tests.
fn plan_for(fixture: &Fixture, exe: &str, args: &UpdateArgs) -> UpdatePlan {
    plan(args, &fixture.env(exe, true, true))
}

// ─── Channels ─────────────────────────────────────────────────────────────────

/// The three channels, their ids, and the package names that carry them. This
/// is the mapping `lev update` bets everything on: the formula name is the only
/// record of the channel that exists anywhere on disk.
#[test]
fn every_channel_has_an_id_and_a_package_that_round_trip() {
    for (channel, id, package) in [
        (Channel::Stable, "stable", "leviath"),
        (Channel::Beta, "beta", "leviath-beta"),
        (Channel::Alpha, "alpha", "leviath-alpha"),
    ] {
        assert_eq!(channel.id(), id);
        assert_eq!(channel.package(), package);
        assert_eq!(Channel::from_package(package), Some(channel));
    }
    // A formula this build does not ship reads as "no channel", not as stable.
    assert_eq!(Channel::from_package("leviath-nightly"), None);
}

// ─── Detection ────────────────────────────────────────────────────────────────

/// The strongest evidence there is: a Cellar path names the formula outright,
/// and the formula names the channel. No flag is consulted.
#[test]
fn a_cellar_path_names_the_formula_and_therefore_the_channel() {
    for (path, formula, channel) in [
        (
            "/opt/homebrew/Cellar/leviath/0.3.4/bin/lev",
            "leviath",
            Channel::Stable,
        ),
        (
            "/usr/local/Cellar/leviath-beta/0.3.4/bin/lev",
            "leviath-beta",
            Channel::Beta,
        ),
        (
            "/home/linuxbrew/.linuxbrew/Cellar/leviath-alpha/0.3.4/bin/lev",
            "leviath-alpha",
            Channel::Alpha,
        ),
    ] {
        // `--channel stable` is passed every time on purpose: the path wins.
        let method = detect(
            Path::new(path),
            Some(Path::new("/home/u")),
            None,
            Some(Channel::Stable),
        );
        assert_eq!(
            method,
            InstallMethod::Homebrew {
                formula: formula.to_string()
            },
            "{path}"
        );
        assert_eq!(method.channel(), Some(channel));
    }
}

/// An unresolved `bin/lev` symlink under a prefix that means Homebrew and
/// nothing else. There is no formula in the path, so the requested channel is
/// what picks it.
#[test]
fn an_unambiguous_brew_prefix_is_homebrew_at_the_requested_channel() {
    let method = detect(
        Path::new("/opt/homebrew/bin/lev"),
        None,
        None,
        Some(Channel::Beta),
    );
    assert_eq!(
        method,
        InstallMethod::Homebrew {
            formula: "leviath-beta".to_string()
        }
    );
    // Linuxbrew's prefix counts the same way, and with no flag it is stable.
    assert_eq!(
        detect(
            Path::new("/home/linuxbrew/.linuxbrew/bin/lev"),
            None,
            None,
            None
        ),
        InstallMethod::Homebrew {
            formula: "leviath".to_string()
        }
    );
}

/// A custom `brew --prefix` is evidence; `/usr/local` is not.
///
/// This is the case that would do real damage if it were wrong: `/usr/local` is
/// both Homebrew's prefix on Intel macOS *and* the directory the Linux
/// installer hard-codes, so treating the prefix as proof would send every
/// script install to `brew upgrade`.
#[test]
fn a_general_purpose_brew_prefix_is_not_evidence_on_its_own() {
    let custom = detect(
        Path::new("/home/u/homebrew/bin/lev"),
        Some(Path::new("/home/u")),
        Some(Path::new("/home/u/homebrew")),
        None,
    );
    assert_eq!(
        custom,
        InstallMethod::Homebrew {
            formula: "leviath".to_string()
        }
    );

    // The same shape under /usr/local reads as the install script instead.
    assert_eq!(
        detect(
            Path::new("/usr/local/bin/lev"),
            Some(Path::new("/home/u")),
            Some(Path::new("/usr/local")),
            None,
        ),
        InstallMethod::Script {
            channel: Channel::Stable
        }
    );
    // Every prefix the guard treats as too general, including the trailing
    // slash and the degenerate forms a mis-parsed `brew --prefix` could give.
    for prefix in ["/usr/local", "/usr/local/", "/usr", "/", ""] {
        assert!(is_ambiguous_prefix(Path::new(prefix)), "{prefix}");
    }
    assert!(!is_ambiguous_prefix(Path::new("/opt/homebrew")));
}

/// Scoop records the package under `apps/`, the same way Homebrew records the
/// formula under `Cellar/`. A shim has no package in it, so the flag decides.
#[test]
fn scoop_reads_the_package_from_apps_and_falls_back_for_a_shim() {
    assert_eq!(
        detect(
            Path::new("C:/Users/u/scoop/apps/leviath-alpha/current/lev.exe"),
            None,
            None,
            None,
        ),
        InstallMethod::Scoop {
            package: "leviath-alpha".to_string()
        }
    );
    // A shim, with the channel named on the command line. The directory is
    // capitalised to prove the match ignores case, which Windows paths do.
    assert_eq!(
        detect(
            Path::new("C:/Users/u/Scoop/shims/lev.exe"),
            None,
            None,
            Some(Channel::Beta),
        ),
        InstallMethod::Scoop {
            package: "leviath-beta".to_string()
        }
    );
}

/// A cargo install is the one that cannot be updated in place.
#[test]
fn a_binary_under_cargo_bin_is_a_cargo_install() {
    assert_eq!(
        detect(
            Path::new("/home/u/.cargo/bin/lev"),
            Some(Path::new("/home/u")),
            None,
            None,
        ),
        InstallMethod::Cargo
    );
    // crates.io tracks the stable channel, so a cargo install has one even
    // though nothing on disk says so.
    assert_eq!(InstallMethod::Cargo.channel(), Some(Channel::Stable));
    // With no home to resolve, `.cargo/bin` cannot be recognised and the path
    // falls through to the unknown arm rather than being guessed at.
    assert_eq!(
        detect(Path::new("/home/u/.cargo/bin/lev"), None, None, None),
        InstallMethod::Unknown {
            path: PathBuf::from("/home/u/.cargo/bin/lev")
        }
    );
}

/// Every directory an installer actually writes to, and the default channel for
/// the one method that records none.
#[test]
fn a_plain_binary_in_an_installer_destination_is_a_script_install() {
    for dir in [
        "/usr/local/bin",
        "/usr/bin",
        "/home/u/.local/bin",
        "/home/u/AppData/Local/Leviath/bin",
    ] {
        assert_eq!(
            detect(
                &Path::new(dir).join("lev"),
                Some(Path::new("/home/u")),
                None,
                None,
            ),
            InstallMethod::Script {
                channel: Channel::Stable
            },
            "{dir}"
        );
    }
    // The install script keeps no receipt, so `--channel` is the only way to
    // say which one to re-install.
    assert_eq!(
        detect(
            Path::new("/usr/local/bin/lev"),
            None,
            None,
            Some(Channel::Alpha),
        ),
        InstallMethod::Script {
            channel: Channel::Alpha
        }
    );
}

/// Anything else is named rather than guessed at.
#[test]
fn a_binary_somewhere_else_is_unknown() {
    let method = detect(
        Path::new("/home/u/dev/leviath/target/release/lev"),
        Some(Path::new("/home/u")),
        None,
        None,
    );
    assert_eq!(
        method,
        InstallMethod::Unknown {
            path: PathBuf::from("/home/u/dev/leviath/target/release/lev")
        }
    );
    assert_eq!(method.channel(), None, "nothing on disk says which channel");
    // A path with no parent at all still lands here rather than panicking.
    assert!(matches!(
        detect(Path::new("/"), None, None, None),
        InstallMethod::Unknown { .. }
    ));
}

#[test]
fn component_after_finds_the_slot_or_says_there_is_none() {
    let path = Path::new("/opt/homebrew/Cellar/leviath/0.3.4/bin/lev");
    assert_eq!(component_after(path, "Cellar").as_deref(), Some("leviath"));
    // A marker that is not there at all.
    assert_eq!(component_after(path, "apps"), None);
    // A marker in the last slot, so there is nothing after it.
    assert_eq!(component_after(Path::new("/a/Cellar"), "Cellar"), None);
    assert!(has_component(path, "cellar"), "the check ignores case");
    assert!(!has_component(path, "scoop"));
}

#[test]
fn script_destinations_grow_by_two_when_a_home_resolves() {
    assert_eq!(script_destinations(None).len(), SCRIPT_DESTINATIONS.len());
    assert_eq!(
        script_destinations(Some(Path::new("/home/u"))).len(),
        SCRIPT_DESTINATIONS.len() + 2
    );
}

// ─── The command per method ───────────────────────────────────────────────────

/// The exact command each method runs, and the two that run nothing.
#[test]
fn each_install_method_maps_to_one_command() {
    assert_eq!(
        binary_step(&InstallMethod::Homebrew {
            formula: "leviath-beta".to_string()
        }),
        BinaryStep::Run(vec![
            "brew".to_string(),
            "upgrade".to_string(),
            "leviath-beta".to_string()
        ])
    );
    assert_eq!(
        binary_step(&InstallMethod::Scoop {
            package: "leviath-alpha".to_string()
        }),
        BinaryStep::Run(vec![
            "scoop".to_string(),
            "update".to_string(),
            "leviath-alpha".to_string()
        ])
    );
    // A cargo install is a compile. It is described, never started.
    let BinaryStep::Advise(cargo) = binary_step(&InstallMethod::Cargo) else {
        panic!("cargo has nothing to run");
    };
    assert!(cargo.contains("cargo install leviath-cli"), "{cargo}");

    let BinaryStep::Advise(unknown) = binary_step(&InstallMethod::Unknown {
        path: PathBuf::from("/somewhere/odd/lev"),
    }) else {
        panic!("an unknown install has nothing to run");
    };
    assert!(unknown.contains("/somewhere/odd/lev"), "{unknown}");
}

/// The installer invocation, which is the one command here that is easy to get
/// silently wrong.
///
/// `LEVIATH_CHANNEL=beta curl ... | sh` looks right and is not: the two sides
/// of a pipe are separate processes, so the assignment belongs to `curl` and
/// the shell that actually runs the installer never sees it. The installer then
/// takes its own default without complaint. `sh -s -- --channel <c>` passes the
/// channel as an argument to the right process.
#[test]
fn the_install_script_is_invoked_with_the_channel_as_an_argument() {
    for channel in [Channel::Stable, Channel::Beta, Channel::Alpha] {
        let BinaryStep::Run(argv) = binary_step(&InstallMethod::Script { channel }) else {
            panic!("the script method runs a command");
        };
        assert_eq!(argv[0], "sh");
        assert_eq!(argv[1], "-c");
        assert_eq!(
            argv[2],
            format!(
                "curl -fsSL https://leviath.dev/install.sh | sh -s -- --channel {}",
                channel.id()
            )
        );
        assert!(
            !argv[2].contains("LEVIATH_CHANNEL"),
            "the env-assignment form must never be generated: {}",
            argv[2]
        );
    }
}

#[test]
fn every_method_has_an_id_and_a_description() {
    let methods = [
        InstallMethod::Homebrew {
            formula: "leviath".to_string(),
        },
        InstallMethod::Scoop {
            package: "leviath-beta".to_string(),
        },
        InstallMethod::Cargo,
        InstallMethod::Script {
            channel: Channel::Alpha,
        },
        InstallMethod::Unknown {
            path: PathBuf::from("/odd/lev"),
        },
    ];
    let ids: Vec<&str> = methods.iter().map(InstallMethod::id).collect();
    assert_eq!(ids, ["homebrew", "scoop", "cargo", "script", "unknown"]);
    for method in &methods {
        assert!(!method.describe().is_empty());
    }
    // A formula name this build does not ship describes itself without
    // claiming a channel it cannot know.
    let odd = InstallMethod::Homebrew {
        formula: "leviath-nightly".to_string(),
    }
    .describe();
    assert!(odd.contains("leviath-nightly"), "{odd}");
    assert!(!odd.contains("channel"), "{odd}");
    // And a Scoop package this build does not ship, so both `from_package`
    // callers are exercised on a miss.
    assert_eq!(
        InstallMethod::Scoop {
            package: "leviath-nightly".to_string()
        }
        .channel(),
        None
    );
}

// ─── Planning ─────────────────────────────────────────────────────────────────

/// A fresh machine: nothing installed, so every blueprint is offered.
#[test]
fn a_plan_offers_every_blueprint_when_none_is_installed() {
    let fixture = Fixture::new();
    let plan = plan_for(&fixture, "/opt/homebrew/bin/lev", &UpdateArgs::default());

    assert_eq!(plan.agents.len(), BUNDLED_AGENTS.len());
    assert!(plan.agents.iter().all(|(_, a)| a.is_change()));
    assert!(plan.migrations.is_empty(), "none ship today");
    assert!(
        matches!(plan.config, ConfigState::Loaded(_)),
        "a missing config loads as defaults"
    );
}

/// A config that will not parse is reported, not fatal: the binary step is
/// what this command is mostly for and it needs no config at all.
#[test]
fn a_config_that_will_not_parse_is_reported_and_the_rest_still_plans() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.dir.path().join("config.toml"),
        "this is not = = toml",
    )
    .expect("write the broken config");

    let plan = plan_for(&fixture, "/opt/homebrew/bin/lev", &UpdateArgs::default());

    assert!(matches!(plan.config, ConfigState::Unreadable(_)));
    assert!(plan.migrations.is_empty());
    assert!(matches!(plan.binary, BinaryStep::Run(_)), "still planned");
}

/// The sample migration the shipped list is empty of. This is what proves the
/// mechanism runs rather than being wiring nobody has ever exercised.
static SAMPLE: &[Migration] = &[
    Migration {
        name: "sample-default-provider",
        description: "point a config with no default_provider at ollama",
        applies: |config, raw| config.default_provider != "ollama" && !raw.is_empty(),
        apply: |config| {
            let was = std::mem::replace(&mut config.default_provider, "ollama".to_string());
            vec![format!("default_provider: {was} -> ollama")]
        },
    },
    Migration {
        name: "sample-never-applies",
        description: "a migration whose predicate is false for this config",
        applies: |_, _| false,
        apply: |_| vec!["unreachable".to_string()],
    },
];

/// A migration that applies to any config at all, for the tests that need the
/// write to be reached rather than the predicate to be interesting.
static ALWAYS: &[Migration] = &[Migration {
    name: "sample-always-applies",
    description: "a migration every config needs",
    applies: |_, _| true,
    apply: |config| {
        config.default_provider = "ollama".to_string();
        vec!["default_provider -> ollama".to_string()]
    },
}];

/// The predicate decides, and it sees both the parsed config and the raw
/// document - which is the whole reason it takes two arguments.
#[test]
fn a_migration_is_selected_by_its_predicate_over_the_config_and_the_raw_toml() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.dir.path().join("config.toml"),
        "default_provider = \"anthropic\"\n",
    )
    .expect("write the config");
    let env = fixture.env_with("/opt/homebrew/bin/lev", true, true, SAMPLE);

    let selected = plan(&UpdateArgs::default(), &env);

    let names: Vec<&str> = selected.migrations.iter().map(|m| m.name).collect();
    assert_eq!(
        names,
        ["sample-default-provider"],
        "only the one whose predicate holds"
    );

    // With no file at all the raw document is empty, so the same predicate
    // says no - the arm that only the raw half of the pair can reach.
    let empty = Fixture::new();
    let env = empty.env_with("/opt/homebrew/bin/lev", true, true, SAMPLE);
    assert!(plan(&UpdateArgs::default(), &env).migrations.is_empty());
}

#[test]
fn the_shipped_migration_list_is_empty_and_that_is_deliberate() {
    // No released config.toml has to change to work with this build. The list
    // is the place a future one goes; the tests above prove it would run.
    assert!(MIGRATIONS.is_empty());
}

// ─── Rendering ────────────────────────────────────────────────────────────────

#[test]
fn the_report_names_the_method_the_command_and_the_blueprints() {
    let fixture = Fixture::new();
    let plan = plan_for(
        &fixture,
        "/usr/local/Cellar/leviath-beta/0.3.4/bin/lev",
        &UpdateArgs::default(),
    );

    let report = format_plan(&plan, "0.3.4");

    assert!(
        report.contains("lev 0.3.4, installed with Homebrew"),
        "{report}"
    );
    assert!(report.contains("beta channel"), "{report}");
    assert!(report.contains("brew upgrade leviath-beta"), "{report}");
    assert!(
        report.contains(&format!(
            "{} of {} would change",
            BUNDLED_AGENTS.len(),
            BUNDLED_AGENTS.len()
        )),
        "{report}"
    );
    assert!(report.contains("nothing to migrate"), "{report}");
}

/// The other side of each branch: nothing to install, nothing to run, and a
/// config that could not be read.
#[test]
fn the_report_says_so_when_there_is_nothing_to_do() {
    let fixture = Fixture::new();
    let agents_dir = fixture.dir.path().join("agents");
    for agent in BUNDLED_AGENTS {
        install_bundled(agent, &agents_dir).expect("install the bundled set");
    }
    std::fs::write(fixture.dir.path().join("config.toml"), "nope = = nope")
        .expect("write the broken config");

    let plan = plan_for(&fixture, "/home/u/.cargo/bin/lev", &UpdateArgs::default());
    let report = format_plan(&plan, "0.3.4");

    assert!(report.contains("cargo install leviath-cli"), "{report}");
    assert!(
        report.contains(&format!(
            "all {} bundled blueprints are up to date",
            BUNDLED_AGENTS.len()
        )),
        "{report}"
    );
    assert!(report.contains("could not be read"), "{report}");
}

/// A pending migration is listed by name and description before anything is
/// touched, which is the promise the command makes about the config.
#[test]
fn the_report_lists_every_pending_migration() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.dir.path().join("config.toml"),
        "default_provider = \"anthropic\"\n",
    )
    .expect("write the config");
    let env = fixture.env_with("/opt/homebrew/bin/lev", true, true, SAMPLE);

    let report = format_plan(&plan(&UpdateArgs::default(), &env), "0.3.4");

    assert!(report.contains("1 migration(s)"), "{report}");
    assert!(report.contains("sample-default-provider"), "{report}");
    assert!(
        report.contains("point a config with no default_provider"),
        "{report}"
    );
}

#[test]
fn the_json_shape_carries_the_method_channel_command_and_rows() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.dir.path().join("config.toml"),
        "default_provider = \"anthropic\"\n",
    )
    .expect("write the config");
    let env = fixture.env_with("/opt/homebrew/bin/lev", true, true, SAMPLE);

    let json = plan_json(&plan(&UpdateArgs::default(), &env), "0.3.4");

    assert_eq!(json["version"], "0.3.4");
    assert_eq!(json["install_method"], "homebrew");
    assert_eq!(json["channel"], "stable");
    assert_eq!(json["binary"]["action"], "run");
    assert_eq!(json["binary"]["command"][0], "brew");
    assert_eq!(json["agents"][0]["changes"], true);
    assert_eq!(json["migrations"][0]["name"], "sample-default-provider");
    assert_eq!(json["config_error"], serde_json::Value::Null);

    // The advise arm, and a channel nothing can name.
    let plan = plan_for(
        &fixture,
        "/home/u/dev/target/release/lev",
        &UpdateArgs::default(),
    );
    let json = plan_json(&plan, "0.3.4");
    assert_eq!(json["install_method"], "unknown");
    assert_eq!(json["channel"], serde_json::Value::Null);
    assert_eq!(json["binary"]["action"], "advise");
    assert!(json["binary"]["message"].is_string());

    // And a config that will not parse, which is the one thing `--json` has to
    // say that the happy path never produces.
    let broken = Fixture::new();
    std::fs::write(broken.dir.path().join("config.toml"), "nope = = nope")
        .expect("write the broken config");
    let json = plan_json(
        &plan_for(&broken, "/opt/homebrew/bin/lev", &UpdateArgs::default()),
        "0.3.4",
    );
    assert!(
        json["config_error"].as_str().is_some_and(|e| !e.is_empty()),
        "{json}"
    );
}

// ─── Execution ────────────────────────────────────────────────────────────────

/// `--check` prints the plan and stops. Nothing is run, nothing is asked, and
/// nothing is written.
#[test]
fn check_reports_and_changes_nothing() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let args = UpdateArgs {
            check: true,
            ..UpdateArgs::default()
        };
        let env = fixture.env("/opt/homebrew/bin/lev", true, true);

        execute_with(&args, &env, "0.3.4").expect("check never fails");

        assert!(fixture.recorder.ran().is_empty());
        assert!(fixture.recorder.asked().is_empty());
        assert!(!fixture.dir.path().join("agents").exists());
    });
}

/// `--json` is the machine-readable twin of `--check`: same guarantee, and the
/// output parses.
#[test]
fn json_reports_and_changes_nothing() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let args = UpdateArgs {
            json: true,
            ..UpdateArgs::default()
        };
        let env = fixture.env("/opt/homebrew/bin/lev", true, true);

        execute_with(&args, &env, "0.3.4").expect("json never fails");

        assert!(fixture.recorder.ran().is_empty());
        assert!(fixture.recorder.asked().is_empty());
    });
}

/// The happy path, unattended: `--yes --install-agents` answers for the user,
/// the command runs, and every clean blueprint is installed.
#[test]
fn yes_and_install_agents_run_the_command_and_install_the_blueprints() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let args = UpdateArgs {
            yes: true,
            install_agents: true,
            ..UpdateArgs::default()
        };
        // The prompt answers `false`, which must not matter: the flags are
        // what decide, and asking at all would be the bug.
        let env = fixture.env(
            "/opt/homebrew/Cellar/leviath-beta/0.3.4/bin/lev",
            false,
            true,
        );

        execute_with(&args, &env, "0.3.4").expect("the whole flow succeeds");

        assert_eq!(
            fixture.recorder.ran(),
            vec![vec![
                "brew".to_string(),
                "upgrade".to_string(),
                "leviath-beta".to_string()
            ]]
        );
        assert!(
            fixture.recorder.asked().is_empty(),
            "the two flags between them answer everything a clean install asks"
        );
        for agent in BUNDLED_AGENTS {
            assert!(
                env.agents_dir
                    .join(agent.name)
                    .join("agent.leviath")
                    .exists(),
                "{} was not installed",
                agent.name
            );
        }
    });
}

/// A blueprint the user edited is asked about on its own, and no flag covers
/// it.
///
/// `install_bundled` removes the destination directory first, so agreeing in
/// bulk would silently destroy work - the user's edits and any file they added
/// alongside. Both flags are on here, and neither answers this question.
#[test]
fn an_edited_blueprint_is_asked_about_separately_whatever_the_flags_say() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let agents_dir = fixture.dir.path().join("agents");
        for agent in BUNDLED_AGENTS {
            install_bundled(agent, &agents_dir).expect("install the bundled set");
        }
        let edited = &BUNDLED_AGENTS[0];
        let manifest = agents_dir.join(edited.name).join("agent.leviath");
        let original = std::fs::read_to_string(&manifest).expect("read it back");
        std::fs::write(&manifest, format!("{original}\n# a local edit\n")).expect("edit it");

        let args = UpdateArgs {
            yes: true,
            install_agents: true,
            ..UpdateArgs::default()
        };
        // The prompt says no, so the edit survives.
        let env = fixture.env("/opt/homebrew/bin/lev", false, true);
        execute_with(&args, &env, "0.3.4").expect("the flow succeeds");

        let asked = fixture.recorder.asked();
        assert_eq!(asked.len(), 1, "exactly the edited blueprint: {asked:?}");
        assert!(asked[0].contains(edited.name), "{asked:?}");
        assert!(
            std::fs::read_to_string(&manifest)
                .expect("still there")
                .contains("# a local edit"),
            "an edited blueprint was overwritten on a blanket yes"
        );
    });
}

/// The same blueprint, with the user saying yes to the separate question.
#[test]
fn an_edited_blueprint_is_reinstalled_when_the_user_says_so() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let agents_dir = fixture.dir.path().join("agents");
        let edited = &BUNDLED_AGENTS[0];
        install_bundled(edited, &agents_dir).expect("install one");
        let manifest = agents_dir.join(edited.name).join("agent.leviath");
        let original = std::fs::read_to_string(&manifest).expect("read it back");
        std::fs::write(&manifest, format!("{original}\n# a local edit\n")).expect("edit it");

        let env = fixture.env("/opt/homebrew/bin/lev", true, true);
        execute_with(&UpdateArgs::default(), &env, "0.3.4").expect("the flow succeeds");

        assert!(
            !std::fs::read_to_string(&manifest)
                .expect("still there")
                .contains("# a local edit"),
            "an explicit yes reinstalls it"
        );
    });
}

/// Saying no leaves everything exactly as it was.
#[test]
fn declining_leaves_the_binary_and_the_blueprints_alone() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let env = fixture.env("/opt/homebrew/bin/lev", false, true);

        execute_with(&UpdateArgs::default(), &env, "0.3.4").expect("declining is not a failure");

        assert!(fixture.recorder.ran().is_empty());
        assert!(!env.agents_dir.exists(), "nothing was installed");
        // Two questions: the binary, and the whole clean blueprint set at once.
        // One prompt per blueprint is how a person stops reading them.
        assert_eq!(fixture.recorder.asked().len(), 2);
    });
}

/// The blueprints and the config are checked whatever the binary step did.
///
/// This is the case the command exists for. Someone who ran `brew upgrade`
/// themselves has a current binary and blueprints from whenever they last ran
/// `lev setup`, so a binary step that reported "nothing to do" and stopped
/// would leave them exactly where they started.
#[test]
fn a_binary_with_nothing_to_do_still_reaches_the_blueprints_and_the_config() {
    with_tracing(|| {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.dir.path().join("config.toml"),
            "default_provider = \"anthropic\"\n",
        )
        .expect("write the config");
        let args = UpdateArgs {
            yes: true,
            install_agents: true,
            ..UpdateArgs::default()
        };
        // `cargo` is the method whose binary step runs nothing at all, which is
        // the strongest form of "the binary had nothing to do".
        let env = fixture.env_with("/home/u/.cargo/bin/lev", true, true, SAMPLE);

        execute_with(&args, &env, "0.3.4").expect("the flow succeeds");

        assert!(fixture.recorder.ran().is_empty(), "no binary step ran");
        for agent in BUNDLED_AGENTS {
            assert!(
                env.agents_dir
                    .join(agent.name)
                    .join("agent.leviath")
                    .exists(),
                "{} was not offered",
                agent.name
            );
        }
        let after = Config::load_from_path_public(&env.config_path).expect("still parses");
        assert_eq!(
            after.default_provider, "ollama",
            "the config migration ran too"
        );
    });
}

/// `--dry-run` walks the whole flow, prompts and all, and performs none of it.
#[test]
fn dry_run_prompts_but_performs_nothing() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let args = UpdateArgs {
            dry_run: true,
            yes: true,
            install_agents: true,
            ..UpdateArgs::default()
        };
        let env = fixture.env("/opt/homebrew/bin/lev", true, true);

        execute_with(&args, &env, "0.3.4").expect("a dry run succeeds");

        assert!(fixture.recorder.ran().is_empty(), "nothing was spawned");
        assert!(!env.agents_dir.exists(), "nothing was written");
    });
}

/// An upgrade command that fails is the command's failure, so the shell sees a
/// non-zero exit and a scripted update does not report success.
#[test]
fn a_failing_upgrade_command_fails_the_command() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let args = UpdateArgs {
            yes: true,
            ..UpdateArgs::default()
        };
        let env = fixture.env("/opt/homebrew/bin/lev", true, false);

        let err = execute_with(&args, &env, "0.3.4").expect_err("the runner failed");

        assert!(err.to_string().contains("the upgrade command failed"));
    });
}

/// A method with nothing to run prints its advice and carries on to the rest.
#[test]
fn a_cargo_install_is_advised_and_the_blueprints_still_run() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let args = UpdateArgs {
            yes: true,
            ..UpdateArgs::default()
        };
        let env = fixture.env("/home/u/.cargo/bin/lev", true, true);

        execute_with(&args, &env, "0.3.4").expect("advice is not a failure");

        assert!(fixture.recorder.ran().is_empty(), "no compile was started");
        assert!(env.agents_dir.exists(), "the blueprints still installed");
    });
}

/// Nothing to install, so it says so rather than printing an empty list.
#[test]
fn an_up_to_date_install_says_there_is_nothing_to_do() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let agents_dir = fixture.dir.path().join("agents");
        for agent in BUNDLED_AGENTS {
            install_bundled(agent, &agents_dir).expect("install the bundled set");
        }
        let args = UpdateArgs {
            yes: true,
            ..UpdateArgs::default()
        };
        let env = fixture.env("/opt/homebrew/bin/lev", true, true);

        execute_with(&args, &env, "0.3.4").expect("nothing to do is not a failure");

        assert_eq!(
            fixture.recorder.asked().len(),
            0,
            "--yes covered the binary, and there was nothing else to ask about"
        );
    });
}

/// An install that fails is a warning, not an abort: an updated binary plus
/// most of the blueprints beats a command that gave up halfway.
#[test]
fn a_blueprint_that_cannot_be_installed_warns_rather_than_aborting() {
    with_tracing(|| {
        let fixture = Fixture::new();
        // The agents directory is a file, so every install fails.
        std::fs::write(fixture.dir.path().join("agents"), "not a directory")
            .expect("block the directory");
        let args = UpdateArgs {
            yes: true,
            ..UpdateArgs::default()
        };
        let env = fixture.env("/opt/homebrew/bin/lev", true, true);

        execute_with(&args, &env, "0.3.4").expect("a failed install is a warning");

        assert_eq!(fixture.recorder.ran().len(), 1, "the binary still updated");
    });
}

// ─── Config migration ─────────────────────────────────────────────────────────

/// The full config path: the change is described, agreed to, and written.
#[test]
fn an_agreed_migration_is_described_and_then_written() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let config_path = fixture.dir.path().join("config.toml");
        std::fs::write(&config_path, "default_provider = \"anthropic\"\n").expect("write it");
        let args = UpdateArgs {
            yes: true,
            ..UpdateArgs::default()
        };
        let env = fixture.env_with("/opt/homebrew/bin/lev", true, true, SAMPLE);

        execute_with(&args, &env, "0.3.4").expect("the migration applies");

        let after = Config::load_from_path_public(&config_path).expect("still parses");
        assert_eq!(after.default_provider, "ollama");
    });
}

/// Declining leaves the file byte for byte as it was.
#[test]
fn a_declined_migration_writes_nothing() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let config_path = fixture.dir.path().join("config.toml");
        let before = "default_provider = \"anthropic\"\n";
        std::fs::write(&config_path, before).expect("write it");
        let env = fixture.env_with("/opt/homebrew/bin/lev", false, true, SAMPLE);

        execute_with(&UpdateArgs::default(), &env, "0.3.4").expect("declining is fine");

        assert_eq!(
            std::fs::read_to_string(&config_path).expect("still there"),
            before
        );
    });
}

/// `--dry-run` reaches the config step, agrees, and still writes nothing.
#[test]
fn a_dry_run_migration_writes_nothing() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let config_path = fixture.dir.path().join("config.toml");
        let before = "default_provider = \"anthropic\"\n";
        std::fs::write(&config_path, before).expect("write it");
        let args = UpdateArgs {
            dry_run: true,
            yes: true,
            ..UpdateArgs::default()
        };
        let env = fixture.env_with("/opt/homebrew/bin/lev", true, true, SAMPLE);

        execute_with(&args, &env, "0.3.4").expect("a dry run succeeds");

        assert_eq!(
            std::fs::read_to_string(&config_path).expect("still there"),
            before
        );
    });
}

/// A config that will not parse is left alone and said so, rather than being
/// rewritten from the defaults - which would silently drop every key in it.
#[test]
fn a_config_that_will_not_parse_is_left_alone() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let config_path = fixture.dir.path().join("config.toml");
        let before = "this is not = = toml";
        std::fs::write(&config_path, before).expect("write it");
        let args = UpdateArgs {
            yes: true,
            ..UpdateArgs::default()
        };
        let env = fixture.env_with("/opt/homebrew/bin/lev", true, true, SAMPLE);

        execute_with(&args, &env, "0.3.4").expect("a broken config is not fatal");

        assert_eq!(
            std::fs::read_to_string(&config_path).expect("still there"),
            before
        );
    });
}

/// A config that cannot be written surfaces, rather than reporting a change
/// that did not happen.
#[test]
fn a_config_that_cannot_be_written_is_an_error() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let config_path = fixture.dir.path().join("config.toml");
        std::fs::write(&config_path, "default_provider = \"anthropic\"\n").expect("write it");
        let args = UpdateArgs {
            yes: true,
            ..UpdateArgs::default()
        };
        let mut env = fixture.env_with("/opt/homebrew/bin/lev", true, true, SAMPLE);
        // A directory in place of the file: it reads (through the parent that
        // still holds the real config) but cannot be written over.
        let blocked = fixture.dir.path().join("blocked");
        std::fs::write(&blocked, "not a directory").expect("block it");
        env.config_path = blocked.join("config.toml");

        // Nothing to migrate at that path, so force the write path by pointing
        // the read at the real file and the write at the blocked one.
        let plan = UpdatePlan {
            method: InstallMethod::Cargo,
            binary: binary_step(&InstallMethod::Cargo),
            agents: Vec::new(),
            migrations: SAMPLE.iter().take(1).collect(),
            config: ConfigState::Loaded(Box::default()),
        };
        let err = migrate_config(&args, &env, &plan).expect_err("the write failed");
        assert!(!err.to_string().is_empty());
    });
}

/// And that failure is the command's failure, so a scripted update exits
/// non-zero rather than reporting a config change that never landed.
#[test]
fn a_config_write_failure_fails_the_whole_command() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let args = UpdateArgs {
            yes: true,
            ..UpdateArgs::default()
        };
        // A file where the config's parent directory should be: the read finds
        // nothing and loads the defaults, and the write cannot create the
        // directory it would need.
        let blocked = fixture.dir.path().join("blocked");
        std::fs::write(&blocked, "not a directory").expect("block it");
        let mut env = fixture.env_with("/home/u/.cargo/bin/lev", true, true, ALWAYS);
        env.config_path = blocked.join("config.toml");

        let err = execute_with(&args, &env, "0.3.4").expect_err("the write failed");

        assert!(!err.to_string().is_empty());
    });
}

#[test]
fn load_config_reads_the_document_behind_the_parsed_value() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("config.toml");

    // No file: the defaults, and an empty document behind them.
    let loaded = load_config(&path).expect("a missing config loads as defaults");
    assert!(loaded.raw.is_empty());
    assert_eq!(loaded.config.default_provider, "anthropic");

    std::fs::write(&path, "default_provider = \"ollama\"\n").expect("write it");
    let loaded = load_config(&path).expect("it parses");
    assert_eq!(loaded.config.default_provider, "ollama");
    assert!(loaded.raw.contains_key("default_provider"));

    std::fs::write(&path, "nope = = nope").expect("write it");
    assert!(load_config(&path).is_err());
}

/// The two-arm helper behind every prompt in the command.
#[test]
fn agreed_asks_unless_yes_already_answered() {
    let fixture = Fixture::new();
    let env = fixture.env("/opt/homebrew/bin/lev", false, true);
    let yes = UpdateArgs {
        yes: true,
        ..UpdateArgs::default()
    };

    assert!(agreed(&yes, &env, "anything?"), "--yes answers");
    assert!(fixture.recorder.asked().is_empty(), "and does not ask");
    assert!(!agreed(&UpdateArgs::default(), &env, "anything?"));
    assert_eq!(fixture.recorder.asked(), vec!["anything?".to_string()]);
}
