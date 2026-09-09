//! `lev yolo` - the profiles behind `--yolo=<name>`, and what each decides.
//!
//! Three questions a person asks about a profile, in the order they ask them:
//! what profiles do I have (`list`), what does this one say (`show`), and
//! what would it do with this call (`test`). `init` writes a commented example
//! to start from. Everything reads the file as it stands, the same way a spawn
//! does, so what this prints is what the next run gets.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use crate::config::ToolPolicy;
use crate::yolo::decide::{DecideInput, Platform, ToolKind};
use crate::yolo::{YoloFile, YoloProfile, yolo_path};

/// The example `lev yolo init` writes, and the docs publish.
pub(crate) const EXAMPLE_TOML: &str = include_str!("../yolo/example.toml");

/// Arguments for `lev yolo`.
#[derive(Args)]
pub struct YoloArgs {
    /// Which yolo subcommand to run.
    #[command(subcommand)]
    pub command: YoloCommand,
}

/// The `lev yolo` subcommands.
#[derive(Subcommand)]
pub enum YoloCommand {
    /// List the profiles in yolo.toml
    List(ListArgs),
    /// Show one profile in full
    Show(ShowArgs),
    /// Say what a profile would decide for one tool call
    Test(TestArgs),
    /// Write a commented example yolo.toml beside your config
    Init(InitArgs),
}

/// Arguments for `lev yolo list`.
#[derive(Args, Default)]
pub struct ListArgs {
    /// Emit the listing as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `lev yolo show`.
#[derive(Args)]
pub struct ShowArgs {
    /// The profile's name, as passed to `--yolo=<name>`.
    pub name: String,
    /// Emit the profile as JSON rather than TOML.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `lev yolo test`.
#[derive(Args)]
pub struct TestArgs {
    /// The profile to test.
    pub name: String,
    /// The tool the model would call (`shell`, `write_file`, `github__create_issue`).
    #[arg(long)]
    pub tool: String,
    /// For the shell: the command line.
    #[arg(long)]
    pub command: Option<String>,
    /// The call's arguments as a JSON object, for a tool that is not the shell.
    #[arg(long, value_name = "JSON")]
    pub args: Option<String>,
    /// The workdir the run would have; relative paths in the command resolve
    /// against it. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    pub workdir: Option<PathBuf>,
    /// What the config layers resolve the tool to (`allow`, `ask`, `deny`).
    /// Left off, the answer is read from your config.toml.
    #[arg(long, value_name = "POLICY")]
    pub configured: Option<String>,
    /// Where the tool comes from, for @group matching: `builtin`, `subagent`,
    /// `script` or `mcp`. Left off, it is guessed from the name.
    #[arg(long)]
    pub kind: Option<String>,
    /// Decide as if `--allow <tool>` had been passed for the run.
    #[arg(long)]
    pub allowed: bool,
    /// Emit the decision as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `lev yolo init`.
#[derive(Args, Default)]
pub struct InitArgs {
    /// Overwrite a file that is already there.
    #[arg(long)]
    pub force: bool,
}

/// Run `lev yolo`.
pub(crate) async fn execute(args: YoloArgs) -> anyhow::Result<()> {
    let path = yolo_path();
    let out = match args.command {
        YoloCommand::List(a) => render_list(&crate::yolo::load_current()?, &path, a.json),
        YoloCommand::Show(a) => {
            let file = crate::yolo::load_current()?;
            let profile = file.resolve(Some(&a.name), &path)?;
            render_show(&profile, a.json)?
        }
        YoloCommand::Test(a) => {
            let file = crate::yolo::load_current()?;
            let profile = file.resolve(Some(&a.name), &path)?;
            let configured = configured_policy(&a, crate::config::Config::load().ok().as_ref())?;
            render_test(&profile, &a, configured)?
        }
        YoloCommand::Init(a) => init(&path, a.force)?,
    };
    println!("{out}");
    Ok(())
}

/// The listing: where the file is, and one line per profile.
fn render_list(file: &YoloFile, path: &Path, json: bool) -> String {
    if json {
        let profiles: Vec<_> = file.profiles().map(|p| p.summary()).collect();
        return serde_json::to_string_pretty(&serde_json::json!({
            "path": path.display().to_string(),
            "exists": file.exists(),
            "profiles": profiles,
        }))
        .expect("a listing serializes");
    }
    let mut lines = vec![format!("yolo profiles ({})", path.display())];
    if !file.exists() {
        lines.push("  no file yet; `lev yolo init` writes an example".to_string());
        return lines.join("\n");
    }
    let names = file.names();
    if names.is_empty() {
        lines.push("  the file defines no profiles".to_string());
        return lines.join("\n");
    }
    let width = names.iter().map(String::len).max().unwrap_or(0);
    for profile in file.profiles() {
        let s = profile.summary();
        lines.push(format!(
            "  {:width$}  default={} questions={} checkpoints={} gate={}  tools {}/{}/{}  shell {}/{}/{}",
            s.name,
            summary_word(s.default == crate::yolo::rules::Waiver::Allow),
            human_word(s.questions),
            human_word(s.checkpoints),
            human_word(s.gate),
            s.tool_rules[0],
            s.tool_rules[1],
            s.tool_rules[2],
            s.shell_rules[0],
            s.shell_rules[1],
            s.shell_rules[2],
        ));
    }
    lines.push("  (counts are allow/ask/deny; bare --yolo reads none of this)".to_string());
    lines.join("\n")
}

fn summary_word(allow: bool) -> &'static str {
    match allow {
        true => "allow",
        false => "ask",
    }
}

fn human_word(h: crate::yolo::rules::Human) -> &'static str {
    match h.is_auto() {
        true => "auto",
        false => "ask",
    }
}

/// One profile in full: its TOML as the file would spell it, and what it
/// keeps for a person.
fn render_show(profile: &YoloProfile, json: bool) -> anyhow::Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(&serde_json::json!({
            "name": profile.name,
            "spec": profile.spec,
            "holds": profile.holds(),
        }))?);
    }
    let mut table = toml::map::Map::new();
    table.insert(profile.name.clone(), toml::Value::try_from(&profile.spec)?);
    let mut out = toml::to_string(&toml::Value::Table(table))?;
    let holds = profile.holds();
    if !holds.is_empty() {
        out.push_str("\n# keeps for you:\n");
        for line in holds {
            out.push_str(&format!("#   {line}\n"));
        }
    }
    Ok(out)
}

/// What the config layers say about the tool, before the profile: the
/// `--configured` override, else the user's global `[tool_permissions]` over
/// the built-in default, clamped by `write_file`'s answer for a shell line
/// that redirects, as the tool lane does.
fn configured_policy(
    args: &TestArgs,
    config: Option<&crate::config::Config>,
) -> anyhow::Result<ToolPolicy> {
    if let Some(word) = &args.configured {
        return match word.to_ascii_lowercase().as_str() {
            "allow" => Ok(ToolPolicy::Allow),
            "ask" => Ok(ToolPolicy::Ask),
            "deny" => Ok(ToolPolicy::Deny),
            other => anyhow::bail!("--configured takes allow, ask or deny, not {other:?}"),
        };
    }
    let launch: std::collections::HashMap<String, ToolPolicy> = std::collections::HashMap::new();
    let blueprint: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let global = config
        .map(|c| c.tool_permissions.clone())
        .unwrap_or_default();
    let resolve = |tool: &str| {
        crate::tools::resolve_policy(
            tool,
            leviath_tools::is_builtin_tool(leviath_tools::canonical_tool_name(tool)),
            &launch,
            &blueprint,
            &blueprint,
            &global,
            false,
        )
    };
    let policy = resolve(&args.tool);
    Ok(crate::tools::clamp_by_effect(
        &args.tool,
        &arguments_of(args)?,
        policy,
        &|| resolve("write_file"),
    ))
}

/// The call's arguments: `--command` for the shell, `--args` JSON otherwise.
fn arguments_of(args: &TestArgs) -> anyhow::Result<serde_json::Value> {
    if let Some(command) = &args.command {
        return Ok(serde_json::json!({ "command": command }));
    }
    match &args.args {
        Some(raw) => Ok(serde_json::from_str(raw)
            .map_err(|e| anyhow::anyhow!("--args is not valid JSON: {e}"))?),
        None => Ok(serde_json::json!({})),
    }
}

/// Where the tool comes from: the `--kind` word, else guessed from the name.
fn kind_of(args: &TestArgs) -> anyhow::Result<ToolKind> {
    let canonical = leviath_tools::canonical_tool_name(&args.tool);
    match args.kind.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None => Ok(ToolKind::classify(
            canonical,
            leviath_tools::is_builtin_tool(canonical),
            false,
        )),
        Some("builtin") => Ok(ToolKind::Builtin),
        Some("subagent") => Ok(ToolKind::Subagent),
        Some("script") => Ok(ToolKind::Script),
        Some("mcp") => Ok(ToolKind::Mcp),
        Some(other) => {
            anyhow::bail!("--kind takes builtin, subagent, script or mcp, not {other:?}")
        }
    }
}

/// The decision for one call, and why.
fn render_test(
    profile: &YoloProfile,
    args: &TestArgs,
    configured: ToolPolicy,
) -> anyhow::Result<String> {
    let arguments = arguments_of(args)?;
    let kind = kind_of(args)?;
    let workdir = match &args.workdir {
        Some(dir) => dir.clone(),
        None => std::env::current_dir()?,
    };
    let home = crate::yolo::home();
    let decision = profile.decide(&DecideInput {
        tool: &args.tool,
        arguments: &arguments,
        configured,
        launch_allowed: args.allowed,
        kind,
        workdir: &workdir,
        home: home.as_deref(),
        platform: Platform::host(),
    });
    if args.json {
        return Ok(serde_json::to_string_pretty(&serde_json::json!({
            "profile": profile.name,
            "tool": args.tool,
            "configured": policy_word(configured),
            "policy": policy_word(decision.policy),
            "reason": decision.reason,
        }))?);
    }
    Ok(format!(
        "{}: {} ({})\n  configured by the config layers: {}\n  under --yolo={}: {}",
        args.tool,
        policy_word(decision.policy),
        decision.reason,
        policy_word(configured),
        profile.name,
        match decision.policy {
            ToolPolicy::Allow => "runs without a prompt",
            ToolPolicy::Ask => "goes through the ordinary approval prompt",
            ToolPolicy::Deny => "is refused",
        }
    ))
}

fn policy_word(policy: ToolPolicy) -> &'static str {
    match policy {
        ToolPolicy::Allow => "allow",
        ToolPolicy::Ask => "ask",
        ToolPolicy::Deny => "deny",
    }
}

/// Write the example, refusing to replace a file that is there unless told.
fn init(path: &Path, force: bool) -> anyhow::Result<String> {
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists; edit it, or pass --force to replace it with the example",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, EXAMPLE_TOML)?;
    Ok(format!(
        "wrote {}\n  run one with `lev run <agent> --yolo=careful`, and `lev yolo list` shows the rest",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_args(name: &str, tool: &str, command: Option<&str>) -> TestArgs {
        TestArgs {
            name: name.to_string(),
            tool: tool.to_string(),
            command: command.map(str::to_string),
            args: None,
            workdir: Some(std::env::temp_dir()),
            configured: None,
            kind: None,
            allowed: false,
            json: false,
        }
    }

    #[test]
    fn the_example_parses_and_lists() {
        let file = YoloFile::from_toml(EXAMPLE_TOML).expect("the example parses");
        assert_eq!(file.names(), ["build-only", "careful"]);
        let text = render_list(&file, Path::new("/x/yolo.toml"), false);
        assert!(text.contains("careful"), "{text}");
        assert!(
            text.contains("default=ask questions=ask checkpoints=ask gate=auto"),
            "{text}"
        );
        assert!(text.contains("tools 1/2/0  shell 3/1/1"), "{text}");
        assert!(text.contains("build-only"), "{text}");
        let json: serde_json::Value =
            serde_json::from_str(&render_list(&file, Path::new("/x/yolo.toml"), true)).unwrap();
        assert_eq!(json["exists"], true);
        assert_eq!(json["profiles"][1]["name"], "careful");
        assert_eq!(
            json["profiles"][1]["shell_rules"],
            serde_json::json!([3, 1, 1])
        );
    }

    #[test]
    fn the_listing_says_when_there_is_nothing() {
        let missing = render_list(&YoloFile::missing(), Path::new("/x/yolo.toml"), false);
        assert!(missing.contains("no file yet"), "{missing}");
        let empty = YoloFile::from_toml("").unwrap();
        let text = render_list(&empty, Path::new("/x/yolo.toml"), false);
        assert!(text.contains("defines no profiles"), "{text}");
        let json: serde_json::Value =
            serde_json::from_str(&render_list(&YoloFile::missing(), Path::new("/x"), true))
                .unwrap();
        assert_eq!(json["exists"], false);
    }

    #[test]
    fn show_prints_the_profile_as_toml_with_its_holds() {
        let file = YoloFile::from_toml(EXAMPLE_TOML).unwrap();
        let careful = file.get("careful").unwrap();
        let text = render_show(&careful, false).unwrap();
        assert!(text.starts_with("[careful]"), "{text}");
        assert!(text.contains("default = \"ask\""), "{text}");
        assert!(text.contains("[[careful.shell.deny]]"), "{text}");
        assert!(text.contains("# keeps for you:"), "{text}");
        assert!(text.contains("#   the model's questions"), "{text}");
        let loose = YoloFile::from_toml("[loose]\ndefault = \"allow\"\n").unwrap();
        let text = render_show(&loose.get("loose").unwrap(), false).unwrap();
        assert!(!text.contains("keeps for you"), "{text}");
        let json: serde_json::Value =
            serde_json::from_str(&render_show(&careful, true).unwrap()).unwrap();
        assert_eq!(json["name"], "careful");
        assert_eq!(json["spec"]["default"], "ask");
        assert!(json["holds"].as_array().unwrap().len() >= 2);
    }

    #[test]
    fn test_reports_the_verdict_and_the_rule() {
        let file = YoloFile::from_toml(EXAMPLE_TOML).unwrap();
        let careful = file.get("careful").unwrap();
        let args = test_args("careful", "shell", Some("curl https://x"));
        let text = render_test(&careful, &args, ToolPolicy::Ask).unwrap();
        assert!(
            text.starts_with("shell: deny (shell deny rule \"curl\")"),
            "{text}"
        );
        assert!(text.contains("is refused"), "{text}");
        let args = test_args("careful", "shell", Some("cargo test"));
        let text = render_test(&careful, &args, ToolPolicy::Ask).unwrap();
        assert!(text.contains("runs without a prompt"), "{text}");
        let args = test_args("careful", "web_fetch", None);
        let text = render_test(&careful, &args, ToolPolicy::Allow).unwrap();
        assert!(text.contains("ordinary approval prompt"), "{text}");
        let mut args = test_args("careful", "shell", Some("git push origin"));
        args.json = true;
        let json: serde_json::Value =
            serde_json::from_str(&render_test(&careful, &args, ToolPolicy::Ask).unwrap()).unwrap();
        assert_eq!(json["policy"], "ask");
        assert_eq!(json["configured"], "ask");
        assert_eq!(json["reason"], "shell ask rule \"git push*\"");
        // No workdir given: the current directory stands in.
        let mut args = test_args("careful", "shell", Some("ls"));
        args.workdir = None;
        assert!(render_test(&careful, &args, ToolPolicy::Ask).is_ok());
    }

    #[test]
    fn test_reads_arguments_and_kinds() {
        let file = YoloFile::from_toml(
            "[p]\ndefault = \"ask\"\n[p.tools]\nallow = [\"@mcp\"]\ndeny = [\"@script\"]\n",
        )
        .unwrap_err();
        assert!(file.to_string().contains("not a tool group"), "{file}");
        let file = YoloFile::from_toml(
            "[p]\ndefault = \"ask\"\n[p.tools]\nallow = [\"@mcp\"]\ndeny = [\"@scripts\"]\n",
        )
        .unwrap();
        let p = file.get("p").unwrap();
        let mut args = test_args("p", "acme__thing", None);
        args.args = Some("{\"x\": 1}".to_string());
        assert!(
            render_test(&p, &args, ToolPolicy::Ask)
                .unwrap()
                .contains("allow")
        );
        args.kind = Some("script".to_string());
        assert!(
            render_test(&p, &args, ToolPolicy::Ask)
                .unwrap()
                .contains("deny")
        );
        for (kind, word) in [("builtin", "ask"), ("subagent", "ask"), ("mcp", "allow")] {
            args.kind = Some(kind.to_string());
            let text = render_test(&p, &args, ToolPolicy::Ask).unwrap();
            assert!(
                text.starts_with(&format!("acme__thing: {word}")),
                "{kind}: {text}"
            );
        }
        args.kind = Some("robot".to_string());
        assert!(
            render_test(&p, &args, ToolPolicy::Ask)
                .unwrap_err()
                .to_string()
                .contains("--kind")
        );
        args.kind = None;
        args.args = Some("not json".to_string());
        assert!(
            render_test(&p, &args, ToolPolicy::Ask)
                .unwrap_err()
                .to_string()
                .contains("--args")
        );
        // A launch allow survives an ask list.
        let file =
            YoloFile::from_toml("[p]\ndefault = \"ask\"\n[p.tools]\nask = [\"web_fetch\"]\n")
                .unwrap();
        let p = file.get("p").unwrap();
        let mut args = test_args("p", "web_fetch", None);
        args.allowed = true;
        assert!(
            render_test(&p, &args, ToolPolicy::Allow)
                .unwrap()
                .starts_with("web_fetch: allow")
        );
    }

    #[test]
    fn the_configured_policy_comes_from_the_flag_or_the_config() {
        let mut args = test_args("p", "shell", Some("echo hi"));
        for (word, policy) in [
            ("allow", ToolPolicy::Allow),
            ("ASK", ToolPolicy::Ask),
            ("deny", ToolPolicy::Deny),
        ] {
            args.configured = Some(word.to_string());
            assert_eq!(configured_policy(&args, None).unwrap(), policy, "{word}");
        }
        args.configured = Some("maybe".to_string());
        assert!(
            configured_policy(&args, None)
                .unwrap_err()
                .to_string()
                .contains("--configured")
        );
        args.configured = None;
        // No config: the built-in default, `ask` for the shell.
        assert_eq!(configured_policy(&args, None).unwrap(), ToolPolicy::Ask);
        // The user's global table is the ceiling, and a redirect is clamped by
        // `write_file` as in the tool lane.
        let config = crate::config::Config {
            tool_permissions: std::collections::HashMap::from([
                ("shell".to_string(), ToolPolicy::Allow),
                ("write_file".to_string(), ToolPolicy::Deny),
            ]),
            ..Default::default()
        };
        assert_eq!(
            configured_policy(&args, Some(&config)).unwrap(),
            ToolPolicy::Allow
        );
        args.command = Some("echo hi > out".to_string());
        assert_eq!(
            configured_policy(&args, Some(&config)).unwrap(),
            ToolPolicy::Deny
        );
        let args = test_args("p", "read_file", None);
        assert_eq!(configured_policy(&args, None).unwrap(), ToolPolicy::Allow);
    }

    #[test]
    fn init_writes_once_and_replaces_only_when_forced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("yolo.toml");
        let out = init(&path, false).unwrap();
        assert!(out.contains("wrote"), "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EXAMPLE_TOML);
        let err = init(&path, false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        std::fs::write(&path, "[x]\ndefault = \"allow\"\n").unwrap();
        init(&path, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EXAMPLE_TOML);
    }

    /// The command end to end, through the isolated config path: every
    /// subcommand against a real file.
    #[tokio::test]
    async fn execute_runs_every_subcommand_against_the_file() {
        crate::config::with_isolated_config_path_async("lev-yolo-execute", |_cfg| async move {
            execute(YoloArgs {
                command: YoloCommand::List(ListArgs::default()),
            })
            .await
            .expect("list with no file");
            execute(YoloArgs {
                command: YoloCommand::Init(InitArgs::default()),
            })
            .await
            .expect("init");
            execute(YoloArgs {
                command: YoloCommand::List(ListArgs { json: true }),
            })
            .await
            .expect("list");
            execute(YoloArgs {
                command: YoloCommand::Show(ShowArgs {
                    name: "careful".to_string(),
                    json: false,
                }),
            })
            .await
            .expect("show");
            let err = execute(YoloArgs {
                command: YoloCommand::Show(ShowArgs {
                    name: "nope".to_string(),
                    json: false,
                }),
            })
            .await
            .expect_err("unknown profile");
            assert!(err.to_string().contains("no yolo profile"), "{err}");
            execute(YoloArgs {
                command: YoloCommand::Test(test_args("careful", "shell", Some("cargo test"))),
            })
            .await
            .expect("test");
        })
        .await;
    }
}
