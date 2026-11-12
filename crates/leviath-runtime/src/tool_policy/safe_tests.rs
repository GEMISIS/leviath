use std::collections::HashMap;

use super::*;
use crate::spec::names::ToolName;

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(ToString::to_string).collect()
}

/// A `[safe_commands]` block with the shipped list off.
fn safe(shell: &[&str], tools: &[&str]) -> SafeCommandSettings {
    SafeCommandSettings {
        defaults: false,
        tools: strings(tools),
        shell: strings(shell),
    }
}

/// A host's reading of a shell entry, for these tests: one word, or two.
fn one_or_two_words(entry: &str) -> bool {
    !entry.is_empty() && entry.split(' ').count() <= 2
}

/// The list as a host that reads shell lines holds it.
fn with_shell(
    config: &SafeCommandSettings,
    agent: Option<&AgentSafeCommandSettings>,
    blueprint: Option<&SafeCommandsDef>,
    global_opt_in: bool,
) -> BTreeMap<String, SafeSource> {
    resolve_safe_keys(
        config,
        agent,
        blueprint,
        global_opt_in,
        Some(&one_or_two_words),
    )
}

/// The list as a caller that reads no shell lines holds it.
fn tools_only(
    config: &SafeCommandSettings,
    agent: Option<&AgentSafeCommandSettings>,
    blueprint: Option<&SafeCommandsDef>,
    global_opt_in: bool,
) -> BTreeMap<String, SafeSource> {
    resolve_safe_keys(config, agent, blueprint, global_opt_in, None)
}

/// The shipped list is on unless the user turns it off, both for a user
/// with no `[safe_commands]` table and for one whose table says nothing
/// about it, and it is the shell half of the list: a caller that reads no
/// shell lines holds none of it.
#[test]
fn the_shipped_list_is_on_by_default_for_a_host_that_reads_shell_lines() {
    assert!(!DEFAULT_SAFE_SHELL.is_empty());
    let stock = SafeCommandSettings::default();
    assert!(stock.defaults);
    let parsed: SafeCommandSettings = toml::from_str("tools = [\"read_files\"]").unwrap();
    assert!(parsed.defaults, "an absent key reads as the default");
    assert_eq!(parsed.tools, ["read_files"]);
    let off: SafeCommandSettings = toml::from_str("defaults = false").unwrap();
    assert!(!off.defaults);

    let keys = with_shell(&stock, None, None, false);
    assert_eq!(keys.len(), DEFAULT_SAFE_SHELL.len());
    assert_eq!(keys.get("shell:ls"), Some(&SafeSource::Default));
    assert_eq!(keys.get("shell:git status"), Some(&SafeSource::Default));
    assert_eq!(keys.get("shell:curl"), None);

    assert!(with_shell(&off, None, None, false).is_empty());
    assert!(
        tools_only(&stock, None, None, false).is_empty(),
        "the shipped list names shell commands only"
    );
}

/// Each layer adds to the list, and the narrowest one that set a key is the
/// one it is reported under.
#[test]
fn each_layer_contributes_and_the_narrowest_one_is_named() {
    let config = safe(&["rg"], &["read_files"]);
    let agent = AgentSafeCommandSettings {
        tools: strings(&["linear__search_issues"]),
        shell: strings(&["./gradlew", "rg"]),
        allow_blueprint: false,
    };
    let keys = with_shell(&config, Some(&agent), None, false);
    assert_eq!(keys.get("read_files"), Some(&SafeSource::Config));
    assert_eq!(
        keys.get("linear__search_issues"),
        Some(&SafeSource::Agent),
        "an MCP name is keyed verbatim, like any non-shell tool"
    );
    assert_eq!(keys.get("shell:./gradlew"), Some(&SafeSource::Agent));
    assert_eq!(keys.get("shell:rg"), Some(&SafeSource::Agent));

    // The same layers without the shell half: the tool names, and nothing
    // under the shell prefix.
    let keys = tools_only(&config, Some(&agent), None, false);
    assert_eq!(
        keys,
        BTreeMap::from([
            ("read_files".to_string(), SafeSource::Config),
            ("linear__search_issues".to_string(), SafeSource::Agent),
        ])
    );
}

/// Declaring is not granting: a blueprint the user downloaded may say what it
/// would like to run unprompted, and the user decides whether that counts,
/// for the one blueprint or for every one.
#[test]
fn a_graphs_own_list_is_inert_until_the_user_opts_in() {
    let config = SafeCommandSettings::default();
    let graph = SafeCommandsDef {
        shell: strings(&["./gradlew"]),
        tools: vec![ToolName::new("web_fetch").unwrap()],
    };

    let inert = with_shell(&config, None, Some(&graph), false);
    assert_eq!(inert.get("shell:./gradlew"), None);
    assert_eq!(inert.get("web_fetch"), None);
    let untrusted = AgentSafeCommandSettings::default();
    assert_eq!(
        with_shell(&config, Some(&untrusted), Some(&graph), false).get("web_fetch"),
        None
    );

    let trusted = AgentSafeCommandSettings {
        allow_blueprint: true,
        ..Default::default()
    };
    let live = with_shell(&config, Some(&trusted), Some(&graph), false);
    assert_eq!(live.get("shell:./gradlew"), Some(&SafeSource::Blueprint));
    assert_eq!(live.get("web_fetch"), Some(&SafeSource::Blueprint));

    let blanket = with_shell(&config, None, Some(&graph), true);
    assert_eq!(blanket.get("shell:./gradlew"), Some(&SafeSource::Blueprint));
    assert_eq!(
        tools_only(&config, None, Some(&graph), true),
        BTreeMap::from([("web_fetch".to_string(), SafeSource::Blueprint)])
    );

    // With the opt-in but nothing declared, there is nothing to add.
    assert_eq!(
        with_shell(&config, Some(&trusted), None, false).get("web_fetch"),
        None
    );
}

/// A typo in a config file costs one prompt, not a run: an entry the host
/// cannot match is left out and the rest stand.
#[test]
fn a_shell_entry_the_host_cannot_match_is_skipped_not_fatal() {
    let _trace = leviath_testkit::tracing_guard();
    let config = safe(&["ls; curl evil", "a b c", "", "rg"], &[]);
    let keys = with_shell(&config, None, None, false);
    assert_eq!(keys.keys().collect::<Vec<_>>(), ["shell:rg"]);
}

/// `tools` and `shell` land in one map, so a `tools` entry spelled with the
/// shell prefix would enter the shell key space unchecked. It is refused
/// whoever resolves the list.
#[test]
fn a_tools_entry_cannot_smuggle_a_shell_key() {
    let _trace = leviath_testkit::tracing_guard();
    let config = SafeCommandSettings {
        tools: strings(&["shell:>/root/.bashrc", "shell:sh", "read_files"]),
        ..SafeCommandSettings::default()
    };
    for keys in [
        with_shell(&config, None, None, false),
        tools_only(&config, None, None, false),
    ] {
        assert!(!keys.contains_key("shell:>/root/.bashrc"));
        assert!(!keys.contains_key("shell:sh"));
        assert_eq!(keys.get("read_files"), Some(&SafeSource::Config));
    }
}

/// A call that is not a shell line is approved under its name, a spawn that
/// carries a graph under the raw-graph permission, and a shell call under
/// nothing a tool name can stand for.
#[test]
fn a_call_that_is_not_a_shell_line_has_one_key() {
    let none = serde_json::json!({});
    assert_eq!(
        approval_key("gh__search", &none),
        Some("gh__search".to_string())
    );
    assert_eq!(
        approval_key("spawn_agent", &none),
        Some("spawn_agent".to_string())
    );
    assert_eq!(
        approval_key("spawn_agent", &serde_json::json!({"source": {"graph": {}}})),
        Some(leviath_tools::SPAWN_RAW_GRAPH_PERMISSION.to_string())
    );
    let line = serde_json::json!({"command": "ls"});
    assert_eq!(approval_key("shell", &line), None);
    assert_eq!(approval_key("bash", &line), None);
}

/// The user's settings resolve the list for a run: the per-agent table of
/// the blueprint it came from and no other, none for a graph its caller
/// wrote, and the graph's own list under the user's switch.
#[test]
fn the_users_settings_resolve_a_runs_safe_keys() {
    let settings = ToolPermissions {
        safe_commands: safe(&["rg"], &["read_files"]),
        agent_safe_commands: HashMap::from([(
            "coder".to_string(),
            AgentSafeCommandSettings {
                tools: strings(&["gh__search"]),
                ..Default::default()
            },
        )]),
        ..ToolPermissions::default()
    };
    let graph = SafeCommandsDef {
        shell: vec![],
        tools: vec![ToolName::new("web_fetch").unwrap()],
    };
    let names = |agent: Option<&str>, settings: &ToolPermissions| -> Vec<String> {
        settings
            .safe_keys(agent, Some(&graph), None)
            .into_keys()
            .collect()
    };
    assert_eq!(
        names(Some("coder"), &settings),
        ["gh__search", "read_files"]
    );
    assert_eq!(names(Some("researcher"), &settings), ["read_files"]);
    assert_eq!(names(None, &settings), ["read_files"]);

    let opted_in = ToolPermissions {
        allow_blueprint_safe_commands: true,
        ..settings.clone()
    };
    assert_eq!(names(None, &opted_in), ["read_files", "web_fetch"]);
    assert_eq!(
        settings
            .safe_keys(None, None, Some(&one_or_two_words))
            .get("shell:rg"),
        Some(&SafeSource::Config)
    );
}
