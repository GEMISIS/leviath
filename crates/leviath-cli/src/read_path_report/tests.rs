use super::*;
use crate::config::{Config, ReadPathGrants};

/// A blueprint whose only interesting part is its `[read_paths]` block.
fn blueprint(name: &str, allow: &[&str]) -> leviath_core::Blueprint {
    let listed = allow
        .iter()
        .map(|e| format!("\"{e}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let block = if allow.is_empty() {
        String::new()
    } else {
        format!("[read_paths]\nallow = [{listed}]\n")
    };
    let toml = format!(
        r#"
[agent]
name = "{name}"
version = "0.1.0"
description = "test blueprint"

[stages.plan]
mode = "autonomous"

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}
conversation = {{ kind = "sliding_window", max_items = 50, max_tokens = 10000 }}

{block}"#
    );
    leviath_core::manifest::parse_manifest(&toml).expect("blueprint parses")
}

/// A config granting `machine_wide` to everyone and `per_agent` to `agent`.
fn config(machine_wide: &[&str], agent: &str, per_agent: &[&str]) -> Config {
    let mut config = Config::default();
    config.security.read_paths = machine_wide.iter().map(|s| s.to_string()).collect();
    if !per_agent.is_empty() {
        config.agent_read_paths.insert(
            agent.to_string(),
            ReadPathGrants {
                allow: per_agent.iter().map(|s| s.to_string()).collect(),
            },
        );
    }
    config
}

/// Run the report with the platform inputs pinned, so the same expectations
/// hold on every OS.
fn report(agent: &str, declared: &[&str], config: &Config) -> GrantReport {
    report_entries(
        agent,
        &declared.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        config,
        std::path::Path::new("/work"),
        Some(std::path::Path::new("/home/me")),
        false,
    )
    .expect("grants compile")
}

fn statuses(report: &GrantReport) -> Vec<GrantStatus> {
    report.entries.iter().map(|e| e.status).collect()
}

// -- what counts as declaring anything ---------------------------------

#[test]
fn a_blueprint_without_read_paths_has_no_report() {
    assert!(
        build(
            &blueprint("a", &[]),
            &Config::default(),
            std::path::Path::new("/work")
        )
        .is_none()
    );
}

/// `[read_paths]` with no `allow` key parses as present-but-empty; it declares
/// nothing, so it reports nothing.
#[test]
fn an_empty_allow_list_has_no_report() {
    let mut bp = blueprint("a", &[]);
    bp.read_paths = Some(leviath_core::ReadPathsConfig { allow: Vec::new() });
    assert!(build(&bp, &Config::default(), std::path::Path::new("/work")).is_none());
}

/// The public entry point, on the real home directory and platform flags.
#[test]
fn build_reports_every_declared_entry() {
    let bp = blueprint("cto", &["/data/runs", "/data/docs"]);
    let report = build(&bp, &Config::default(), std::path::Path::new("/work"))
        .expect("declares read paths")
        .expect("grants compile");
    assert_eq!(report.agent, "cto");
    assert_eq!(report.declared(), 2);
    assert_eq!(report.granted(), 0);
}

// -- verdicts ----------------------------------------------------------

#[test]
fn nothing_granted_means_every_entry_is_refused() {
    let report = report("cto", &["/data/runs", "glob:/docs/**"], &Config::default());
    assert_eq!(
        statuses(&report),
        vec![GrantStatus::NotGranted, GrantStatus::NotGranted]
    );
    assert_eq!(report.ungranted(), vec!["/data/runs", "glob:/docs/**"]);
    assert_eq!(report.summary(), "2 declared, 0 granted");
}

#[test]
fn the_blanket_override_grants_every_declaration() {
    let mut config = Config::default();
    config.security.allow_blueprint_read_paths = true;
    let report = report("cto", &["/data/runs", "glob:/docs/**"], &config);
    assert_eq!(
        statuses(&report),
        vec![GrantStatus::Granted, GrantStatus::Granted]
    );
    assert!(!report.has_ungranted());
    assert!(report.allow_blueprint);
}

/// The reported case: some entries granted, some not. The counts and the
/// stanza both have to name only what is missing.
#[test]
fn a_partial_grant_is_reported_entry_by_entry() {
    let config = config(&["/data/runs"], "cto", &[]);
    let report = report("cto", &["/data/runs", "glob:/docs/**"], &config);
    assert_eq!(
        statuses(&report),
        vec![GrantStatus::Granted, GrantStatus::NotGranted]
    );
    assert_eq!(report.summary(), "2 declared, 1 granted");
    assert_eq!(report.ungranted(), vec!["glob:/docs/**"]);
}

/// A per-agent grant counts, and only for that agent.
#[test]
fn a_per_agent_grant_applies_to_that_agent_alone() {
    let config = config(&[], "cto", &["/data/runs"]);
    assert_eq!(
        statuses(&report("cto", &["/data/runs"], &config)),
        vec![GrantStatus::Granted]
    );
    assert_eq!(
        statuses(&report("worker", &["/data/runs"], &config)),
        vec![GrantStatus::NotGranted]
    );
}

/// A grant does not have to be written the same way as the declaration: an
/// exact directory grant covers a glob under it, which is exactly how the
/// runtime behaves.
#[test]
fn a_grant_covers_a_declaration_written_differently() {
    let config = config(&["/home/me/design-docs"], "cto", &[]);
    assert_eq!(
        statuses(&report("cto", &["glob:~/design-docs/**"], &config)),
        vec![GrantStatus::Granted]
    );
    assert_eq!(
        statuses(&report("cto", &["regex:~/design-docs/.*"], &config)),
        vec![GrantStatus::Granted]
    );
}

/// A grant for a neighbouring directory must not read as covering it.
#[test]
fn a_grant_for_a_sibling_directory_does_not_count() {
    let config = config(&["/data/runs"], "cto", &[]);
    assert_eq!(
        statuses(&report("cto", &["/data/runs-archive"], &config)),
        vec![GrantStatus::NotGranted]
    );
}

/// No representative path can be built from a character class, so the verdict
/// is "cannot tell" - and an entry nobody can judge is not offered up for
/// granting.
#[test]
fn an_uncheckable_entry_is_undetermined_not_ungranted() {
    let config = config(&["/data/runs"], "cto", &[]);
    let report = report("cto", &["glob:/docs/[ab]/**"], &config);
    assert_eq!(statuses(&report), vec![GrantStatus::Undetermined]);
    assert!(report.ungranted().is_empty());
    assert!(!report.has_ungranted());
    assert_eq!(report.summary(), "1 declared, 0 granted");
}

/// A declaration that needs a home directory nobody can resolve is not a
/// grant problem, so it is undetermined rather than refused.
#[test]
fn a_declaration_that_cannot_compile_here_is_undetermined() {
    let config = config(&["/data/runs"], "cto", &[]);
    let status = entry_status(
        "~/docs",
        &leviath_core::ReadPathSet::compile(
            &["/data/runs".to_string()],
            std::path::Path::new("/work"),
            None,
            false,
        )
        .expect("grant compiles"),
        false,
        std::path::Path::new("/work"),
        None,
        false,
    );
    assert_eq!(status, GrantStatus::Undetermined);
    // The config itself is fine; it is the declaration that could not resolve.
    assert_eq!(config.security.read_paths, vec!["/data/runs".to_string()]);
}

/// The user's own grant list is compiled too, and a broken one is worth
/// saying out loud: it is a hard error at spawn.
#[test]
fn a_malformed_config_grant_is_an_error() {
    let config = config(&["regex:relative/.*"], "cto", &[]);
    let err = report_entries(
        "cto",
        &["/data/runs".to_string()],
        &config,
        std::path::Path::new("/work"),
        Some(std::path::Path::new("/home/me")),
        false,
    )
    .expect_err("a relative regex grant cannot compile");
    assert!(err.contains("grant in your config.toml"), "{err}");
}

// -- rendering ---------------------------------------------------------

#[test]
fn the_report_block_lists_each_entry_and_the_stanza_to_paste() {
    let config = config(&["/data/runs"], "cto", &[]);
    let lines = report("cto", &["/data/runs", "glob:/docs/**"], &config).report_lines("  ", "⚠ ");
    let joined = lines.join("\n");
    assert!(joined.contains("2 declared, 1 granted"), "{joined}");
    assert!(joined.contains("/data/runs: granted"), "{joined}");
    assert!(joined.contains("glob:/docs/**: NOT granted"), "{joined}");
    assert!(joined.contains("[agent_read_paths.cto]"), "{joined}");
    assert!(joined.contains(r#"allow = ["glob:/docs/**"]"#), "{joined}");
    assert!(lines.iter().all(|l| l.starts_with("  ")), "{joined}");
}

#[test]
fn a_fully_granted_report_offers_no_stanza() {
    let config = config(&["/data/runs"], "cto", &[]);
    let report = report("cto", &["/data/runs"], &config);
    assert!(report.grant_stanza().is_empty());
    assert!(report.warning_line().is_none());
    let joined = report.report_lines("", "").join("\n");
    assert!(!joined.contains("Add to your config.toml"), "{joined}");
}

#[test]
fn the_blanket_override_is_named_in_the_report_block() {
    let mut config = Config::default();
    config.security.allow_blueprint_read_paths = true;
    let joined = report("cto", &["/data/runs"], &config)
        .report_lines("", "")
        .join("\n");
    assert!(
        joined.contains("allow_blueprint_read_paths = true"),
        "{joined}"
    );
}

#[test]
fn an_undetermined_entry_says_so_in_the_report_block() {
    let config = config(&["/data/runs"], "cto", &[]);
    let joined = report("cto", &["glob:/docs/[ab]/**"], &config)
        .report_lines("", "")
        .join("\n");
    assert!(joined.contains("cannot be checked"), "{joined}");
}

#[test]
fn the_one_line_warning_names_the_agent_and_the_counts() {
    let warning = report("cto", &["/data/runs"], &Config::default())
        .warning_line()
        .expect("something is ungranted");
    assert!(warning.contains("agent 'cto'"), "{warning}");
    assert!(warning.contains("1 declared, 0 granted"), "{warning}");
    assert!(warning.contains("will be refused"), "{warning}");
}
