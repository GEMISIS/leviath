//! A `lev run` command line read into the request it asks for.

use leviath_core::mime::InboundPart;
use leviath_runtime::spec::launch::Unattended;
use leviath_runtime::spec::request::Attachment;

use super::*;

/// `run`'s input `name`, when it is text.
fn input_of(run: &LocalRun, name: &str) -> Option<String> {
    match run.request.inputs.get(name) {
        Some(RawInput::Text(t)) => Some(t.clone()),
        _ => None,
    }
}

/// `run`'s task, or nothing.
fn task_of(run: &LocalRun) -> String {
    input_of(run, TASK_INPUT).unwrap_or_default()
}

/// What the command line itself found wrong with `run`, as reported.
fn refused(run: anyhow::Result<LocalRun>) -> String {
    let run = run.unwrap();
    assert!(
        !run.issues.is_empty(),
        "the command line found nothing wrong"
    );
    issues_report(&run.issues)
}

/// The directory of the blueprint `run` asked for.
fn source_name(run: &LocalRun) -> String {
    serde_json::to_value(&run.request.source).unwrap()["blueprint_file"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The region an attachment names, as text.
fn region_of(part: &Attachment) -> Option<&str> {
    part.region.as_ref().map(|r| r.as_str())
}

/// The coder blueprint in `dir`.
fn write_manifest(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("agent.toml"),
        crate::test_support::inline_coder_manifest(),
    )
    .unwrap();
    dir.join("agent.toml")
}

/// A blueprint in `dir` named `name` whose context holds `regions`, each a
/// pinned region taking the input of the same name: `(name, required,
/// accepts)`.
fn write_regions(dir: &Path, name: &str, regions: &[(&str, bool, &str)]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let layout: Vec<String> = regions
        .iter()
        .map(|(region, _, accepts)| {
            format!(
                "{{ name = \"{region}\", kind = \"pinned\", budget = 4000, accepts = [{accepts}] }}"
            )
        })
        .collect();
    let inputs: Vec<String> = regions
        .iter()
        .map(|(region, required, _)| {
            format!(
                "{{ name = \"{region}\", type = {{ kind = \"text\", multiline = true }}, required = {required}, binds = [{{ region = \"{region}\" }}] }}"
            )
        })
        .collect();
    std::fs::write(
        dir.join("agent.toml"),
        format!(
            "[blueprint]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[graph]\n\
             stages = [{{ name = \"main\", model = {{ models = [{{ provider = \"anthropic\", model = \"claude-sonnet-5\" }}] }} }}]\n\
             inputs = [{}]\n\
             layout = {{ total_budget_tokens = 18000, regions = [{}, \
             {{ name = \"conversation\", kind = {{ kind = \"sliding_window\", max_items = 20 }}, budget = 10000 }}] }}\n",
            inputs.join(", "),
            layout.join(", ")
        ),
    )
    .unwrap();
    dir.join("agent.toml")
}

/// A blueprint driven by a named input, taking no task at all.
const DIFF_ONLY: &[(&str, bool, &str)] = &[("diff", false, "")];
/// A task it does not insist on, and a `diff` beside it.
const DIFF_OR_TASK: &[(&str, bool, &str)] = &[("task", false, ""), ("diff", false, "")];
/// A task and an `art` input whose region takes pictures.
const TASK_AND_ART: &[(&str, bool, &str)] = &[("task", false, ""), ("art", false, "\"image/*\"")];

/// A command line naming `manifest`, working in `/work`, reading paths
/// against `cwd`.
fn line<'a>(manifest: &'a Path, cwd: &'a Path) -> RunLine<'a> {
    RunLine::new(Some(manifest.to_str().unwrap()), "/work", cwd)
}

#[test]
fn a_blueprint_and_its_task_make_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(&dir.path().join("my-agent"));
    let run = run_request(RunLine {
        task: Some("do it"),
        model: Some("m".to_string()),
        ..line(&manifest, dir.path())
    })
    .unwrap();
    assert!(source_name(&run).ends_with("my-agent"));
    assert_eq!(task_of(&run), "do it");
    assert_eq!(
        run.request
            .model
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("m")
    );
    assert_eq!(
        run.manifest,
        std::fs::canonicalize(dir.path().join("my-agent"))
            .unwrap()
            .join("agent.toml")
    );
    assert_eq!(run.workdir, "/work");
    assert_eq!(run.request.workdir.as_deref(), Some(Path::new("/work")));
}

/// The daemon has its own working directory, so a relative `PATH` has to be
/// resolved before the request leaves: `lev run .` reaching the daemon as
/// `./agent.toml` fails there, and it is the very command `lev create`
/// prints as the next step.
#[test]
fn a_relative_blueprint_path_is_sent_absolute() {
    // Reading the CWD races the tests that move it; take their lock.
    let _guard = crate::config::isolate_cwd_for_test();
    // Rooted in the current directory, so the relative path exists: a temp
    // dir can be on another drive on Windows.
    let dir = tempfile::Builder::new()
        .prefix("lev-relpath-")
        .tempdir_in(".")
        .unwrap();
    write_manifest(&dir.path().join("my-agent"));
    let relative = Path::new(".")
        .join(dir.path().file_name().unwrap())
        .join("my-agent");
    let run = run_request(RunLine {
        task: Some("do it"),
        ..RunLine::new(relative.to_str(), "/work", dir.path())
    })
    .unwrap();
    assert!(run.manifest.is_absolute());
    assert!(Path::new(&source_name(&run)).is_absolute());
}

/// An installed blueprint is asked for by its name and read from the
/// installed copy, the way the daemon reads it.
#[test]
fn an_installed_blueprint_is_named_by_its_name() {
    crate::config::with_isolated_config_path("run-request-installed", |home| {
        write_manifest(&home.join(".leviath").join("agents").join("coder"));
        let cwd = tempfile::tempdir().unwrap();
        let run = run_request(RunLine {
            task: Some("t"),
            ..RunLine::new(Some("coder"), "/w", cwd.path())
        })
        .unwrap();
        assert!(
            matches!(&run.request.source, SpawnSource::Blueprint(r) if r.name.as_str() == "coder"),
            "{:?}",
            run.request.source
        );
        assert!(
            run.manifest.ends_with("coder/agent.toml"),
            "{:?}",
            run.manifest
        );
    });
}

#[test]
fn a_blueprint_that_is_not_there_or_does_not_load_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let err = run_request(RunLine {
        task: Some("t"),
        ..RunLine::new(Some("/no/such/agent"), "/work", dir.path())
    })
    .unwrap_err();
    assert!(!err.to_string().is_empty());

    let broken = dir.path().join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("agent.toml"), "this is : not = valid toml [[[").unwrap();
    let err = run_request(RunLine {
        task: Some("t"),
        ..line(&broken.join("agent.toml"), dir.path())
    })
    .unwrap_err();
    assert!(
        err.to_string().contains("is not a valid blueprint"),
        "{err}"
    );
}

/// `--task <file>` reads the file, and no `--task` with no terminal is
/// refused here, before the daemon is asked.
#[test]
fn the_task_is_read_from_a_file_or_demanded() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(&dir.path().join("my-agent"));
    let task = dir.path().join("task.md");
    std::fs::write(&task, "  summarize the README  \n").unwrap();
    let run = run_request(RunLine {
        task: task.to_str(),
        ..line(&manifest, dir.path())
    })
    .unwrap();
    assert_eq!(task_of(&run), "summarize the README");

    let err = run_request(line(&manifest, dir.path())).unwrap_err();
    assert!(err.to_string().contains("No task provided"), "{err}");

    // `--check` does not stop to ask: the daemon reports a missing task with
    // every other problem.
    let run = run_request(RunLine {
        ask_for_task: false,
        ..line(&manifest, dir.path())
    })
    .unwrap();
    assert!(!run.request.inputs.contains_key(TASK_INPUT));
}

/// A task that is optional is not demanded when something else was handed
/// in; a required one still is; a blueprint with no task never asks, and
/// refuses one, naming what it takes.
#[test]
fn a_task_is_asked_for_only_when_the_run_needs_one() {
    let dir = tempfile::tempdir().unwrap();
    let optional = write_regions(
        &dir.path().join("diff-or-task"),
        "diff-or-task",
        DIFF_OR_TASK,
    );
    let diff = || HashMap::from([("diff".to_string(), "- a\n+ b".to_string())]);
    let run = run_request(RunLine {
        named: diff(),
        ..line(&optional, dir.path())
    })
    .unwrap();
    assert_eq!(task_of(&run), "");
    assert_eq!(input_of(&run, "diff").as_deref(), Some("- a\n+ b"));
    let err = run_request(line(&optional, dir.path())).unwrap_err();
    assert!(err.to_string().contains("No task provided"), "{err}");

    let insisting = write_regions(
        &dir.path().join("diff-or-task"),
        "diff-or-task",
        &[("task", true, ""), ("diff", false, "")],
    );
    assert_eq!(insisting, optional);
    let err = run_request(RunLine {
        named: diff(),
        ..line(&optional, dir.path())
    })
    .unwrap_err();
    assert!(err.to_string().contains("No task provided"), "{err}");

    let taskless = write_regions(&dir.path().join("diffonly"), "diffonly", DIFF_ONLY);
    let run = run_request(RunLine {
        named: diff(),
        ..line(&taskless, dir.path())
    })
    .unwrap();
    assert_eq!(task_of(&run), "");
    // A blank task is no task, so it is not refused either.
    run_request(RunLine {
        task: Some("   "),
        ..line(&taskless, dir.path())
    })
    .unwrap();
    let err = refused(run_request(RunLine {
        task: Some("review my code"),
        ..line(&taskless, dir.path())
    }));
    assert!(
        err.contains("inputs.task: unknown: diffonly takes no task"),
        "{err}"
    );
    assert!(err.contains("--input <name>=<value>"), "{err}");
    assert!(err.contains("diff"), "{err}");
}

/// `--input` and `--<name>` both give inputs, typed by their declarations;
/// a name nothing declares, or a value that will not read, is refused with
/// every other problem, before the task is asked for.
#[test]
fn inputs_are_read_by_their_declared_types() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_regions(&dir.path().join("reviewer"), "reviewer", TASK_AND_ART);
    let policy = dir.path().join("policy.md");
    std::fs::write(&policy, "  focus on safety  ").unwrap();
    let run = run_request(RunLine {
        task: Some("review it"),
        inputs: vec![format!("art=@{}", policy.display())],
        ..line(&manifest, dir.path())
    })
    .unwrap();
    assert_eq!(input_of(&run, "art").as_deref(), Some("focus on safety"));

    // No task was given, and nobody is asked for one: the run is refused
    // anyway, and the daemon's word on the missing task is not news.
    let run = run_request(RunLine {
        inputs: vec!["bogus=1".to_string(), "no-equals".to_string()],
        named: HashMap::from([
            ("art".to_string(), "@/no/such/file.md".to_string()),
            ("other".to_string(), "x".to_string()),
        ]),
        ..line(&manifest, dir.path())
    })
    .unwrap();
    assert!(run.task_unasked);
    assert!(!run.request.inputs.contains_key(TASK_INPUT));
    let err = issues_report(&run.issues);
    assert!(err.starts_with("4 problems with this run:"), "{err}");
    assert!(
        err.contains("inputs.bogus: unknown: --input names no input"),
        "{err}"
    );
    assert!(
        err.contains("inputs.other: unknown: --other names no input"),
        "{err}"
    );
    assert!(err.contains("not name=value"), "{err}");
    assert!(err.contains("Failed to read region file"), "{err}");

    // The daemon's answer joins them: what it says at a path the command
    // line already refused is left out, and so is the unasked task.
    let task = SpecPath::root().field("inputs").key(TASK_INPUT);
    let daemon = SpawnIssues(vec![
        SpawnIssue::new(
            SpecPath::root().field("inputs").key("art"),
            IssueCode::Missing,
            "required",
        ),
        SpawnIssue::new(task.clone(), IssueCode::Missing, "no task"),
        SpawnIssue::new(SpecPath::root().field("workdir"), IssueCode::Missing, "w"),
    ]);
    let all = merged_issues(&run, daemon.clone());
    let paths: Vec<String> = all.iter().map(|i| i.path.to_string()).collect();
    assert_eq!(paths.len(), 5, "{paths:?}");
    assert_eq!(paths[4], "workdir");
    let asked = LocalRun {
        task_unasked: false,
        ..run
    };
    assert!(merged_issues(&asked, daemon).iter().any(|i| i.path == task));

    // Values typed by the dashboard go straight on the request.
    let run = run_request(RunLine {
        task: Some("t"),
        values: BTreeMap::from([("art".to_string(), RawInput::Text("given".into()))]),
        ..line(&manifest, dir.path())
    })
    .unwrap();
    assert_eq!(input_of(&run, "art").as_deref(), Some("given"));
}

/// Every flag that does not read is an issue at the field it would set, all
/// at once: the model, a yolo profile name, an allowed tool, the output shape.
#[test]
fn launch_flags_that_do_not_read_are_refused_together() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(&dir.path().join("a"));
    let err = refused(run_request(RunLine {
        task: Some("t"),
        model: Some("not a model".to_string()),
        yolo: true,
        yolo_profile: Some("bad\nname".to_string()),
        allow: vec!["ok_tool".to_string(), "bad tool".to_string()],
        output_request: Some(leviath_core::output::OutputSpec {
            format: Some("json".to_string()),
            instructions: None,
            example: None,
            schema: None,
            validator: None,
            on_validator_error: None,
            overwrite_artifacts: None,
            artifacts: vec![leviath_core::output::ArtifactSpec {
                name: "final".to_string(),
                mime_type: "not a type".to_string(),
                required: false,
                description: None,
            }],
        }),
        ..line(&manifest, dir.path())
    }));
    assert!(err.contains("model: invalid"), "{err}");
    assert!(err.contains("launch.unattended: invalid"), "{err}");
    assert!(err.contains("launch.allow[1]: invalid"), "{err}");
    assert!(!err.contains("launch.allow[0]"), "{err}");
    assert!(err.contains("output.artifacts[0]: invalid"), "{err}");
}

/// The launch flags land on the request: unattended (plain or under a
/// profile), the allowed tools, the depth, seed commands off, and the output
/// shape.
#[test]
fn launch_flags_land_on_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(&dir.path().join("a"));
    let run = run_request(RunLine {
        task: Some("t"),
        yolo: true,
        yolo_profile: Some("careful".to_string()),
        allow: vec!["read_file".to_string()],
        max_depth: Some(1000),
        no_seed_commands: true,
        output_request: crate::commands::run::output_request(Some("json".into()), None, None)
            .unwrap(),
        model: Some("  ".to_string()),
        ..line(&manifest, dir.path())
    })
    .unwrap();
    let launch = &run.request.launch;
    assert_eq!(
        launch.unattended,
        Unattended::Profile(leviath_runtime::spec::names::ProfileName::new("careful").unwrap())
    );
    assert_eq!(launch.allow.len(), 1);
    assert_eq!(launch.max_depth, Some(u8::MAX));
    assert!(!launch.seed_commands);
    assert!(run.request.output.is_some());
    assert!(run.request.model.is_none(), "a blank model is no model");
    assert!(run.yolo);
    assert_eq!(run.yolo_profile.as_deref(), Some("careful"));
    let run = run_request(RunLine {
        task: Some("t"),
        yolo: true,
        ..line(&manifest, dir.path())
    })
    .unwrap();
    assert_eq!(run.request.launch.unattended, Unattended::All);
}

/// Files attached, named by a region flag, and named in the task all become
/// attachments, in their regions; an exact repeat is dropped.
#[test]
fn every_file_becomes_an_attachment_once() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_regions(&dir.path().join("artist"), "artist", TASK_AND_ART);
    let png = dir.path().join("hero.png");
    let bytes = b"\x89PNG\r\n\x1a\nbody".to_vec();
    std::fs::write(&png, &bytes).unwrap();
    let task = format!(
        "edit @{} so the arm is longer, not @nothing.png",
        png.display()
    );
    let run = run_request(RunLine {
        task: Some(&task),
        named: HashMap::from([("art".to_string(), format!("@{}", png.display()))]),
        parts: vec![
            InboundPart::from_bytes("extra.wav", vec![1, 2, 3]),
            InboundPart::from_bytes("hero.png", bytes),
        ],
        ..line(&manifest, dir.path())
    })
    .unwrap();
    assert_eq!(task_of(&run), task);
    assert!(
        !run.request.inputs.contains_key("art"),
        "a picture is a part"
    );
    let got: Vec<(Option<&str>, &str)> = run
        .request
        .attachments
        .iter()
        .map(|p| (region_of(p), p.name.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (None, "extra.wav"),
            (None, "hero.png"),
            (Some("art"), "hero.png")
        ]
    );

    // A task naming an empty file is refused before anything is dialled.
    let empty = dir.path().join("empty.png");
    std::fs::write(&empty, b"").unwrap();
    let task = format!("edit @{}", empty.display());
    let err = run_request(RunLine {
        task: Some(&task),
        ..line(&manifest, dir.path())
    })
    .unwrap_err();
    assert!(err.to_string().contains("nothing to attach"), "{err}");
}

/// A whole request from a file: TOML or JSON, its own workdir kept unless
/// one is asked for, its inputs read against the graph it carries, and the
/// command line's flags over it.
#[test]
fn a_request_file_is_sent_with_the_flags_over_it() {
    let dir = tempfile::tempdir().unwrap();
    let graph =
        leviath_blueprint::BlueprintFile::parse(&crate::test_support::inline_coder_manifest())
            .unwrap()
            .graph;
    let mut request = SpawnRequest::new(SpawnSource::Raw(Box::new(graph)));
    request.workdir = Some(PathBuf::from("/theirs"));
    request.launch.unattended = leviath_runtime::spec::launch::Unattended::Profile(
        leviath_runtime::spec::names::ProfileName::new("careful").unwrap(),
    );
    let toml_file = dir.path().join("run.toml");
    std::fs::write(&toml_file, toml::to_string(&request).unwrap()).unwrap();
    let json_file = dir.path().join("run.json");
    std::fs::write(&json_file, serde_json::to_string(&request).unwrap()).unwrap();

    let from_toml = RunLine {
        request_file: Some(&toml_file),
        task: Some("from a file"),
        ..RunLine::new(None, "/mine", dir.path())
    };
    let run = run_request(from_toml).unwrap();
    assert_eq!(task_of(&run), "from a file");
    assert_eq!(run.request.workdir.as_deref(), Some(Path::new("/theirs")));
    // What the run says about itself is what the file asked for, not the
    // command line's own defaults.
    assert_eq!(run.workdir, "/theirs");
    assert!(run.yolo);
    assert_eq!(run.yolo_profile.as_deref(), Some("careful"));
    assert_eq!(run.manifest, PathBuf::new(), "a raw graph has no manifest");
    let run = run_request(RunLine {
        request_file: Some(&json_file),
        task: Some("t"),
        workdir_given: true,
        ..RunLine::new(None, "/mine", dir.path())
    })
    .unwrap();
    assert_eq!(run.request.workdir.as_deref(), Some(Path::new("/mine")));
    assert_eq!(run.workdir, "/mine");
    let run = run_request(RunLine {
        request_file: Some(&json_file),
        task: Some("t"),
        yolo: true,
        ..RunLine::new(None, "/mine", dir.path())
    })
    .unwrap();
    assert!(
        run.yolo && run.yolo_profile.is_none(),
        "the bare flag is over it"
    );

    // An untitled graph is "this run" where it is named.
    let mut untitled = request.clone();
    if let SpawnSource::Raw(graph) = &mut untitled.source {
        graph.title = None;
        graph.inputs.clear();
    }
    std::fs::write(&json_file, serde_json::to_string(&untitled).unwrap()).unwrap();
    let err = refused(run_request(RunLine {
        request_file: Some(&json_file),
        task: Some("t"),
        ..RunLine::new(None, "/mine", dir.path())
    }));
    assert!(err.contains("this run takes no task"), "{err}");
}

/// A request file names what it runs, so naming a blueprint as well is
/// refused; one that is not there, or not a request, says so.
#[test]
fn a_request_file_that_will_not_do_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(&dir.path().join("a"));
    let file = dir.path().join("r.json");
    std::fs::write(
        &file,
        r#"{"source": {"blueprint": {"name": "a"}}, "input": {}}"#,
    )
    .unwrap();
    let err = run_request(RunLine {
        request_file: Some(&file),
        ..line(&manifest, dir.path())
    })
    .unwrap_err();
    assert!(err.to_string().contains("not both"), "{err}");
    let err = run_request(RunLine {
        request_file: Some(&file),
        ..RunLine::new(None, "/w", dir.path())
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("unknown field `input`"), "{err}");
    assert!(err.contains("lev schema spawn-request"), "{err}");
    let err = read_request_file(&dir.path().join("gone.toml")).unwrap_err();
    assert!(
        err.to_string().contains("could not read --request"),
        "{err}"
    );
    std::fs::write(dir.path().join("bad.toml"), "source = 3").unwrap();
    assert!(read_request_file(&dir.path().join("bad.toml")).is_err());
}

/// One issue reads as one problem; several say how many.
#[test]
fn an_issues_report_counts_its_problems() {
    let one = SpawnIssues(vec![SpawnIssue::new(
        SpecPath::root().field("model"),
        IssueCode::Invalid,
        "no",
    )]);
    assert_eq!(
        issues_report(&one),
        "1 problem with this run:\n  model: invalid: no"
    );
}
