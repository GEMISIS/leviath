use leviath_core::JsonDoc;
use leviath_runtime::spec::inputs::InputValues;
use leviath_runtime::spec::names::ToolName;

use super::*;
use crate::daemon::resolve_env::tests::env;

/// Run `seed` in `workdir` with the given task and command switch.
fn seed_in(
    env: &DaemonEnv,
    workdir: &Path,
    seed: &Seed,
    task: Option<&str>,
    commands: bool,
) -> Result<String, String> {
    let code = CodeFiles::new();
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
    run(
        env,
        seed,
        SeedCx {
            workdir,
            commands_allowed: commands,
            code: &code,
            inputs: &inputs,
        },
    )
}

#[tokio::test]
async fn the_env_hands_back_what_a_seed_produced_as_region_content() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let code = CodeFiles::new();
    let inputs = InputValues::default();
    let cx = SeedCx {
        workdir: dir.path(),
        commands_allowed: false,
        code: &code,
        inputs: &inputs,
    };
    let content = env.seed(&Seed::Literal("hello".into()), cx).await.unwrap();
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
    let go = |seed: Seed| seed_in(&env, &work, &seed, None, false);

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

#[test]
fn code_seeds_run_with_the_task_and_workdir() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let script = Seed::Code(CodeRef::Inline("input.task + \"!\"".into()));
    assert_eq!(
        seed_in(&env, dir.path(), &script, Some("go"), false).unwrap(),
        "go!"
    );
    assert_eq!(
        seed_in(&env, dir.path(), &script, None, false).unwrap(),
        "!"
    );
    let failing = Seed::Code(CodeRef::Inline("throw \"no\"".into()));
    let err = seed_in(&env, dir.path(), &failing, None, false).unwrap_err();
    assert!(err.starts_with("code seed failed"), "{err}");
    let by_file = Seed::Code(CodeRef::File("seed.rhai".into()));
    let err = seed_in(&env, dir.path(), &by_file, None, false).unwrap_err();
    assert!(err.contains("runs the code it carries"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn command_seeds_run_only_when_allowed_and_pre_approved() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let echo = Seed::Command("echo seeded".into());
    let off = seed_in(&env, dir.path(), &echo, None, false).unwrap_err();
    assert!(off.contains("command seeds are disabled"), "{off}");
    let out = seed_in(&env, dir.path(), &echo, None, true).unwrap();
    assert!(out.contains("seeded"), "{out}");
    let unapproved = Seed::Command("curl https://example.com".into());
    assert!(seed_in(&env, dir.path(), &unapproved, None, true).is_err());
}

fn call(tool: &str, args: serde_json::Value) -> SeedToolCall {
    SeedToolCall {
        tool: ToolName::new(tool).unwrap(),
        args: JsonDoc::new(args),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_seeds_answer_to_tool_permissions_call_by_call() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "remember this").unwrap();
    std::fs::write(dir.path().join("empty.txt"), "").unwrap();
    let tools = |calls: Vec<SeedToolCall>| {
        seed_in(
            &env,
            dir.path(),
            &Seed::Tools {
                calls,
                refresh: Default::default(),
            },
            None,
            false,
        )
    };
    let read = call("read_file", serde_json::json!({"path": "notes.txt"}));
    let empty = call("read_file", serde_json::json!({"path": "empty.txt"}));
    let shell = call("shell", serde_json::json!({"command": "ls"}));
    let out = tools(vec![read.clone(), empty.clone(), shell.clone()]).unwrap();
    assert!(
        out.contains("--- read_file ---") && out.contains("remember this"),
        "{out}"
    );
    assert_eq!(tools(vec![empty]).unwrap(), "");
    let refused = tools(vec![shell]).unwrap_err();
    assert!(refused.starts_with("shell: "), "{refused}");
}
