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
            // Declines, so these tests keep asserting on a plan built without a
            // network call - which is what every one of them was written
            // against. The lookup has its own tests beside it.
            latest: Arc::new(|_: &str| Err("no network in tests".to_string())),
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

/// The exact commands each method runs, and the two that run nothing.
///
/// Both package managers refresh their index first. Without it they answer
/// from metadata they already had, so a release published minutes earlier is
/// invisible and the upgrade reports the installed version as the newest one -
/// which is what sent people to `brew update && brew upgrade leviath` by hand.
#[test]
fn each_install_method_maps_to_its_commands() {
    assert_eq!(
        binary_step(&InstallMethod::Homebrew {
            formula: "leviath-beta".to_string()
        }),
        BinaryStep::Run(vec![
            vec!["brew".to_string(), "update".to_string()],
            vec![
                "brew".to_string(),
                "upgrade".to_string(),
                "leviath-beta".to_string()
            ]
        ])
    );
    assert_eq!(
        binary_step(&InstallMethod::Scoop {
            package: "leviath-alpha".to_string()
        }),
        BinaryStep::Run(vec![
            vec!["scoop".to_string(), "update".to_string()],
            vec![
                "scoop".to_string(),
                "update".to_string(),
                "leviath-alpha".to_string()
            ]
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

/// The plan shows every command it will run, joined the way a user would type
/// them, so `--check` and the confirmation prompt name the refresh as well as
/// the upgrade.
#[test]
fn the_rendered_commands_read_as_one_shell_line() {
    assert_eq!(
        render_commands(&[
            vec!["brew".to_string(), "update".to_string()],
            vec![
                "brew".to_string(),
                "upgrade".to_string(),
                "leviath".to_string()
            ],
        ]),
        "brew update && brew upgrade leviath"
    );
    // One command renders as itself, with no stray separator.
    assert_eq!(
        render_commands(&[vec!["scoop".to_string(), "update".to_string()]]),
        "scoop update"
    );
}

/// A failed index refresh stops there. Upgrading against metadata that failed
/// to refresh is the situation this whole change exists to avoid, so it must
/// not happen quietly as a second command.
#[test]
fn a_failed_first_command_does_not_run_the_second() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let args = UpdateArgs {
            yes: true,
            ..UpdateArgs::default()
        };
        // `false` makes every runner call fail, so the first one does.
        let env = fixture.env("/opt/homebrew/Cellar/leviath/0.3.4/bin/lev", false, false);

        let err =
            execute_with(&args, &env, "0.3.4").expect_err("a failed refresh fails the update");

        assert!(err.to_string().contains("failed"), "{err}");
        assert_eq!(
            fixture.recorder.ran(),
            vec![vec!["brew".to_string(), "update".to_string()]],
            "the upgrade never ran"
        );
    });
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
        let BinaryStep::Run(commands) = binary_step(&InstallMethod::Script { channel }) else {
            panic!("the script method runs a command");
        };
        // The installer fetches what it needs itself, so there is nothing to
        // refresh in front of it: one command, not two.
        assert_eq!(commands.len(), 1);
        let argv = &commands[0];
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

/// Every shipped migration is named, described, and does nothing to a config
/// that has already taken it.
///
/// This replaced an assertion that the list was empty, which was true until a
/// released `config.toml` first had something in it worth changing. Emptiness
/// was never the property worth holding - these are, because a migration ships
/// forever and runs on every `lev update` after the one that needed it.
#[test]
fn every_shipped_migration_is_named_and_idempotent() {
    assert!(
        !MIGRATIONS.is_empty(),
        "the machinery is exercised by what ships, not only by a sample"
    );
    for migration in MIGRATIONS {
        assert!(!migration.name.is_empty(), "a migration needs a name");
        assert!(
            !migration.description.is_empty(),
            "{}: needs a line saying what it changes",
            migration.name
        );
        // A default config has taken every migration by construction: it is
        // what a fresh install writes.
        let fresh = crate::config::Config::default();
        assert!(
            !(migration.applies)(&fresh, &toml::Table::new()),
            "{}: applies to a config that never needed it",
            migration.name
        );
    }
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

    let json = plan_json(
        &plan(&UpdateArgs::default(), &env),
        "0.3.4",
        &crate::commands::update::latest::LatestCheck::default(),
    );

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
    let json = plan_json(
        &plan,
        "0.3.4",
        &crate::commands::update::latest::LatestCheck::default(),
    );
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
        &crate::commands::update::latest::LatestCheck::default(),
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

        // Both commands actually reach the runner, in order. The refresh is
        // the half that was missing, so asserting only the upgrade would pass
        // against the bug this fixes.
        assert_eq!(
            fixture.recorder.ran(),
            vec![
                vec!["brew".to_string(), "update".to_string()],
                vec![
                    "brew".to_string(),
                    "upgrade".to_string(),
                    "leviath-beta".to_string()
                ]
            ]
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

        // Two: the index refresh and the upgrade itself.
        assert_eq!(fixture.recorder.ran().len(), 2, "the binary still updated");
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

// ─── The real machine ─────────────────────────────────────────────────────────

/// An `Output` carrying `stdout`, for the parsing test below. The status is
/// borrowed from a real spawn because `ExitStatus` cannot be constructed
/// portably, and it is the one field `brew_prefix_from` never reads.
#[cfg(test)]
fn output_with(stdout: &[u8]) -> std::process::Output {
    let status = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .arg("--a-flag-that-lists-no-tests-and-runs-none")
        .arg("--list")
        .output()
        .expect("spawning the test binary to borrow an ExitStatus")
        .status;
    std::process::Output {
        status,
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
    }
}

/// `brew --prefix` is evidence, never a requirement, so every way of not
/// answering has to come out as "no evidence" rather than as a failure.
///
/// The empty case is the one worth stating outright: an empty prefix is not
/// merely useless, it is dangerous. [`detect`] asks whether the executable path
/// `starts_with` the prefix, and every path starts with an empty one - so a
/// blank answer from `brew` would make every install on the machine look like a
/// Homebrew install.
#[test]
fn brew_prefix_treats_every_non_answer_as_no_evidence() {
    // No `brew` on this machine at all.
    assert_eq!(brew_prefix_from(None), None);
    // A `brew` that answered with nothing, or with only whitespace.
    assert_eq!(brew_prefix_from(Some(output_with(b""))), None);
    assert_eq!(brew_prefix_from(Some(output_with(b"  \n "))), None);
    // Output that is not text.
    assert_eq!(brew_prefix_from(Some(output_with(&[0xff, 0xfe]))), None);
    // And the answer it is there for, trailing newline and all.
    assert_eq!(
        brew_prefix_from(Some(output_with(b"/opt/homebrew\n"))),
        Some(PathBuf::from("/opt/homebrew"))
    );
}

/// The cached wrapper resolves, and keeps resolving to the same thing. Which
/// answer it gives is a fact about whatever machine is running the tests, not
/// about this code - the parsing is pinned above - so this asserts only that
/// the cache works.
#[test]
fn brew_prefix_is_cached_and_stable() {
    assert_eq!(brew_prefix(), brew_prefix());
}

/// A planning environment describes this machine and refuses to act on it.
///
/// The refusals matter more than they look. `plan` never reaches them, so
/// nothing today would notice if they were no-ops - and a no-op runner is
/// exactly how a caller who wired this into `execute_with` would get an update
/// that looked successful and changed nothing.
#[test]
fn a_planning_environment_describes_the_machine_and_refuses_to_change_it() {
    let env = UpdateEnv::for_planning();
    assert_eq!(env.migrations.len(), MIGRATIONS.len());
    assert!(env.config_path.ends_with("config.toml"));

    let refused = (env.runner)(&["brew".to_string(), "upgrade".to_string()]);
    let message = refused
        .expect_err("a planning env runs nothing")
        .to_string();
    assert!(message.contains("built for planning only"), "{message}");
    assert!(!(env.confirm)("upgrade?"));
}

/// Whatever this test binary is, planning against it reaches a verdict rather
/// than failing. That is what `UpdateEnv::real` is infallible for: a copy in a
/// place no installer uses is `Unknown` with advice, which is an answer, and
/// refusing to answer is not.
#[test]
fn planning_against_the_real_machine_always_reaches_a_verdict() {
    let plan = plan(&UpdateArgs::default(), &UpdateEnv::for_planning());
    assert!(!plan.method.describe().is_empty());
    match &plan.binary {
        BinaryStep::Run(commands) => assert!(!commands.is_empty()),
        BinaryStep::Advise(message) => assert!(!message.is_empty()),
    }
}

/// The line `lev update` prints about whether the update is worth doing. Silent
/// when the check could not answer: the command was asked how to update, not
/// whether to, and a failed lookup of something nobody asked for is noise.
#[test]
fn the_update_line_speaks_only_when_it_has_something_to_say() {
    use crate::commands::update::latest::LatestCheck;

    let available = LatestCheck {
        latest: Some("0.5.0".to_string()),
        update_available: Some(true),
        checked_at: Some(1),
    };
    let line = super::format_latest(&available, "0.4.0");
    assert!(line.contains("0.5.0 is available"), "{line}");
    assert!(line.contains("0.4.0"), "it says what you have: {line}");

    let current = LatestCheck {
        latest: Some("0.4.0".to_string()),
        update_available: Some(false),
        checked_at: Some(1),
    };
    assert!(
        super::format_latest(&current, "0.4.0").contains("newest on this channel"),
        "a current copy is told so"
    );

    assert_eq!(
        super::format_latest(&LatestCheck::default(), "0.4.0"),
        "",
        "an unanswered check prints nothing at all"
    );
}

/// A copy whose channel could not be worked out is not asked about. The only
/// answer available would be the stable release compared against a build that
/// may not be on that line, which is the guess this exists to stop making.
#[test]
fn a_copy_with_no_known_channel_is_not_asked_about() {
    use crate::commands::update::latest::LatestCheck;

    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let env = {
        let asked = Arc::clone(&asked);
        UpdateEnv {
            latest: Arc::new(move |_: &str| {
                asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(r#"{"name": "9.9.9"}"#.to_string())
            }),
            ..UpdateEnv::for_planning()
        }
    };

    // A path no installer uses detects as `Unknown`, which carries no channel.
    let unknown = plan_for(
        &Fixture::new(),
        "/nowhere/at/all/lev",
        &UpdateArgs::default(),
    );
    assert_eq!(
        super::check_latest(&unknown, &env, "0.4.0"),
        LatestCheck::default()
    );
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// The offline env's fetcher is a refusal, not a no-op that quietly returns a
/// stale-looking answer.
#[test]
fn the_offline_env_declines_to_look_anything_up() {
    let env = UpdateEnv::for_planning_offline();
    assert!((env.latest)("https://example.invalid").is_err());
}

/// The switch is on when nobody has said otherwise, and an existing config
/// written before the key existed means on.
///
/// `#[serde(default)]` on a `bool` is `false`, so getting this wrong would have
/// silently switched the check off for every install that upgraded into it -
/// the opposite of what leaving a key out means.
#[test]
fn the_update_check_is_on_unless_a_config_turns_it_off() {
    let absent: crate::config::Config =
        toml::from_str("").expect("an empty config is a valid config");
    assert!(absent.update_check, "a config that says nothing means on");
    assert!(
        crate::config::Config::default().update_check,
        "and so does the default"
    );

    let off: crate::config::Config =
        toml::from_str("update_check = false").expect("a valid config");
    assert!(!off.update_check);
}

/// Switched off, the env declines to look anything up - so the CLI and the API
/// both report "cannot tell" rather than one of them quietly still asking.
#[test]
fn a_config_that_turns_the_check_off_declines_to_ask() {
    let on =
        UpdateEnv::real_with_config(Arc::new(|_: &[String]| Ok(())), Arc::new(|_| false), true);
    let off =
        UpdateEnv::real_with_config(Arc::new(|_: &[String]| Ok(())), Arc::new(|_| false), false);

    assert!(
        (off.latest)("https://example.invalid").is_err(),
        "switched off, nothing is looked up"
    );
    // The `on` env carries the real fetcher. Asserting on its identity rather
    // than calling it: calling it would reach the network from a test.
    assert!(
        !std::ptr::addr_eq(Arc::as_ptr(&on.latest), Arc::as_ptr(&off.latest)),
        "the two envs do not share one fetcher"
    );
}

/// The whole point of the change: somebody who installed the new binary their
/// own way runs `lev update` for the blueprints, and is not first asked to run a
/// package manager over a binary with nothing to fetch.
#[test]
fn a_current_copy_is_not_offered_a_binary_upgrade() {
    use crate::commands::update::latest::LatestCheck;

    let current = LatestCheck {
        latest: Some("0.5.0".to_string()),
        update_available: Some(false),
        checked_at: Some(1),
    };
    assert!(
        super::format_latest(&current, "0.5.0").contains("newest on this channel"),
        "and it says so rather than staying silent"
    );
}

/// Not knowing is not the same as being current. A check that could not run -
/// switched off, no network, no channel to ask about - still offers the upgrade,
/// because refusing to offer it would be acting on an answer nobody has.
#[test]
fn an_unanswered_check_still_offers_the_upgrade() {
    use crate::commands::update::latest::LatestCheck;

    let unknown = LatestCheck::default();
    assert_eq!(
        unknown.update_available, None,
        "the state that must not be read as `already current`"
    );
    assert_eq!(
        super::format_latest(&unknown, "0.5.0"),
        "",
        "and nothing is claimed about it on the terminal"
    );
}

/// The decision the binary step turns on, as data.
#[test]
fn the_binary_step_is_skipped_only_on_a_definite_current() {
    use crate::commands::update::latest::LatestCheck;

    let current = LatestCheck {
        latest: Some("0.5.0".to_string()),
        update_available: Some(false),
        checked_at: Some(1),
    };
    assert!(
        !super::binary_step_needed(&current),
        "nothing to fetch, so nothing to offer"
    );

    let behind = LatestCheck {
        update_available: Some(true),
        ..current.clone()
    };
    assert!(super::binary_step_needed(&behind));

    assert!(
        super::binary_step_needed(&LatestCheck::default()),
        "not knowing is not the same as being current"
    );
}

/// End to end: a copy the check says is current runs no package-manager command
/// at all, and still goes on to the blueprint and migration steps.
///
/// This is the case somebody who installed the binary their own way is in, and
/// before this they had to decline a `brew upgrade` prompt to reach the parts
/// they came for.
#[test]
fn a_current_copy_runs_no_upgrade_command_but_still_updates_blueprints() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let args = UpdateArgs {
            yes: true,
            ..UpdateArgs::default()
        };
        // A Homebrew copy, so the plan *would* carry `brew update && brew
        // upgrade` - and a fetcher that reports this very version as newest.
        let mut env = fixture.env("/opt/homebrew/Cellar/leviath/0.3.4/bin/lev", true, true);
        env.latest = Arc::new(|_: &str| Ok(r#"{"name": "0.3.4"}"#.to_string()));

        execute_with(&args, &env, "0.3.4").expect("nothing to run, nothing to fail");

        assert!(
            fixture.recorder.ran().is_empty(),
            "no command was offered: {:?}",
            fixture.recorder.ran()
        );
    });
}

/// And the other way: a copy that is behind is still offered the upgrade.
#[test]
fn a_copy_that_is_behind_still_runs_the_upgrade() {
    with_tracing(|| {
        let fixture = Fixture::new();
        let args = UpdateArgs {
            yes: true,
            ..UpdateArgs::default()
        };
        let mut env = fixture.env("/opt/homebrew/Cellar/leviath/0.3.4/bin/lev", true, true);
        env.latest = Arc::new(|_: &str| Ok(r#"{"name": "9.9.9"}"#.to_string()));

        execute_with(&args, &env, "0.3.4").expect("the runner succeeds");

        assert!(
            !fixture.recorder.ran().is_empty(),
            "a copy that is behind is offered the upgrade"
        );
    });
}

/// The migration on the shape it was written for: a real config carrying the
/// stale line, run through `applies` and `apply`, and saved.
#[test]
fn the_stale_serves_migration_removes_the_line_and_leaves_the_rest() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "default_provider = \"openrouter\"\n\n\
         [model_providers.cerebras]\n\
         base_url = \"https://api.cerebras.ai/v1\"\n\
         serves = []\n\n\
         [model_providers.keeps-its-list]\n\
         serves = [\"a-model\"]\n",
    )
    .expect("writes");

    let mut config =
        crate::config::Config::load_from_path_public(&path).expect("the fixture parses");
    let raw: toml::Table =
        toml::from_str(&std::fs::read_to_string(&path).expect("reads")).expect("parses");

    let migration = &crate::commands::update::MIGRATIONS[0];
    assert!(
        (migration.applies)(&config, &raw),
        "a config carrying the stale line needs it"
    );

    let done = (migration.apply)(&mut config);
    assert_eq!(done.len(), 1, "one provider changed, not both: {done:?}");
    assert!(done[0].contains("cerebras"), "{done:?}");

    config.save_to_path_public(&path).expect("saves");
    let written = std::fs::read_to_string(&path).expect("reads back");

    assert!(
        !written.contains("serves = []"),
        "the empty one is gone:\n{written}"
    );
    assert!(
        written.contains("a-model"),
        "and a real list is untouched:\n{written}"
    );
    assert!(
        written.contains("cerebras"),
        "as is the provider itself:\n{written}"
    );

    // Run again and it has nothing to do, which is what makes it safe to keep
    // shipping after everyone has taken it.
    let after = crate::config::Config::load_from_path_public(&path).expect("still parses");
    let raw_after: toml::Table = toml::from_str(&written).expect("parses");
    assert!(!(migration.applies)(&after, &raw_after));
}

/// A config that never had the line is left alone entirely.
#[test]
fn the_stale_serves_migration_does_not_apply_to_a_clean_config() {
    let config = crate::config::Config::default();
    let raw = toml::Table::new();
    assert!(!(crate::commands::update::MIGRATIONS[0].applies)(
        &config, &raw
    ));
}
