//! `lev rage` against a fake install: a temp root holding a config with
//! secrets in every place one can hide, a run family with a journal, blobs
//! and a blueprint, drop-in scripts, logs and the two auth stores. The
//! invariant test builds the bundle and proves none of the planted secrets
//! survived; the rest walk the flags, the screen and the edges.

use std::io::Read;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use leviath_core::run_archive::{
    InferenceRequestRecord, InferenceResponseRecord, MessageRecord, RunIdentity, RunRecord,
    ToolCallRecord, read_archive, write_archive_start, write_record,
};
use leviath_core::run_meta::{RunMeta, RunStatus};

use super::collect::{
    self, Bundle, Selection, family, installed_blueprints, list_metas, resolve_run_id,
};
use super::state::{Rage, Step};
use super::*;
use crate::commands::setup::import::Roots;
use crate::tui::{TestEventSource, TestSetup, key, key_with, test_terminal};

// ─── Planted secrets ─────────────────────────────────────────────────────────

const CONFIG_KEY: &str = "sk-ant-PLANTED-config-key-0000000000000000";
const HEADER_VALUE: &str = "planted-header-token-1234567890";
const MCP_ENV_VALUE: &str = "planted-mcp-env-secret-abcdef";
const GATEWAY_KEY: &str = "planted-gateway-key-qwerty";
const EXTRA_VALUE: &str = "planted-extra-secret-xyz123";
const ENV_SECRET: &str = "planted-env-secret-value-999";
const MCP_AUTH_TOKEN: &str = "planted-mcp-access-token-7777";
const PROVIDER_AUTH_TOKEN: &str = "planted-provider-refresh-token-8888";
const CALLBACK_SECRET: &str = "planted-callback-secret-5555";
const AWS_KEY: &str = "AKIAPLANTEDAWSKEY123";
const GITHUB_TOKEN: &str = "ghp_PLANTEDgithubtoken0123456789abcd";
const SLACK_TOKEN: &str = "xoxb-PLANTED-slack-token-12345";
const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJwbGFudGVkIn0.PLANTEDsignature123";
const CONTROL_TOKEN: &str = "planted-live-control-token-3333";

const ALL_SECRETS: &[&str] = &[
    CONFIG_KEY,
    HEADER_VALUE,
    MCP_ENV_VALUE,
    GATEWAY_KEY,
    EXTRA_VALUE,
    ENV_SECRET,
    MCP_AUTH_TOKEN,
    PROVIDER_AUTH_TOKEN,
    CALLBACK_SECRET,
    AWS_KEY,
    GITHUB_TOKEN,
    SLACK_TOKEN,
    JWT,
    CONTROL_TOKEN,
];

const ROOT_RUN: &str = "run-root-000001";
const CHILD_RUN: &str = "run-child-00002";
const LISTED_CHILD: &str = "run-listed-0003";
const OTHER_RUN: &str = "run-other-00004";

// ─── The fake install ────────────────────────────────────────────────────────

/// Run `f` with the config path, data root and runs directory redirected
/// into one fresh temp root, and every provider key cleared. One `temp_env`
/// call, never nested: it serializes process-wide and holds its lock across
/// the future.
async fn with_env<R, Fut>(f: impl FnOnce(PathBuf) -> Fut) -> R
where
    Fut: std::future::Future<Output = R>,
{
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir.path().to_path_buf();
    let mut vars = crate::config::config_isolation_vars(&root);
    vars.push(("LEVIATH_HOME", Some(root.clone().into_os_string())));
    vars.push(("LEVIATH_RUNS_DIR", Some(root.join("runs").into_os_string())));
    vars.push(("BRAVE_API_KEY", None));
    temp_env::async_with_vars(vars, f(root)).await
}

fn env_for(root: &Path) -> RageEnv {
    let data = root.join(".leviath");
    RageEnv {
        data_dir: data.clone(),
        config_path: root.join("config.toml"),
        runs_dir: root.join("runs"),
        agents_dir: data.join("agents"),
        policy_dir: root.join("policy"),
        dashboard_log: data.join("dashboard.log"),
        cwd: root.join("out"),
        import_roots: Roots::new(root.to_path_buf(), root.join("os-config"), root.join("cwd")),
        env_lookup: Box::new(|name| match name {
            "PLANTED_API_KEY" => Some(ENV_SECRET.to_string()),
            "LEVIATH_HOME" => Some("/x".to_string()),
            "OLLAMA_BASE_URL" => Some("http://localhost:11434".to_string()),
            _ => None,
        }),
        env_names: Box::new(|| {
            [
                "PLANTED_API_KEY",
                "LEVIATH_HOME",
                "OLLAMA_BASE_URL",
                "PATH",
                "UNSET_TOKEN",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect()
        }),
        daemon: Box::new(|| DaemonSnapshot {
            running: false,
            cli_build: "test".to_string(),
            note: Some("a test never has a daemon".to_string()),
            ..DaemonSnapshot::default()
        }),
        install: Box::new(|| "a test install".to_string()),
        now: Box::new(chrono::Local::now),
    }
}

fn write(path: &Path, contents: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(path, contents).expect("write");
}

/// A manifest that parses and validates: a bundled one, which the bundled
/// tests hold to exactly that.
fn manifest_text() -> String {
    crate::bundled::BUNDLED_AGENTS
        .iter()
        .flat_map(|agent| agent.files.iter())
        .find(|(rel, _)| *rel == "agent.leviath")
        .map(|(_, content)| (*content).to_string())
        .expect("a bundled agent ships a manifest")
}

fn meta(id: &str, blueprint: &Path) -> RunMeta {
    let mut meta = RunMeta::new(
        id.to_string(),
        "coder".to_string(),
        blueprint.display().to_string(),
        "fix the planted bug".to_string(),
        Some("anthropic/m".to_string()),
        "/tmp/work".to_string(),
        3,
    );
    meta.status = RunStatus::Error;
    meta.error = Some("the tool failed".to_string());
    meta.callback_secret = Some(CALLBACK_SECRET.to_string());
    meta
}

fn write_meta(runs: &Path, meta: &RunMeta) {
    write(
        &runs.join(&meta.run_id).join("meta.json"),
        serde_json::to_string_pretty(meta).unwrap(),
    );
}

fn archive(records: &[RunRecord], extra_frame: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    write_archive_start(&mut out, 1).unwrap();
    for record in records {
        write_record(&mut out, record).unwrap();
    }
    if let Some(frame) = extra_frame {
        out.extend_from_slice(&(frame.len() as u64).to_be_bytes());
        out.extend_from_slice(frame);
    }
    out
}

/// Plant the whole install under `root`, and hand back the blueprint dir.
fn plant(root: &Path) -> PathBuf {
    let data = root.join(".leviath");
    write(
        &root.join("config.toml"),
        format!(
            r#"agent_paths = []
default_provider = "anthropic"

[providers]
anthropic_api_key = "{CONFIG_KEY}"
[providers.anthropic_headers]
x-planted = "{HEADER_VALUE}"

[[mcp_servers]]
name = "planted"
command = "echo"
[mcp_servers.env]
PLANTED_TOKEN = "{MCP_ENV_VALUE}"

[model_providers.gw]
kind = "openai-compatible"
base_url = "http://localhost:9"
api_key = "{GATEWAY_KEY}"

[model_providers.scripted]
api_token = "{EXTRA_VALUE}"
"#
        ),
    );
    write(
        &root.join("yolo.toml"),
        "[profiles.fast]\nallow = [\"read_file\"]\n",
    );
    write(&root.join("policy").join("policy.toml"), "[rules]\n");
    write(
        &root.join("policy").join("rules").join("r.toml"),
        "name = \"r\"\n",
    );
    write(&data.join("ui-state.json"), "not json at all");
    // The key provider checks are fingerprinted with. A report carries the
    // capability cache, so it must never carry this beside it.
    write(&data.join("provider-check.key"), "00".repeat(32));
    write(&data.join("control.token"), CONTROL_TOKEN);
    write(
        &data.join("mcp-auth.json"),
        format!(
            r#"{{"servers":{{"s":{{"access_token":"{MCP_AUTH_TOKEN}","resource":"https://x"}}}}}}"#
        ),
    );
    write(
        &data.join("provider-auth.json"),
        format!(r#"{{"providers":{{"codex":{{"refresh_token":"{PROVIDER_AUTH_TOKEN}"}}}}}}"#),
    );
    write(
        &data.join("daemon.log"),
        format!("info daemon up\nkey {CONFIG_KEY} leaked\n{SLACK_TOKEN}\n"),
    );
    write(&data.join("daemon.log.1"), "older\n");
    write(
        &data.join("serve-3000.log"),
        format!("serve up {SLACK_TOKEN}\n"),
    );
    write(&data.join("serve-3000.log.1"), "older serve\n");
    write(&data.join("serve-notes.txt"), "not a log");
    write(&data.join("dashboard.log"), "2026-09-16 opened\n");

    // Installed blueprints, with a tree deeper than the bundle copies.
    let demo = data.join("agents").join("demo");
    write(&demo.join("agent.leviath"), manifest_text());
    write(
        &demo.join("tools").join("t.rhai"),
        format!("let k = \"{GITHUB_TOKEN}\";\n"),
    );
    write(
        &demo
            .join("tools")
            .join("deep")
            .join("deeper")
            .join("deepest")
            .join("x.rhai"),
        "1",
    );
    write(&demo.join("image.png"), [0u8, 1, 2]);
    write(
        &data.join("tools").join("planted.rhai"),
        format!("// {GITHUB_TOKEN}\n"),
    );

    // The blueprint the run used.
    let blueprint = root.join("blueprint");
    write(&blueprint.join("agent.leviath"), manifest_text());
    write(&blueprint.join("tools").join("helper.rhai"), "fn x() {}\n");

    // A run family: root, a child by parent_run_id, a child by the parent's
    // list, a ghost in that list, and an unrelated run.
    let runs = root.join("runs");
    let mut root_meta = meta(ROOT_RUN, &blueprint);
    // One child listed here and by its own parent id, one listed here only,
    // and one that no longer exists.
    root_meta.children = vec![
        CHILD_RUN.to_string(),
        LISTED_CHILD.to_string(),
        "run-ghost-99999".to_string(),
    ];
    write_meta(&runs, &root_meta);
    let mut child = meta(CHILD_RUN, &blueprint);
    child.parent_run_id = Some(ROOT_RUN.to_string());
    child.agent_path = root.join("missing-blueprint").display().to_string();
    child.started_at -= 10;
    write_meta(&runs, &child);
    let mut listed = meta(LISTED_CHILD, &blueprint);
    listed.started_at -= 20;
    // As the daemon records it: the manifest file, not its directory.
    listed.agent_path = blueprint.join("agent.leviath").display().to_string();
    write_meta(&runs, &listed);
    let mut other = meta(OTHER_RUN, &blueprint);
    other.started_at -= 30;
    write_meta(&runs, &other);

    let run_dir = runs.join(ROOT_RUN);
    write(&run_dir.join("stages.json"), "[]");
    write(
        &run_dir.join("context.json"),
        format!(
            r#"{{"stage_name":"analyze","total_tokens":1,"max_tokens":2,"regions":[{{"name":"task","kind":"pinned","current_tokens":1,"max_tokens":2,"entries":[{{"content":"token {JWT} here","tokens":1,"kind":"user_message"}}]}}]}}"#
        ),
    );
    write(&run_dir.join("final_output"), "done");
    write(
        &run_dir.join("stages").join("0").join("output.log"),
        format!("out {ENV_SECRET}\n"),
    );
    write(
        &run_dir.join("stages").join("0").join("logs.log"),
        "log line\n",
    );
    write(&run_dir.join("stages").join("0").join("context.json"), "{}");
    write(
        &run_dir.join("stages").join("0").join("taint_audit.json"),
        "[]",
    );
    std::fs::create_dir_all(run_dir.join("stages").join("1")).unwrap();
    let records = [
        RunRecord::Header {
            identity: RunIdentity {
                run_id: ROOT_RUN.to_string(),
                machine_id: "m".to_string(),
                world_id: "w".to_string(),
                created_at: 0,
            },
            meta: Box::new(root_meta.clone()),
        },
        RunRecord::Inference {
            stage: "analyze".to_string(),
            iteration: 0,
            request: InferenceRequestRecord {
                model: "m".to_string(),
                system: vec![format!("system with {CONFIG_KEY}")],
                messages: vec![MessageRecord {
                    role: "user".to_string(),
                    content: "fix the planted bug".to_string(),
                }],
                tool_names: vec!["bash".to_string()],
                temperature: 0.0,
                max_tokens: 1,
            },
            response: InferenceResponseRecord {
                content: "running a tool".to_string(),
                tool_calls: vec![],
                prompt_tokens: 1,
                completion_tokens: 1,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            at: 1,
        },
        RunRecord::ToolBatch {
            calls: vec![ToolCallRecord {
                execution_id: String::new(),
                id: "c1".to_string(),
                name: "bash".to_string(),
                arguments: format!(r#"{{"cmd":"echo {ENV_SECRET}"}}"#),
                result: Some(format!("AWS_ACCESS_KEY_ID={AWS_KEY}").into()),
                thought_signature: None,
            }],
            at: 2,
            stage_index: 0,
            iteration: 0,
            visit_id: String::new(),
            requested_by: String::new(),
            response: String::new(),
        },
    ];
    write(
        &run_dir.join("run.lvr"),
        archive(&records, Some(br#"{"NoSuchRecord":{"at":1}}"#)),
    );
    write(&run_dir.join("blobs").join("aa11"), [0u8, 159, 146, 150]);
    write(&run_dir.join("blobs").join("bb22"), b"small text blob");
    // The child's journal is corrupt, and it has nothing else; the listed
    // child's is clean and small.
    write(&runs.join(CHILD_RUN).join("run.lvr"), b"not an archive");
    write(
        &runs.join(LISTED_CHILD).join("run.lvr"),
        archive(&[records[0].clone()], None),
    );
    // A blueprint file too large to be one, left out by size.
    write(&demo.join("NOTES.md"), vec![b'x'; 300 * 1024]);
    blueprint
}

fn selection(about: About) -> Selection {
    Selection {
        about,
        run_id: (about == About::Run).then(|| ROOT_RUN.to_string()),
        agent: None,
        note: "it broke".to_string(),
        include_blobs: true,
    }
}

/// Every member of the zip at `path`, as (name, bytes).
fn unzip(path: &Path) -> Vec<(String, Vec<u8>)> {
    let file = std::fs::File::open(path).expect("the zip exists");
    let mut zip = zip::ZipArchive::new(file).expect("a zip");
    (0..zip.len())
        .map(|i| {
            let mut entry = zip.by_index(i).expect("an entry");
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).expect("readable");
            (entry.name().to_string(), bytes)
        })
        .collect()
}

fn member<'a>(members: &'a [(String, Vec<u8>)], suffix: &str) -> &'a [u8] {
    members
        .iter()
        .find(|(name, _)| name.ends_with(suffix))
        .map(|(_, bytes)| bytes.as_slice())
        .unwrap_or_else(|| panic!("no member ending in {suffix}: {:?}", names(members)))
}

fn names(members: &[(String, Vec<u8>)]) -> Vec<&str> {
    members.iter().map(|(name, _)| name.as_str()).collect()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

// ─── The invariant ───────────────────────────────────────────────────────────

#[tokio::test]
async fn no_planted_secret_survives_and_no_credential_file_is_copied() {
    with_env(|root| async move {
        plant(&root);
        let env = env_for(&root);
        let out = root.join("bundle.zip");
        let outcome = build(&env, &selection(About::Run), Some(&out))
            .await
            .unwrap();
        assert_eq!(outcome.zip_path, out);
        assert!(outcome.redactions > 0);
        let members = unzip(&out);
        let member_names = names(&members);

        // The message names the secret by its index, not its value, so a
        // failure never prints one and a code scanner never sees one logged.
        for (name, bytes) in &members {
            for (index, secret) in ALL_SECRETS.iter().enumerate() {
                assert!(
                    !contains(bytes, secret),
                    "planted secret #{index} survived in {name}"
                );
            }
        }
        for forbidden in [
            "control.token",
            "mcp-auth.json",
            "provider-auth.json",
            "provider-check.key",
        ] {
            assert!(
                !member_names.iter().any(|n| n.ends_with(forbidden)),
                "{forbidden} must never be copied: {member_names:?}"
            );
        }
        for expected in [
            "README.md",
            "manifest.json",
            "environment.json",
            "doctor.json",
            "daemon.json",
            "config/config.toml",
            "config/yolo.toml",
            "config/policy.toml",
            "config/rules/r.toml",
            "config/ui-state.json",
            "agents/demo/agent.leviath",
            "agents/demo/tools/t.rhai",
            "tools/planted.rhai",
            "logs/daemon.log",
            "logs/daemon.log.1",
            "logs/dashboard.log",
            "logs/serve-3000.log",
            "logs/serve-3000.log.1",
            &format!("runs/{ROOT_RUN}/meta.json"),
            &format!("runs/{ROOT_RUN}/context.json"),
            &format!("runs/{ROOT_RUN}/final_output"),
            &format!("runs/{ROOT_RUN}/stages/0/output.log"),
            &format!("runs/{ROOT_RUN}/stages/0/taint_audit.json"),
            &format!("runs/{ROOT_RUN}/run.lvr"),
            &format!("runs/{ROOT_RUN}/blobs/aa11"),
            &format!("runs/{ROOT_RUN}/blueprint/agent.leviath"),
            &format!("runs/{ROOT_RUN}/blueprint/tools/helper.rhai"),
            &format!("runs/{CHILD_RUN}/meta.json"),
            &format!("runs/{LISTED_CHILD}/meta.json"),
            &format!("runs/{LISTED_CHILD}/blueprint/agent.leviath"),
        ] {
            assert!(
                member_names.iter().any(|n| n.ends_with(expected)),
                "{expected} missing from {member_names:?}"
            );
        }
        assert!(
            !member_names.iter().any(|n| n.contains(OTHER_RUN)),
            "an unrelated run is not part of the family"
        );
        assert!(
            !member_names.iter().any(|n| n.contains("serve-notes")),
            "only log files beside the serve logs are taken"
        );
        assert!(
            member_names.iter().all(|n| n.starts_with("leviath-rage-")),
            "everything sits under one top-level directory: {member_names:?}"
        );

        // The config keeps its shape and its header names.
        let config = String::from_utf8_lossy(member(&members, "config/config.toml")).into_owned();
        assert!(config.contains("x-planted = \"<redacted>\""), "{config}");
        assert!(
            config.contains("default_provider = \"anthropic\""),
            "{config}"
        );
        // The run's metadata lost its signing key and kept its task.
        let meta: serde_json::Value =
            serde_json::from_slice(member(&members, &format!("runs/{ROOT_RUN}/meta.json")))
                .unwrap();
        assert_eq!(meta["callback_secret"], serde_json::Value::Null);
        assert_eq!(meta["task"], "fix the planted bug");
        // The journal reads back with the same tools, minus the frame this
        // build could not parse.
        let (_, records) = read_archive(&mut member(&members, "run.lvr")).unwrap();
        assert_eq!(records.len(), 3);
        // The binary blob came through byte for byte.
        assert_eq!(member(&members, "blobs/aa11"), &[0u8, 159, 146, 150]);
        // The manifest accounts for what was left out.
        let manifest: serde_json::Value =
            serde_json::from_slice(member(&members, "manifest.json")).unwrap();
        let skipped = manifest["skipped"].as_array().unwrap();
        let reasons: Vec<String> = skipped
            .iter()
            .map(|s| format!("{} ({})", s["path"], s["reason"]))
            .collect();
        assert!(
            reasons
                .iter()
                .any(|r| r.contains("daemon.stdio.log") && r.contains("not present")),
            "{reasons:?}"
        );
        assert!(
            reasons
                .iter()
                .any(|r| r.contains("image.png") && r.contains("not a text")),
            "{reasons:?}"
        );
        assert!(
            reasons
                .iter()
                .any(|r| r.contains("deepest/") && r.contains("deeper")),
            "{reasons:?}"
        );
        assert!(
            reasons
                .iter()
                .any(|r| r.contains(CHILD_RUN) && r.contains("run.lvr")),
            "{reasons:?}"
        );
        assert!(
            reasons.iter().any(|r| r.contains("missing-blueprint")),
            "{reasons:?}"
        );
        assert!(
            reasons.iter().any(|r| r.contains("providers/")),
            "{reasons:?}"
        );
        assert!(
            reasons
                .iter()
                .any(|r| r.contains("NOTES.md") && r.contains("over the")),
            "{reasons:?}"
        );
        assert!(
            member_names
                .iter()
                .any(|n| n.ends_with(&format!("runs/{LISTED_CHILD}/run.lvr")))
        );
        let notes = manifest["notes"].as_array().unwrap();
        assert!(
            notes
                .iter()
                .any(|n| n.as_str().unwrap().contains("frame(s)")),
            "{notes:?}"
        );
        // The README carries the warning and the sections.
        let readme = String::from_utf8_lossy(member(&members, "README.md")).into_owned();
        assert!(readme.contains("Before you share this"), "{readme}");
        assert!(readme.contains("it broke"), "{readme}");
        assert!(readme.contains(ROOT_RUN), "{readme}");
        assert!(readme.contains("Left out, and why"), "{readme}");
        // The environment names the set variables without their values.
        let environment =
            String::from_utf8_lossy(member(&members, "environment.json")).into_owned();
        assert!(environment.contains("PLANTED_API_KEY"), "{environment}");
        assert!(environment.contains("OLLAMA_BASE_URL"), "{environment}");
        assert!(!environment.contains("\"PATH\""), "{environment}");
        assert!(environment.contains("a test install"), "{environment}");
        // The daemon snapshot went in as given.
        let daemon = String::from_utf8_lossy(member(&members, "daemon.json")).into_owned();
        assert!(daemon.contains("a test never has a daemon"), "{daemon}");
    })
    .await
}

#[tokio::test]
async fn blobs_can_be_left_out() {
    with_env(|root| async move {
        plant(&root);
        let env = env_for(&root);
        let out = root.join("bundle.zip");
        let mut sel = selection(About::Run);
        sel.include_blobs = false;
        let outcome = build(&env, &sel, Some(&out)).await.unwrap();
        let members = unzip(&out);
        assert!(!names(&members).iter().any(|n| n.contains("blobs/")));
        assert!(
            outcome
                .skipped
                .iter()
                .any(|s| s.reason.contains("--no-blobs"))
        );
    })
    .await
}

#[tokio::test]
async fn the_other_categories_carry_their_own_extras() {
    with_env(|root| async move {
        let blueprint = plant(&root);
        // A blueprint that parses and then fails to validate.
        write(
            &root.join("bad").join("agent.leviath"),
            format!(
                "{}\n[[dependencies]]\nname = \"dup\"\nkind = \"env\"\nvar = \"X\"\n[[dependencies]]\nname = \"dup\"\nkind = \"env\"\nvar = \"Y\"\n",
                manifest_text()
            ),
        );
        write(&root.join("broken").join("agent.leviath"), "not = = toml");
        let env = env_for(&root);

        let setup = collect::collect(&env, &selection(About::Setup), "now").await;
        let imports = setup.members.iter().find(|m| m.path == "setup/imports.json").unwrap();
        assert!(String::from_utf8_lossy(&imports.bytes).contains("Claude Code"));

        let mut sel = selection(About::Agent);
        sel.agent = Some(blueprint.join("agent.leviath"));
        let agent = collect::collect(&env, &sel, "now").await;
        let check = agent.members.iter().find(|m| m.path == "blueprint-check.json").unwrap();
        let check: serde_json::Value = serde_json::from_slice(&check.bytes).unwrap();
        assert_eq!(check["parses"], true);
        assert_eq!(check["validates"], true, "{check}");
        assert!(check["name"].is_string(), "{check}");
        assert!(agent.members.iter().any(|m| m.path == "blueprint/agent.leviath"));

        sel.agent = Some(root.join("bad"));
        let bad = collect::collect(&env, &sel, "now").await;
        let check = bad.members.iter().find(|m| m.path == "blueprint-check.json").unwrap();
        let check: serde_json::Value = serde_json::from_slice(&check.bytes).unwrap();
        assert_eq!(check["parses"], true, "{check}");
        assert_eq!(check["validates"], false, "{check}");
        assert!(check["validation_error"].is_string(), "{check}");

        sel.agent = Some(root.join("broken"));
        let broken = collect::collect(&env, &sel, "now").await;
        let check = broken.members.iter().find(|m| m.path == "blueprint-check.json").unwrap();
        let check: serde_json::Value = serde_json::from_slice(&check.bytes).unwrap();
        assert_eq!(check["parses"], false);

        sel.agent = Some(root.join("nowhere"));
        let nowhere = collect::collect(&env, &sel, "now").await;
        let check = nowhere.members.iter().find(|m| m.path == "blueprint-check.json").unwrap();
        assert!(String::from_utf8_lossy(&check.bytes).contains("cannot read"));

        // A category that needs a choice, with none: nothing extra, nothing lost.
        sel.agent = None;
        let none = collect::collect(&env, &sel, "now").await;
        assert!(!none.members.iter().any(|m| m.path.starts_with("blueprint")));
        let mut run_sel = selection(About::Run);
        run_sel.run_id = None;
        let none = collect::collect(&env, &run_sel, "now").await;
        assert!(!none.members.iter().any(|m| m.path.starts_with("runs/")));
        let other = collect::collect(&env, &selection(About::Other), "now").await;
        assert!(other.members.iter().any(|m| m.path == "README.md"));
    })
    .await
}

#[tokio::test]
async fn an_empty_install_is_reported_not_faked() {
    with_env(|root| async move {
        let env = env_for(&root);
        let mut sel = selection(About::Run);
        sel.run_id = Some("run-nowhere".to_string());
        let bundle = collect::collect(&env, &sel, "now").await;
        let skipped: Vec<&str> = bundle.skipped.iter().map(|s| s.path.as_str()).collect();
        assert!(skipped.contains(&"config/config.toml"), "{skipped:?}");
        assert!(skipped.contains(&"agents/"), "{skipped:?}");
        assert!(skipped.contains(&"runs/run-nowhere/"), "{skipped:?}");
        assert!(skipped.contains(&"logs/daemon.log"), "{skipped:?}");
        let sections = bundle.sections();
        assert!(sections.iter().any(|s| s.name == "README.md"));
    })
    .await
}

#[tokio::test]
async fn a_config_that_will_not_load_is_still_copied_and_scrubbed() {
    with_env(|root| async move {
        write(
            &root.join("config.toml"),
            format!("broken = = {CONFIG_KEY}"),
        );
        let env = env_for(&root);
        let bundle = collect::collect(&env, &selection(About::Other), "now").await;
        assert!(
            bundle.notes.iter().any(|n| n.contains("did not load")),
            "{:?}",
            bundle.notes
        );
        let config = bundle
            .members
            .iter()
            .find(|m| m.path == "config/config.toml")
            .unwrap();
        assert!(!contains(&config.bytes, CONFIG_KEY));
        assert!(config.redactions >= 1);
    })
    .await
}

#[tokio::test]
async fn an_unparseable_meta_is_copied_as_text() {
    with_env(|root| async move {
        write(
            &root.join("runs").join("r1").join("meta.json"),
            format!("{{not json {CONFIG_KEY}"),
        );
        let env = env_for(&root);
        let mut sel = selection(About::Run);
        sel.run_id = Some("r1".to_string());
        let bundle = collect::collect(&env, &sel, "now").await;
        let meta = bundle
            .members
            .iter()
            .find(|m| m.path == "runs/r1/meta.json")
            .unwrap();
        assert!(!contains(&meta.bytes, CONFIG_KEY));
        assert!(bundle.skipped.iter().any(|s| s.path == "runs/r1/run.lvr"));
    })
    .await
}

// ─── Readers ─────────────────────────────────────────────────────────────────

#[test]
fn read_capped_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    write(&file, "12345");
    assert_eq!(collect::read_capped(&file, 10).unwrap(), b"12345");
    assert!(
        collect::read_capped(&file, 4)
            .unwrap_err()
            .contains("over the 4 byte cap")
    );
    assert_eq!(
        collect::read_capped(&dir.path().join("nope"), 10).unwrap_err(),
        "not present"
    );
    assert_eq!(
        collect::describe_io(std::io::Error::from(std::io::ErrorKind::NotFound)),
        "not present"
    );
    assert!(collect::describe_io(std::io::Error::other("boom")).contains("boom"));
}

#[test]
fn tail_text_keeps_the_end_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("log");
    write(&file, "aaaa\nbbbb\ncccc\n");
    let scrubber = super::scrub::Scrubber::new(Vec::<String>::new());
    let mut bundle = Bundle::default();
    collect::tail_text(&file, "logs/x", 5, &scrubber, &mut bundle);
    collect::tail_text(&file, "logs/whole", 100, &scrubber, &mut bundle);
    collect::tail_text(
        &dir.path().join("nope"),
        "logs/nope",
        100,
        &scrubber,
        &mut bundle,
    );
    let tail = String::from_utf8_lossy(&bundle.members[0].bytes).into_owned();
    assert!(tail.starts_with("[... earlier lines left out"), "{tail}");
    assert!(tail.ends_with("cccc\n"), "{tail}");
    assert!(bundle.members[0].truncated);
    assert!(!bundle.members[1].truncated);
    assert_eq!(bundle.skipped[0].reason, "not present");
}

#[test]
fn blobs_respect_the_caps() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = dir.path().join("blobs");
    write(&blobs.join("a"), "12");
    write(&blobs.join("b"), "123456");
    write(&blobs.join("c"), "12");
    let mut bundle = Bundle::default();
    let mut budget = 3;
    collect::copy_blobs(dir.path(), "runs/r", true, 4, &mut budget, &mut bundle);
    let copied: Vec<&str> = bundle.members.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(copied, vec!["runs/r/blobs/a"]);
    assert_eq!(
        bundle.skipped.len(),
        2,
        "one over the part cap, one over the budget"
    );
    // No blobs directory: nothing to say.
    let mut none = Bundle::default();
    collect::copy_blobs(
        &dir.path().join("nowhere"),
        "runs/r",
        true,
        4,
        &mut 10,
        &mut none,
    );
    assert!(none.members.is_empty() && none.skipped.is_empty());
}

#[test]
fn family_is_the_root_and_what_it_spawned() {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path();
    let blueprint = Path::new("/nowhere");
    let mut root = meta("root", blueprint);
    root.children = vec![
        "child".to_string(),
        "listed".to_string(),
        "ghost".to_string(),
    ];
    let mut child = meta("child", blueprint);
    child.parent_run_id = Some("root".to_string());
    let mut grandchild = meta("grandchild", blueprint);
    grandchild.parent_run_id = Some("child".to_string());
    let listed = meta("listed", blueprint);
    let other = meta("other", blueprint);
    for m in [&root, &child, &grandchild, &listed, &other] {
        write_meta(runs, m);
    }
    let metas = list_metas(runs);
    let mut ids = family(&metas, runs, "root");
    assert_eq!(ids.remove(0), "root");
    ids.sort();
    assert_eq!(ids, vec!["child", "grandchild", "listed"]);
    assert_eq!(family(&metas, runs, "other"), vec!["other"]);
}

#[test]
fn run_ids_resolve_exactly_or_by_a_unique_prefix() {
    let blueprint = Path::new("/nowhere");
    let metas = vec![
        meta("abc-1", blueprint),
        meta("abd-2", blueprint),
        meta("abd-3", blueprint),
    ];
    assert_eq!(resolve_run_id(&metas, "abc-1").unwrap(), "abc-1");
    assert_eq!(resolve_run_id(&metas, "abc").unwrap(), "abc-1");
    let err = resolve_run_id(&metas, "abd").unwrap_err();
    assert!(err.contains("matches 2 runs"), "{err}");
    assert!(err.contains("abd-2") && err.contains("abd-3"), "{err}");
    assert!(
        resolve_run_id(&metas, "zzz")
            .unwrap_err()
            .contains("no run")
    );
}

#[test]
fn installed_blueprints_need_a_manifest() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("agents").join("a").join("agent.leviath"),
        "x",
    );
    std::fs::create_dir_all(dir.path().join("agents").join("empty")).unwrap();
    let found = installed_blueprints(&dir.path().join("agents"));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "a");
    assert!(installed_blueprints(&dir.path().join("nowhere")).is_empty());
}

// ─── The zip and the flags path ──────────────────────────────────────────────

#[tokio::test]
async fn a_zip_that_cannot_be_written_is_an_error() {
    with_env(|root| async move {
        let env = env_for(&root);
        let out = root.join("no-such-dir").join("bundle.zip");
        let err = build(&env, &selection(About::Other), Some(&out))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot write"), "{err}");
    })
    .await
}

#[tokio::test]
async fn the_default_output_lands_in_the_working_directory() {
    with_env(|root| async move {
        let env = env_for(&root);
        std::fs::create_dir_all(&env.cwd).unwrap();
        let outcome = build(&env, &selection(About::Other), None).await.unwrap();
        assert!(outcome.zip_path.starts_with(&env.cwd));
        assert!(
            outcome
                .zip_path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("leviath-rage-")
        );
        assert!(outcome.zip_path.exists());
        assert_eq!(
            outcome.zip_bytes,
            std::fs::metadata(&outcome.zip_path).unwrap().len()
        );
    })
    .await
}

#[tokio::test]
async fn the_flags_decide_the_selection() {
    with_env(|root| async move {
        plant(&root);
        let env = env_for(&root);
        let args = RageArgs::default();
        assert_eq!(
            selection_from_args(&args, &env).unwrap().about,
            About::Other
        );

        let args = RageArgs {
            run: Some("run-root".to_string()),
            ..Default::default()
        };
        let sel = selection_from_args(&args, &env).unwrap();
        assert_eq!(sel.about, About::Run);
        assert_eq!(sel.run_id.as_deref(), Some(ROOT_RUN));

        let args = RageArgs {
            agent: Some(root.join("blueprint")),
            no_blobs: true,
            note: Some("a note".to_string()),
            ..Default::default()
        };
        let sel = selection_from_args(&args, &env).unwrap();
        assert_eq!(sel.about, About::Agent);
        assert!(!sel.include_blobs);
        assert_eq!(sel.note, "a note");

        let args = RageArgs {
            about: Some(About::Run),
            ..Default::default()
        };
        let err = selection_from_args(&args, &env).unwrap_err();
        assert!(err.to_string().contains("--run"), "{err}");

        let args = RageArgs {
            run: Some("zzz".to_string()),
            ..Default::default()
        };
        assert!(selection_from_args(&args, &env).is_err());

        let args = RageArgs {
            about: Some(About::Agent),
            ..Default::default()
        };
        let err = selection_from_args(&args, &env).unwrap_err();
        assert!(err.to_string().contains("--agent"), "{err}");
    })
    .await
}

#[tokio::test]
async fn non_interactive_and_a_pipe_both_take_the_flags_path() {
    with_env(|root| async move {
        plant(&root);
        let env = env_for(&root);
        let mut args = RageArgs {
            output: Some(root.join("a.zip")),
            non_interactive: true,
            ..Default::default()
        };
        let mut setup = TestSetup::new();
        let mut events = TestEventSource::new(vec![]);
        execute_with(&args, &env, &mut setup, &mut events, true)
            .await
            .unwrap();
        assert!(root.join("a.zip").exists());
        assert_eq!(setup.enable_calls, 0, "no terminal is taken");

        args.output = Some(root.join("b.zip"));
        args.non_interactive = false;
        execute_with(&args, &env, &mut setup, &mut events, false)
            .await
            .unwrap();
        assert!(root.join("b.zip").exists());
        assert_eq!(setup.enable_calls, 0);
    })
    .await
}

#[test]
fn outcome_lines_say_what_was_written_and_what_was_not() {
    let outcome = Outcome {
        zip_path: PathBuf::from("/tmp/x.zip"),
        zip_bytes: 2048,
        sections: vec![Section {
            name: "runs/".to_string(),
            files: 3,
            bytes: 1_500_000,
            redactions: 2,
        }],
        skipped: vec![SkippedEntry {
            path: "logs/daemon.log".to_string(),
            reason: "not present".to_string(),
        }],
        redactions: 2,
        notes: vec!["a note".to_string()],
    };
    let lines = outcome_lines(&outcome);
    assert!(
        lines[0].contains("/tmp/x.zip") && lines[0].contains("2 KiB"),
        "{lines:?}"
    );
    assert!(
        lines[1].contains("runs/") && lines[1].contains("1.4 MiB"),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("left out: logs/daemon.log")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("note: a note")),
        "{lines:?}"
    );
    assert!(
        lines.last().unwrap().contains("reporting-issues"),
        "{lines:?}"
    );
    assert_eq!(human_bytes(12), "12 B");
}

#[test]
fn the_real_helpers_answer_for_this_machine() {
    let dir = tempfile::tempdir().unwrap();
    let (policy, log) = temp_env::with_vars(
        [
            ("LEVIATH_HOME", Some(dir.path().as_os_str().to_owned())),
            ("LEVIATH_DASHBOARD_LOG_PATH", None),
        ],
        || (real_policy_dir(), real_dashboard_log_path()),
    );
    assert!(policy.ends_with("leviath"), "{}", policy.display());
    assert_eq!(log, dir.path().join(".leviath").join("dashboard.log"));
    assert!(!real_install_description().is_empty());
}

#[test]
fn about_describes_itself() {
    assert!(About::Setup.describe().contains("setting"));
    assert!(About::Run.describe().contains("run"));
    assert!(About::Agent.describe().contains("blueprint"));
    assert!(About::Other.describe().contains("else"));
}

// ─── The screen ──────────────────────────────────────────────────────────────

fn ctrl(c: char) -> crossterm::event::Event {
    key_with(KeyCode::Char(c), KeyModifiers::CONTROL)
}

async fn drive(ui: &mut Rage, events: Vec<crossterm::event::Event>) -> anyhow::Result<LoopExit> {
    let mut terminal = test_terminal();
    let mut source = TestEventSource::new(events);
    run_loop(
        ui,
        &mut terminal,
        &mut source,
        std::time::Duration::from_millis(1),
    )
    .await
}

#[tokio::test]
async fn the_screen_walks_every_step_and_writes_the_bundle() {
    with_env(|root| async move {
        plant(&root);
        let env = env_for(&root);
        let args = RageArgs {
            output: Some(root.join("tui.zip")),
            ..Default::default()
        };
        let ui = Rage::new(&args, &env).unwrap();
        assert_eq!(ui.step, Step::About);
        // Down to "A run", choose it, choose the newest run, type a note,
        // build, read the summary, done. The terminal is taken twice: once
        // for the questions, once for the summary.
        let mut setup = TestSetup::new();
        let mut events = TestEventSource::new(vec![
            key(KeyCode::Down),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            key(KeyCode::Char('h')),
            key(KeyCode::Char('i')),
            ctrl('s'),
            key(KeyCode::Enter),
        ]);
        execute_with(&args, &env, &mut setup, &mut events, true)
            .await
            .unwrap();
        assert_eq!(setup.enable_calls, 2);
        let zip = root.join("tui.zip");
        assert!(zip.exists());
        let members = unzip(&zip);
        assert!(names(&members).iter().any(|n| n.contains("runs/")));
        let readme = String::from_utf8_lossy(member(&members, "README.md")).into_owned();
        assert!(readme.contains("hi"), "{readme}");
    })
    .await
}

#[tokio::test]
async fn the_summary_screen_shows_the_warning_and_the_sections() {
    with_env(|root| async move {
        plant(&root);
        let env = env_for(&root);
        let args = RageArgs {
            run: Some(ROOT_RUN.to_string()),
            note: Some("preset".to_string()),
            output: Some(root.join("s.zip")),
            ..Default::default()
        };
        let mut ui = Rage::new(&args, &env).unwrap();
        assert_eq!(ui.step, Step::Summary, "everything was answered by flags");
        let mut terminal = test_terminal();
        // The loop hands the terminal back for the build, then shows it.
        let mut source =
            TestEventSource::new(vec![key(KeyCode::Char('x')), key(KeyCode::Char('q'))]);
        assert!(matches!(
            run_loop(
                &mut ui,
                &mut terminal,
                &mut source,
                std::time::Duration::from_millis(1)
            )
            .await
            .unwrap(),
            LoopExit::Build
        ));
        ui.outcome = Some(
            build(&env, &ui.selection(), ui.output.as_deref())
                .await
                .unwrap(),
        );
        // A key the summary does not answer to, then one it does.
        let exit = run_loop(
            &mut ui,
            &mut terminal,
            &mut source,
            std::time::Duration::from_millis(1),
        )
        .await
        .unwrap();
        assert!(matches!(exit, LoopExit::Finished(_)));
        let text = terminal.backend().text();
        assert!(text.contains("BEFORE YOU SHARE THIS FILE"), "{text}");
        assert!(text.contains("Your bundle"), "{text}");
        assert!(text.contains("config/"), "{text}");
        assert!(text.contains("Left out"), "{text}");
        assert!(text.contains("note:"), "{text}");
        assert!(text.contains("more, listed in manifest.json"), "{text}");
    })
    .await
}

#[tokio::test]
async fn the_flags_path_reports_a_bad_run_or_an_unwritable_zip() {
    with_env(|root| async move {
        plant(&root);
        let env = env_for(&root);
        let bad_run = RageArgs {
            run: Some("zzz".to_string()),
            ..Default::default()
        };
        assert!(run_non_interactive(&bad_run, &env).await.is_err());
        // The screen refuses the same run before taking the terminal.
        let mut setup = TestSetup::new();
        let mut events = TestEventSource::new(vec![]);
        assert!(
            execute_with(&bad_run, &env, &mut setup, &mut events, true)
                .await
                .is_err()
        );
        assert_eq!(setup.enable_calls, 0);
        let bad_zip = RageArgs {
            output: Some(root.join("nowhere").join("x.zip")),
            ..Default::default()
        };
        assert!(run_non_interactive(&bad_zip, &env).await.is_err());
    })
    .await
}

#[tokio::test]
async fn a_short_skipped_list_is_shown_whole() {
    with_env(|root| async move {
        let env = env_for(&root);
        let args = RageArgs {
            about: Some(About::Other),
            note: Some("n".to_string()),
            ..Default::default()
        };
        let mut ui = Rage::new(&args, &env).unwrap();
        ui.outcome = Some(Outcome {
            zip_path: root.join("x.zip"),
            zip_bytes: 10,
            sections: vec![],
            skipped: vec![
                SkippedEntry {
                    path: "logs/daemon.log".to_string(),
                    reason: "not present".to_string(),
                },
                SkippedEntry {
                    path: "logs/dashboard.log".to_string(),
                    reason: "not present".to_string(),
                },
            ],
            redactions: 0,
            notes: vec![],
        });
        let mut terminal = test_terminal();
        terminal
            .draw(|frame| super::render::draw(frame, &ui))
            .unwrap();
        let text = terminal.backend().text();
        assert!(text.contains("logs/dashboard.log"), "{text}");
        assert!(!text.contains("more, listed"), "{text}");
        // And nothing left out at all: no list.
        ui.outcome.as_mut().unwrap().skipped.clear();
        terminal
            .draw(|frame| super::render::draw(frame, &ui))
            .unwrap();
        let text = terminal.backend().text();
        assert!(!text.contains("Left out"), "{text}");
    })
    .await
}

#[test]
fn a_readme_for_a_bundle_with_nothing_left_out_has_no_such_list() {
    let text = report::readme(
        About::Other,
        "",
        None,
        "0.0.0",
        "b",
        "now",
        &Bundle::default(),
    );
    assert!(!text.contains("Left out, and why"));
    assert!(text.contains("No description was given"));
}

#[tokio::test]
async fn a_build_that_fails_is_shown_then_returned() {
    with_env(|root| async move {
        let env = env_for(&root);
        let args = RageArgs {
            about: Some(About::Other),
            note: Some("preset".to_string()),
            output: Some(root.join("nowhere").join("s.zip")),
            ..Default::default()
        };
        let mut setup = TestSetup::new();
        let mut events = TestEventSource::new(vec![key(KeyCode::Esc)]);
        let err = execute_with(&args, &env, &mut setup, &mut events, true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot write"), "{err}");
        // What the summary showed before Esc: the failure, in red.
        let mut ui = Rage::new(&args, &env).unwrap();
        ui.error = Some("cannot write it".to_string());
        let mut terminal = test_terminal();
        terminal
            .draw(|frame| super::render::draw(frame, &ui))
            .unwrap();
        let text = terminal.backend().text();
        assert!(text.contains("could not be written"), "{text}");
        assert!(text.contains("cannot write it"), "{text}");
    })
    .await
}

#[tokio::test]
async fn going_back_and_quitting_from_each_step() {
    with_env(|root| async move {
        plant(&root);
        let env = env_for(&root);
        let args = RageArgs::default();

        // Esc on the first step quits.
        let mut ui = Rage::new(&args, &env).unwrap();
        assert!(matches!(
            drive(&mut ui, vec![key(KeyCode::Esc)]).await.unwrap(),
            LoopExit::Quit
        ));

        // Setting up -> note -> Esc goes back to the category, whose chooser
        // reopens on the category that was chosen; Ctrl-C quits anywhere.
        let mut ui = Rage::new(&args, &env).unwrap();
        ui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert_eq!(ui.step, Step::Note);
        assert_eq!(ui.about, Some(About::Setup));
        ui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(ui.step, Step::About);
        assert_eq!(ui.picker.as_ref().unwrap().cursor, 0);
        ui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(ui.should_quit);

        // A run: Esc from the run chooser goes back to the category, and Esc
        // from the note goes back to the run chooser.
        let mut ui = Rage::new(&args, &env).unwrap();
        for code in [KeyCode::Down, KeyCode::Enter] {
            ui.handle_key(KeyEvent::new(code, KeyModifiers::empty()));
        }
        assert_eq!(ui.step, Step::Run);
        ui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(ui.step, Step::About);
        for code in [KeyCode::Enter, KeyCode::Enter] {
            ui.handle_key(KeyEvent::new(code, KeyModifiers::empty()));
        }
        assert_eq!(ui.step, Step::Note);
        assert_eq!(
            ui.run_id.as_deref(),
            Some(ROOT_RUN),
            "the newest run is first"
        );
        ui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(ui.step, Step::Run);

        // A blueprint: the third row, then the installed blueprint.
        let mut ui = Rage::new(&args, &env).unwrap();
        for code in [KeyCode::Down, KeyCode::Down, KeyCode::Enter, KeyCode::Enter] {
            ui.handle_key(KeyEvent::new(code, KeyModifiers::empty()));
        }
        assert_eq!(ui.step, Step::Note);
        assert!(ui.agent.as_ref().unwrap().ends_with("demo"));
        ui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(ui.step, Step::Agent);
        // Typing filters the chooser rather than acting.
        ui.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()));
        assert_eq!(ui.step, Step::Agent);

        // With the category preset, Esc from the chooser quits.
        let preset = RageArgs {
            about: Some(About::Run),
            ..Default::default()
        };
        let mut ui = Rage::new(&preset, &env).unwrap();
        assert_eq!(ui.step, Step::Run);
        ui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert!(ui.should_quit);

        // With the category and the run preset, the note is first and Esc quits.
        let preset = RageArgs {
            run: Some(ROOT_RUN.to_string()),
            ..Default::default()
        };
        let mut ui = Rage::new(&preset, &env).unwrap();
        assert_eq!(ui.step, Step::Note);
        assert_eq!(ui.about, Some(About::Run));
        ui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert!(ui.should_quit);

        // With the note preset, a choice goes straight to the summary.
        let preset = RageArgs {
            agent: Some(root.join("blueprint")),
            note: Some("n".to_string()),
            ..Default::default()
        };
        let ui = Rage::new(&preset, &env).unwrap();
        assert_eq!(ui.step, Step::Summary);
        let preset = RageArgs {
            about: Some(About::Run),
            note: Some("n".to_string()),
            ..Default::default()
        };
        let mut ui = Rage::new(&preset, &env).unwrap();
        ui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert_eq!(ui.step, Step::Summary);
        assert!(ui.needs_build());

        // A bad --run is refused before any terminal is taken.
        let bad = RageArgs {
            run: Some("zzz".to_string()),
            ..Default::default()
        };
        assert!(Rage::new(&bad, &env).is_err());
    })
    .await
}

#[tokio::test]
async fn empty_lists_say_so_in_the_footer() {
    with_env(|root| async move {
        let env = env_for(&root);
        let mut args = RageArgs {
            about: Some(About::Run),
            ..Default::default()
        };
        let ui = Rage::new(&args, &env).unwrap();
        assert!(ui.message.as_ref().unwrap().contains("No runs"));
        args.about = Some(About::Agent);
        let ui = Rage::new(&args, &env).unwrap();
        assert!(
            ui.message
                .as_ref()
                .unwrap()
                .contains("No installed blueprints")
        );
        // Drawn, so the footer message and the intro are exercised.
        let mut terminal = test_terminal();
        terminal
            .draw(|frame| super::render::draw(frame, &ui))
            .unwrap();
        let text = terminal.backend().text();
        assert!(text.contains("No installed blueprints"), "{text}");
        assert!(text.contains("Which blueprint?"), "{text}");
    })
    .await
}

#[tokio::test]
async fn the_loop_survives_ticks_releases_and_reports_terminal_failures() {
    with_env(|root| async move {
        plant(&root);
        let env = env_for(&root);
        let args = RageArgs::default();

        // A poll timeout and a key release both leave the screen where it is.
        let mut ui = Rage::new(&args, &env).unwrap();
        let mut terminal = test_terminal();
        let mut release = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        release.kind = KeyEventKind::Release;
        let mut source = TestEventSource::new_with_nones(vec![
            None,
            Some(crossterm::event::Event::Key(release)),
            Some(key(KeyCode::Esc)),
        ]);
        assert!(matches!(
            run_loop(
                &mut ui,
                &mut terminal,
                &mut source,
                std::time::Duration::from_millis(1)
            )
            .await
            .unwrap(),
            LoopExit::Quit
        ));

        // An event source that fails.
        let mut ui = Rage::new(&args, &env).unwrap();
        let mut failing = TestEventSource::failing();
        assert!(
            run_loop(
                &mut ui,
                &mut terminal,
                &mut failing,
                std::time::Duration::from_millis(1)
            )
            .await
            .is_err()
        );

        // A terminal that cannot be taken, one that cannot be created, and
        // one whose draws fail, before and after the build.
        let mut setup = TestSetup::new();
        setup.enable_should_fail = true;
        let mut events = TestEventSource::new(vec![]);
        assert!(
            execute_with(&args, &env, &mut setup, &mut events, true)
                .await
                .is_err()
        );
        let mut setup = TestSetup::new();
        setup.create_should_fail = true;
        assert!(
            execute_with(&args, &env, &mut setup, &mut events, true)
                .await
                .is_err()
        );
        let mut setup = TestSetup::new();
        setup.draw_should_fail = true;
        assert!(
            execute_with(&args, &env, &mut setup, &mut events, true)
                .await
                .is_err()
        );
        let preset = RageArgs {
            about: Some(About::Other),
            note: Some("n".to_string()),
            ..Default::default()
        };
        // The second take of the terminal, for the summary, can fail too.
        let mut setup = TestSetup::new();
        setup.enable_fails_on_call = Some(2);
        assert!(
            execute_with(&preset, &env, &mut setup, &mut events, true)
                .await
                .is_err()
        );
        let mut setup = TestSetup::new();
        setup.create_fails_on_call = Some(2);
        assert!(
            execute_with(&preset, &env, &mut setup, &mut events, true)
                .await
                .is_err()
        );
        let mut setup = TestSetup::new();
        setup.draw_should_fail = true;
        assert!(
            execute_with(&preset, &env, &mut setup, &mut events, true)
                .await
                .is_err()
        );

        // The whole thing through `execute_with`: cancelled, then written.
        let mut setup = TestSetup::new();
        let mut events = TestEventSource::new(vec![key(KeyCode::Esc)]);
        execute_with(&args, &env, &mut setup, &mut events, true)
            .await
            .unwrap();
        let written = RageArgs {
            about: Some(About::Other),
            note: Some("n".to_string()),
            output: Some(root.join("e.zip")),
            ..Default::default()
        };
        let mut events = TestEventSource::new(vec![key(KeyCode::Enter)]);
        execute_with(&written, &env, &mut setup, &mut events, true)
            .await
            .unwrap();
        assert!(root.join("e.zip").exists());
    })
    .await
}
