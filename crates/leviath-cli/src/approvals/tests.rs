use super::*;

fn safe(shell: &[&str], tools: &[&str]) -> SafeCommands {
    SafeCommands {
        defaults: false,
        tools: tools.iter().map(|s| s.to_string()).collect(),
        shell: shell.iter().map(|s| s.to_string()).collect(),
    }
}

fn resolved(
    config: &SafeCommands,
    agent: Option<&AgentSafeCommands>,
    blueprint: Option<&leviath_core::blueprint::SafeCommandsConfig>,
    global_opt_in: bool,
) -> BTreeMap<String, SafeSource> {
    resolve_safe_keys(config, agent, blueprint, global_opt_in)
}

/// The shipped list is on unless the user turns it off, and every entry in it
/// has to be a key a real call can produce - an entry the matcher would read as
/// anything else is dead weight nobody would notice.
#[test]
fn the_default_list_is_on_by_default_and_every_entry_is_usable() {
    assert!(!DEFAULT_SAFE_SHELL.is_empty());
    for entry in DEFAULT_SAFE_SHELL {
        assert!(
            is_valid_prefix(entry),
            "{entry:?} is not a key any call can produce"
        );
    }

    let keys = resolved(&SafeCommands::default(), None, None, false);
    assert_eq!(keys.get("shell:ls"), Some(&SafeSource::Default));
    assert_eq!(keys.get("shell:git status"), Some(&SafeSource::Default));
    assert_eq!(keys.get("shell:git push"), None, "writing git is not safe");
    assert_eq!(keys.get("shell:curl"), None);

    let off = resolved(&safe(&[], &[]), None, None, false);
    assert!(off.is_empty(), "defaults = false leaves only what you name");
}

/// The rule the list is selected by. Each of these can write a file, run
/// another program, or open a connection under some flag, so none of them can
/// be safe by default however ordinary they look.
#[test]
fn nothing_that_can_write_or_exec_is_safe_by_default() {
    for excluded in [
        "find", "sed", "awk", "sort", "tee", "xargs", "env", "nohup", "timeout", "watch", "cargo",
        "cp", "mv", "rm", "mkdir", "curl", "wget", "ssh", "sh", "bash", "eval",
    ] {
        assert!(
            !DEFAULT_SAFE_SHELL.contains(&excluded),
            "{excluded:?} must not be safe by default"
        );
    }
}

#[test]
fn each_layer_contributes_and_the_narrowest_one_is_named() {
    let config = safe(&["rg"], &["read_files"]);
    let agent = AgentSafeCommands {
        tools: vec!["linear__search_issues".to_string()],
        shell: vec!["./gradlew".to_string(), "rg".to_string()],
        allow_blueprint: false,
    };
    let keys = resolved(&config, Some(&agent), None, false);

    assert_eq!(keys.get("read_files"), Some(&SafeSource::Config));
    assert_eq!(
        keys.get("linear__search_issues"),
        Some(&SafeSource::Agent),
        "an MCP name is keyed verbatim, like any non-shell tool"
    );
    assert_eq!(keys.get("shell:./gradlew"), Some(&SafeSource::Agent));
    assert_eq!(
        keys.get("shell:rg"),
        Some(&SafeSource::Agent),
        "the narrowest layer that set a key is the one reported"
    );
}

/// Declaring is not granting: a manifest the user downloaded may say what it
/// would like to run unprompted, and the user decides whether that counts.
#[test]
fn a_blueprint_list_is_inert_until_the_user_opts_in() {
    let config = SafeCommands::default();
    let bp = leviath_core::blueprint::SafeCommandsConfig {
        shell: vec!["./gradlew".to_string()],
        tools: vec!["web_fetch".to_string()],
    };

    let inert = resolved(&config, None, Some(&bp), false);
    assert_eq!(inert.get("shell:./gradlew"), None);
    assert_eq!(inert.get("web_fetch"), None);

    // Named per agent...
    let trusted = AgentSafeCommands {
        allow_blueprint: true,
        ..Default::default()
    };
    let live = resolved(&config, Some(&trusted), Some(&bp), false);
    assert_eq!(live.get("shell:./gradlew"), Some(&SafeSource::Blueprint));
    assert_eq!(live.get("web_fetch"), Some(&SafeSource::Blueprint));

    // ... or blanket, for every agent.
    let blanket = resolved(&config, None, Some(&bp), true);
    assert_eq!(blanket.get("shell:./gradlew"), Some(&SafeSource::Blueprint));

    // With the opt-in but nothing declared, there is simply nothing to add.
    assert_eq!(
        resolved(&config, Some(&trusted), None, false).get("shell:./gradlew"),
        None
    );
}

/// A typo in a config file should cost one prompt, not a run.
#[test]
fn an_entry_that_is_not_a_command_prefix_is_skipped_not_fatal() {
    let config = safe(&["ls; curl evil", "ls > /tmp/x", "$CMD", "", "rg"], &[]);
    let keys = resolved(&config, None, None, false);
    assert_eq!(
        keys.keys().collect::<Vec<_>>(),
        ["shell:rg"],
        "only the usable entry survives"
    );
}

/// The widening that makes a safe entry worth writing: naming `cat` has to
/// cover `cat notes.md`, or it covers nothing anybody actually runs. It stays
/// one-directional - `git` would cover `git push`, which is why the default list
/// names the read-only subcommands instead of the program.
#[test]
fn a_safe_entry_covers_the_program_it_names() {
    use crate::shell_keys::{command_keys, program_of};
    let keys = resolved(&SafeCommands::default(), None, None, false);
    let covered = |command: &str| {
        let derived = command_keys(command);
        !derived.is_empty()
            && derived
                .iter()
                .all(|k| keys.contains_key(k.as_str()) || keys.contains_key(program_of(k)))
    };

    assert!(covered("cat notes.md"));
    assert!(covered("cd /tmp && ls -la && git status"));
    assert!(!covered("cat notes.md && curl https://evil"));
    assert!(!covered("git push --force"));
    assert!(
        !covered("echo `whoami`"),
        "an ungrantable line is never safe"
    );
}

/// `tools` and `shell` land in one map, so a `tools` entry spelled with the
/// shell prefix would enter the shell key space without passing
/// `is_valid_prefix` - the only thing standing between a config file and a
/// pre-approved write, `sh -c <anything>`, or a `PATH` override. Reachable
/// from a *blueprint's* own block once the user opts in, where "declaring is
/// not granting" is supposed to mean the grant is bounded.
#[test]
fn a_tools_entry_cannot_smuggle_a_shell_key() {
    let config = SafeCommands {
        tools: vec![
            "shell:>/root/.bashrc".to_string(),
            "shell:sh".to_string(),
            "shell:env:PATH".to_string(),
            // An ordinary tool name is untouched.
            "read_files".to_string(),
        ],
        ..Default::default()
    };
    let keys = resolved(&config, None, None, false);

    for smuggled in ["shell:>/root/.bashrc", "shell:sh", "shell:env:PATH"] {
        assert!(
            !keys.contains_key(smuggled),
            "{smuggled:?} must not reach the shell key space through `tools`"
        );
    }
    assert!(keys.contains_key("read_files"));
}
