use leviath_core::JsonDoc;
use leviath_core::policy::ToolPolicy;
use leviath_runtime::spec::inputs::InputValues;
use leviath_runtime::spec::launch::LaunchPolicy;
use leviath_runtime::spec::names::ToolName;

use super::run as run_seed;
use super::*;
use crate::daemon::resolve_env::tests::{env, env_with, load_installed_graph};

/// The run a seed belongs to: the test manifest's graph, attended.
struct Run {
    run_id: RunId,
    graph: RunGraph,
    launch: LaunchPolicy,
    code: CodeFiles,
    refs: Vec<(CodeRef, Digest)>,
    inputs: InputValues,
}

impl Run {
    fn new(task: Option<&str>) -> Self {
        let inputs = match task {
            Some(t) => InputValues(
                [(
                    leviath_runtime::spec::names::InputName::new("task").unwrap(),
                    InputValue::Text(t.to_string()),
                )]
                .into(),
            ),
            None => InputValues::default(),
        };
        Self {
            run_id: RunId::new("seed-run-1").unwrap(),
            graph: load_installed_graph(),
            launch: LaunchPolicy {
                unattended: Unattended::Off,
                allow: vec![],
                max_depth: 1,
                seed_commands: true,
                capture_model_input: false,
            },
            code: CodeFiles::new(),
            refs: Vec::new(),
            inputs,
        }
    }

    /// Hold `text` as the run's copy of `code`.
    fn holding(mut self, code: CodeRef, text: &str) -> Self {
        let digest = Digest::of(text.as_bytes());
        self.code.insert(digest.clone(), text.as_bytes().to_vec());
        self.refs.push((code, digest));
        self
    }

    fn cx<'a>(&'a self, workdir: &'a Path, commands: bool) -> SeedCx<'a> {
        SeedCx {
            run_id: &self.run_id,
            agent: "helper",
            graph: &self.graph,
            launch: &self.launch,
            workdir,
            blueprint_dir: None,
            commands_allowed: commands,
            code: &self.code,
            code_refs: &self.refs,
            inputs: &self.inputs,
        }
    }
}

/// Run `seed` in `workdir` for `run`.
fn seed_in(
    env: &DaemonEnv,
    run: &Run,
    workdir: &Path,
    seed: &Seed,
    commands: bool,
) -> Result<String, String> {
    run_seed(env, seed, run.cx(workdir, commands))
}

#[tokio::test]
async fn the_env_hands_back_what_a_seed_produced_as_region_content() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let run = Run::new(None);
    let content = env
        .seed(&Seed::Literal("hello".into()), run.cx(dir.path(), false))
        .await
        .unwrap();
    assert_eq!(content.text, "hello");
    assert!(content.parts.is_empty());
}

#[test]
fn literal_files_and_glob_seeds_read_inside_the_workdir_only() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join("docs")).unwrap();
    std::fs::write(work.join("docs/a.md"), "alpha").unwrap();
    std::fs::write(work.join("docs/b.md"), "beta").unwrap();
    std::fs::write(dir.path().join("secret.md"), "s").unwrap();
    let run = Run::new(None);
    let go = |seed: Seed| seed_in(&env, &run, &work, &seed, false);

    assert_eq!(go(Seed::Literal("hi".into())).unwrap(), "hi");
    let files = go(Seed::Files(vec![WorkdirPath::new("docs/a.md").unwrap()])).unwrap();
    assert!(files.contains("--- ") && files.contains("alpha"), "{files}");
    let missing = go(Seed::Files(vec![WorkdirPath::new("gone.md").unwrap()])).unwrap_err();
    assert!(missing.contains("gone.md"), "{missing}");

    let globbed = go(Seed::Glob("docs/*.md".into())).unwrap();
    assert!(
        globbed.contains("alpha") && globbed.contains("beta"),
        "{globbed}"
    );
    assert_eq!(go(Seed::Glob("none/*.md".into())).unwrap(), "");
    let outside = go(Seed::Glob("../*.md".into())).unwrap_err();
    assert!(
        outside.contains("outside the working directory"),
        "{outside}"
    );
    let bad = go(Seed::Glob("[".into())).unwrap_err();
    assert!(bad.contains("bad glob"), "{bad}");
}

/// A `blueprint:` path reads from the blueprint's own directory and never
/// leaves it; a graph its caller wrote has no directory to read from.
#[test]
fn a_blueprint_path_reads_the_files_the_blueprint_ships() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let shipped = dir.path().join("blueprint");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(shipped.join("rubrics")).unwrap();
    std::fs::write(shipped.join("style.md"), "house style").unwrap();
    std::fs::write(shipped.join("rubrics/a.md"), "rubric a").unwrap();
    std::fs::write(dir.path().join("secret.md"), "s").unwrap();
    let run = Run::new(None);
    let go = |seed: Seed, from: Option<&Path>| {
        let cx = SeedCx {
            blueprint_dir: from,
            ..run.cx(&work, false)
        };
        run_seed(&env, &seed, cx)
    };
    let style = Seed::Files(vec![WorkdirPath::new("blueprint:style.md").unwrap()]);
    let read = go(style.clone(), Some(&shipped)).unwrap();
    assert!(read.contains("house style"), "{read}");
    let rubric = go(Seed::Glob("blueprint:rubrics/*.md".into()), Some(&shipped)).unwrap();
    assert!(rubric.contains("rubric a"), "{rubric}");
    let escaped = go(Seed::Glob("blueprint:../*.md".into()), Some(&shipped)).unwrap_err();
    assert!(escaped.contains("blueprint's directory"), "{escaped}");
    let nowhere = go(style, None).unwrap_err();
    assert!(nowhere.contains("has none"), "{nowhere}");
    let nowhere = go(Seed::Glob("blueprint:rubrics/*.md".into()), None).unwrap_err();
    assert!(nowhere.contains("has none"), "{nowhere}");
}

/// A workdir path may leave the workdir where the run's `[read_paths]` are
/// granted, and only there; an entry that will not compile fails the seed.
#[test]
fn a_seed_reads_outside_the_workdir_where_read_paths_are_granted() {
    let mut config = Config::default();
    config.security.allow_blueprint_read_paths = true;
    let (env, _agents) = env_with(config);
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(dir.path().join("shared")).unwrap();
    std::fs::write(dir.path().join("shared/notes.md"), "shared notes").unwrap();
    std::fs::write(dir.path().join("secret.md"), "s").unwrap();
    let mut run = Run::new(None);
    let shared = dir.path().join("shared").to_string_lossy().into_owned();
    run.graph.read_paths = vec![shared];
    let go = |seed: Seed| seed_in(&env, &run, &work, &seed, false);
    let notes = go(Seed::Glob("../shared/*.md".into())).unwrap();
    assert!(notes.contains("shared notes"), "{notes}");
    let refused = go(Seed::Glob("../*.md".into())).unwrap_err();
    assert!(refused.contains("no [read_paths] grant"), "{refused}");

    let mut broken = Run::new(None);
    broken.graph.read_paths = vec!["regex:relative".to_string()];
    let err = seed_in(&env, &broken, &work, &Seed::Glob("*.md".into()), false).unwrap_err();
    assert!(err.contains("[read_paths]"), "{err}");
    let files = Seed::Files(vec![WorkdirPath::new("a.md").unwrap()]);
    let err = seed_in(&env, &broken, &work, &files, false).unwrap_err();
    assert!(err.contains("[read_paths]"), "{err}");
}

#[test]
fn code_seeds_run_the_runs_own_copy_with_the_task_and_workdir() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let inline = CodeRef::Inline("input.task + \"!\"".into());
    let by_file = CodeRef::File("seeds/shout.rhai".into());
    let run = Run::new(Some("go"))
        .holding(inline.clone(), "input.task + \"!\"")
        .holding(by_file.clone(), "input.task + \"?\"");
    let go = |seed: Seed, run: &Run| seed_in(&env, run, dir.path(), &seed, false);
    assert_eq!(go(Seed::Code(inline.clone()), &run).unwrap(), "go!");
    assert_eq!(go(Seed::Code(by_file), &run).unwrap(), "go?");
    let taskless = Run::new(None).holding(inline.clone(), "input.task + \"!\"");
    assert_eq!(go(Seed::Code(inline), &taskless).unwrap(), "!");

    let failing = CodeRef::Inline("throw \"no\"".into());
    let run = Run::new(None).holding(failing.clone(), "throw \"no\"");
    let err = go(Seed::Code(failing), &run).unwrap_err();
    assert!(err.starts_with("code seed failed"), "{err}");
    let unheld = go(Seed::Code(CodeRef::File("gone.rhai".into())), &run).unwrap_err();
    assert!(unheld.contains("holds no code"), "{unheld}");
    let binary = CodeRef::File("bin.rhai".into());
    let mut run = Run::new(None);
    let digest = Digest::of(&[0xff]);
    run.code.insert(digest.clone(), vec![0xff]);
    run.refs.push((binary.clone(), digest));
    let err = go(Seed::Code(binary), &run).unwrap_err();
    assert!(err.contains("not UTF-8"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn command_seeds_run_only_when_allowed_and_pre_approved() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let run = Run::new(None);
    let echo = Seed::Command("echo seeded".into());
    let off = seed_in(&env, &run, dir.path(), &echo, false).unwrap_err();
    assert!(off.contains("command seeds are disabled"), "{off}");
    let out = seed_in(&env, &run, dir.path(), &echo, true).unwrap();
    assert!(out.contains("seeded"), "{out}");
    let unapproved = Seed::Command("curl https://example.com".into());
    assert!(seed_in(&env, &run, dir.path(), &unapproved, true).is_err());
}

/// A sandbox no machine can build, which fails where it is built.
fn unbuildable() -> leviath_runtime::spec::graph::SandboxDef {
    leviath_runtime::spec::graph::SandboxDef {
        kind: leviath_core::sandbox::SandboxKind::Container,
        image: None,
        engine: Some("docker".into()),
        network: true,
        mounts: vec![],
        keep_warm: false,
        on_unavailable: leviath_core::sandbox::OnUnavailable::Error,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn command_and_tool_seeds_run_inside_the_entry_stages_sandbox() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let mut run = Run::new(None);
    run.graph.sandbox = Some(unbuildable());
    let echo = Seed::Command("echo seeded".into());
    assert!(seed_in(&env, &run, dir.path(), &echo, true).is_err());
    let tools = Seed::Tools {
        calls: vec![],
        refresh: Default::default(),
    };
    assert!(seed_in(&env, &run, dir.path(), &tools, false).is_err());

    // A sandbox that falls back with a warning still carries the calls.
    std::fs::write(dir.path().join("notes.txt"), "inside").unwrap();
    run.graph.sandbox = Some(leviath_runtime::spec::graph::SandboxDef {
        kind: leviath_core::sandbox::SandboxKind::Namespace,
        on_unavailable: leviath_core::sandbox::OnUnavailable::Warn,
        ..unbuildable()
    });
    let read = Seed::Tools {
        calls: vec![call("read_file", serde_json::json!({"path": "notes.txt"}))],
        refresh: Default::default(),
    };
    let out = seed_in(&env, &run, dir.path(), &read, false).unwrap();
    assert!(out.contains("inside"), "{out}");
}

fn call(tool: &str, args: serde_json::Value) -> SeedToolCall {
    SeedToolCall {
        tool: ToolName::new(tool).unwrap(),
        args: JsonDoc::new(args),
    }
}

fn tools(calls: Vec<SeedToolCall>) -> Seed {
    Seed::Tools {
        calls,
        refresh: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_seeds_answer_to_tool_permissions_call_by_call() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "remember this").unwrap();
    std::fs::write(dir.path().join("empty.txt"), "").unwrap();
    let run = Run::new(None);
    let go = |calls: Vec<SeedToolCall>| seed_in(&env, &run, dir.path(), &tools(calls), false);
    let read = call("read_file", serde_json::json!({"path": "notes.txt"}));
    let empty = call("read_file", serde_json::json!({"path": "empty.txt"}));
    let shell = call("shell", serde_json::json!({"command": "ls"}));
    let out = go(vec![read.clone(), empty.clone(), shell.clone()]).unwrap();
    assert!(
        out.contains("--- read_file ---") && out.contains("remember this"),
        "{out}"
    );
    assert_eq!(go(vec![empty]).unwrap(), "");
    let refused = go(vec![shell]).unwrap_err();
    assert!(refused.starts_with("shell: "), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_seeds_answer_to_the_runs_allow_list_and_its_yolo_profile() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let write = call(
        "write_file",
        serde_json::json!({"path": "out.txt", "content": "seeded"}),
    );
    let attended = Run::new(None);
    let refused = seed_in(
        &env,
        &attended,
        dir.path(),
        &tools(vec![write.clone()]),
        false,
    );
    assert!(refused.is_err(), "write_file asks, and a seed cannot ask");

    let mut allowed = Run::new(None);
    allowed.launch.allow = vec![ToolName::new("write_file").unwrap()];
    seed_in(
        &env,
        &allowed,
        dir.path(),
        &tools(vec![write.clone()]),
        false,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "seeded"
    );

    let mut unattended = Run::new(None);
    unattended.launch.unattended = Unattended::All;
    let rewrite = call(
        "write_file",
        serde_json::json!({"path": "again.txt", "content": "yolo"}),
    );
    seed_in(&env, &unattended, dir.path(), &tools(vec![rewrite]), false).unwrap();
    assert!(dir.path().join("again.txt").exists());

    let home = tempfile::tempdir().unwrap();
    let mut unknown = Run::new(None);
    unknown.launch.unattended =
        Unattended::Profile(leviath_runtime::spec::names::ProfileName::new("ghost").unwrap());
    let err = temp_env::with_vars(
        [
            ("LEVIATH_HOME", Some(home.path().as_os_str())),
            ("LEVIATH_CONFIG_PATH", None),
        ],
        || seed_in(&env, &unknown, dir.path(), &tools(vec![write]), false),
    )
    .unwrap_err();
    assert!(err.contains("ghost"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_seed_can_call_the_runs_own_script_tools() {
    let mut config = Config::default();
    config.security.allow_blueprint_permissions = true;
    let (env, _agents) = env_with(config);
    let dir = tempfile::tempdir().unwrap();
    let mine = CodeRef::File("tools/hello.rhai".into());
    let mut run = Run::new(None).holding(mine, "// @tool hello\n\"hi from a script\"");
    run.graph.tool_permissions = [(ToolName::new("hello").unwrap(), ToolPolicy::Allow)].into();
    let out = seed_in(
        &env,
        &run,
        dir.path(),
        &tools(vec![call("hello", serde_json::json!({}))]),
        false,
    )
    .unwrap();
    assert!(out.contains("hi from a script"), "{out}");
}
