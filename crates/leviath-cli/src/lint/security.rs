//! The checks that read a manifest as a request for capability.
//!
//! Tool policies, command seeds, pre-approved safe commands, read-path grants
//! and held checkpoints: everything a blueprint can ask for that the user has
//! to decide about. These are the findings `lev validate` exists to surface
//! before a downloaded agent runs, not after.

use super::*;

/// Permissions that do not land on the tool they look like they land on, and
/// shell grants left to the default.
///
/// Policy is resolved against the name the *model* calls the tool by, which is
/// always the canonical one. A permission written under an alias of a tool the
/// stage granted canonically (or the reverse) is looked up under a key nothing
/// ever asks for, so the entry has no effect at all: it reads as a decision and
/// is not one.
pub(super) fn lint_tool_policies(
    stage: &leviath_core::Stage,
    agent_permissions: &HashMap<String, String>,
) -> Vec<LintFinding> {
    // Either spelling counts, because policy resolution accepts either: a stage
    // granting `bash` is covered by a `shell` permission and the reverse. This
    // used to warn about the mismatch, back when only the name as called was
    // looked up and every entry written under an alias was dead.
    let has_policy = |name: &str| {
        leviath_tools::tool_name_spellings(name)
            .any(|n| stage.tool_permissions.contains_key(n) || agent_permissions.contains_key(n))
    };

    stage
        .available_tools
        .iter()
        .filter(|t| !has_policy(t))
        // Only worth saying for the shell, whose default is `ask` - and an `ask`
        // with nobody to answer waits rather than denying, so an unattended run
        // hangs on the first command instead of failing it.
        .filter(|t| canonical_tool_name(t) == "shell")
        .map(|tool| {
            LintFinding::new(
                LintSeverity::Warning,
                "implicit-shell-policy",
                format!(
                    "grants '{tool}' with no permission set for it, so it \
                     defaults to ask - and an unattended run waits on that \
                     prompt rather than being denied"
                ),
            )
            .in_stage(&stage.name)
            .with_fix(format!(
                "set {tool} = \"allow\" or \"deny\" in [tool_permissions] or \
                 [stages.{}.tool_permissions]",
                stage.name
            ))
        })
        .collect()
}

/// Regions whose `seed = { command = "..." }` runs a shell command at spawn.
///
/// This one is an audit line rather than a complaint: the commands run before
/// the first inference and before any tool-approval prompt, so whoever is about
/// to `lev add` a blueprint they did not write should see them first.
pub(super) fn lint_command_seeds(blueprint: &Blueprint) -> Vec<LintFinding> {
    let seeds: Vec<String> = blueprint
        .context_layout
        .regions
        .iter()
        .filter_map(|r| match &r.seed {
            Some(leviath_core::layout::RegionSeed::Command { command }) => {
                // Whether it will actually run matters more than that it is
                // declared. A seed runs before any prompt exists, so it only
                // runs if the safe list already covers it - and finding that
                // out here is a one-line config fix, where finding out at spawn
                // is a region that silently came up empty.
                let verdict = match default_safe_keys_cover(command) {
                    true => "pre-approved",
                    false => "NOT pre-approved by the default safe list, so it will be refused",
                };
                Some(format!("{}: {command} ({verdict})", r.name))
            }
            _ => None,
        })
        .collect();
    if seeds.is_empty() {
        return Vec::new();
    }
    vec![
        LintFinding::new(
            LintSeverity::Note,
            "command-seed",
            format!(
                "{} region(s) run a shell command at spawn, before the first \
                 inference and before any tool-approval prompt: {}",
                seeds.len(),
                seeds.join(", ")
            ),
        )
        .with_fix(
            "a refused seed needs its programs in `[safe_commands] shell`; disable seeds \
             entirely with `--no-seed-commands`, or machine-wide via \
             `[security] allow_seed_commands = false`",
        ),
    ]
}

/// List the tools a blueprint calls at spawn.
///
/// The sibling of [`lint_command_seeds`], and an audit line for the same
/// reason: these run before the first inference and therefore before any
/// approval prompt, so somebody about to `lev add` a blueprint they did not
/// write should see them first.
///
/// Unlike a command seed there is no pre-approval to report. A seeded call
/// resolves against the same `[tool_permissions]` a mid-run call does, so the
/// question "will this run" has the same answer as "may this agent call it",
/// which the permission table already states.
pub(super) fn lint_tool_seeds(blueprint: &Blueprint) -> Vec<LintFinding> {
    let seeds: Vec<String> = blueprint
        .context_layout
        .regions
        .iter()
        .filter_map(|r| match &r.seed {
            Some(leviath_core::layout::RegionSeed::Tools { calls, refresh }) => {
                let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                // A seed that re-runs on every stage entry is worth saying so:
                // it is a tool call per stage for the life of the run, not one.
                let when = match refresh {
                    leviath_core::layout::SeedRefresh::Once => "",
                    leviath_core::layout::SeedRefresh::EachStage => " (on every stage entry)",
                };
                Some(format!("{}: {}{when}", r.name, names.join(", ")))
            }
            _ => None,
        })
        .collect();
    if seeds.is_empty() {
        return Vec::new();
    }
    vec![
        LintFinding::new(
            LintSeverity::Note,
            "tool-seed",
            format!(
                "{} region(s) call tools at spawn, before the first inference and \
                 before any tool-approval prompt: {}",
                seeds.len(),
                seeds.join("; ")
            ),
        )
        .with_fix(
            "each call answers to `[tool_permissions]` exactly as a mid-run call would; \
             a tool set to `ask` is refused at spawn because there is nobody to prompt",
        ),
    ]
}

/// Whether the *default* safe list covers `command`.
///
/// Reports against the shipped defaults rather than the reader's own config:
/// `lev validate` is most often run on a blueprint somebody is deciding whether
/// to install, and "would this run on a stock machine" is the question that
/// answers. A user who has added entries sees a false alarm, which is the safe
/// direction for an audit line.
pub(super) fn default_safe_keys_cover(command: &str) -> bool {
    let safe = crate::approvals::resolve_safe_keys(&Default::default(), None, None, false);
    let keys = crate::shell_keys::command_keys(command);
    !keys.is_empty()
        && keys
            .iter()
            .all(|k| safe.contains_key(k) || safe.contains_key(crate::shell_keys::program_of(k)))
}

/// Checkpoints that hold a `--yolo` run for a person.
///
/// A note, not a warning: this is the blueprint working as written. It is worth
/// saying because `--yolo` reads as "run without me", so a run that stops anyway
/// looks like a hang, and `lev validate` is where an author or an operator finds
/// out before the run rather than during it.
pub(super) fn lint_held_checkpoints(blueprint: &Blueprint) -> Vec<LintFinding> {
    let held = crate::held_checkpoints::held_points(blueprint)
        .into_iter()
        .map(|h| (h, "an interaction point that declares unattended = \"ask\""))
        .chain(
            crate::held_checkpoints::held_tools(blueprint)
                .into_iter()
                .map(|h| (h, "a blocking tool the stage keeps in required_tools")),
        );
    held.map(|(h, what)| {
        LintFinding::new(
            LintSeverity::Note,
            "holds-under-yolo",
            format!(
                "'{}' still stops an unattended run for a person: {what}",
                h.name
            ),
        )
        .in_stage(&h.stage)
        .with_fix(
            "this is deliberate if the checkpoint needs a person. `[limits] \
             interaction_timeout_secs` bounds the wait, and an unanswered \
             checkpoint stops the run with an error rather than approving it",
        )
    })
    .collect()
}

/// A `[safe_commands]` block: entries that will never match, and whether the
/// block counts at all on this install.
pub(super) fn lint_safe_commands(blueprint: &Blueprint, env: &LintEnv) -> Vec<LintFinding> {
    let Some(sc) = blueprint.safe_commands.as_ref() else {
        return Vec::new();
    };
    let mut findings = Vec::new();

    // An entry the key parser reads as anything other than itself can never
    // match a call, so it reads as a decision and is not one. An error rather
    // than a warning: unlike a permission written under an alias, there is no
    // spelling of this that does something else useful.
    for entry in &sc.shell {
        if !crate::shell_keys::is_valid_prefix(entry) {
            findings.push(
                LintFinding::new(
                    LintSeverity::Error,
                    "unparseable-safe-command",
                    format!(
                        "[safe_commands] shell entry '{entry}' is not a bare command prefix, so \
                         no call can ever match it"
                    ),
                )
                .with_fix(
                    "write a program, optionally with the subcommand that narrows it: \
                     'rg', 'cargo test', 'git status'. No flags, arguments, redirects, \
                     quotes or chained commands",
                ),
            );
        }
    }

    // Declaring is not granting, and an author who does not know that ships a
    // block that does nothing on every install but their own.
    if !sc.shell.is_empty() || !sc.tools.is_empty() {
        let message = match env.safe_commands_granted {
            Some(true) => None,
            Some(false) => Some(
                "declares [safe_commands], and your config does not honour blueprint \
                 safe-commands, so none of it applies"
                    .to_string(),
            ),
            None => Some(
                "declares [safe_commands]. Declaring is not granting: entries apply only \
                 where the user opts in"
                    .to_string(),
            ),
        };
        if let Some(message) = message {
            findings.push(
                LintFinding::new(LintSeverity::Note, "safe-commands-declared", message).with_fix(
                    format!(
                        "set [agent_safe_commands.{}] allow_blueprint = true in your \
                         config.toml, or [security] allow_blueprint_safe_commands for every \
                         agent",
                        blueprint.name
                    ),
                ),
            );
        }
    }

    findings
}

/// `[read_paths]` declarations: what the agent asks to read beyond its workdir,
/// whether this machine's config actually grants each entry, and a sharper
/// warning for an entry so broad it amounts to "my whole home directory" or
/// "any absolute path".
///
/// The grant status is the point (issue #209). A declaration is inert on its
/// own, and before this it took reading the config schema to find that out: the
/// blueprint validated, the run spawned, and the first out-of-workdir read was
/// refused with nothing said earlier. When `env` has no answer - the daemon's
/// offline lint, which has no user config to consult - the note falls back to
/// stating the rule.
pub(super) fn lint_read_paths(blueprint: &Blueprint, env: &LintEnv) -> Vec<LintFinding> {
    let Some(rp) = blueprint
        .read_paths
        .as_ref()
        .filter(|rp| !rp.allow.is_empty())
    else {
        return Vec::new();
    };
    let mut findings = match &env.read_paths {
        Some(Ok(report)) => grant_findings(report),
        // A grant list of the user's own that will not compile is a hard spawn
        // error; saying so here is where it costs least.
        Some(Err(e)) => vec![
            LintFinding::new(LintSeverity::Warning, "read-paths-grant-invalid", e.clone())
                .with_fix("fix the entry in your config.toml, or remove it"),
        ],
        None => vec![
            LintFinding::new(
                LintSeverity::Note,
                "read-paths-declared",
                format!(
                    "declares [read_paths] (reads outside the run workdir): {}",
                    rp.allow.join(", ")
                ),
            )
            .with_fix("these are refused unless your own config grants them"),
        ],
    };
    findings.extend(
        rp.allow
            .iter()
            .filter(|e| read_path_entry_is_broad(e))
            .map(|entry| {
                LintFinding::new(
                    LintSeverity::Warning,
                    "broad-read-path",
                    format!(
                        "read_paths entry '{entry}' is very broad - it can match \
                     your entire home directory or any path on this machine"
                    ),
                )
                .with_fix("name the directory it actually needs")
            }),
    );
    findings
}

/// One finding per declared entry, judged against the config: a note for the
/// ones that are live, a warning naming each one that is not, and the stanza
/// that would grant them all.
///
/// An entry whose pattern admits no representative path is reported as
/// unchecked rather than as inert - claiming a working grant is broken would be
/// worse than saying nothing.
pub(super) fn grant_findings(report: &crate::read_path_report::GrantReport) -> Vec<LintFinding> {
    let mut findings = vec![
        LintFinding::new(
            LintSeverity::Note,
            "read-paths-declared",
            format!(
                "declares [read_paths] (reads outside the run workdir): {}",
                report.summary()
            ),
        )
        .with_fix(match report.allow_blueprint {
            true => "all granted by [security] allow_blueprint_read_paths = true".to_string(),
            false => report
                .entries
                .iter()
                .map(|e| format!("{}: {}", e.raw, e.status.label()))
                .collect::<Vec<_>>()
                .join("; "),
        }),
    ];
    if report.has_ungranted() {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "read-paths-not-granted",
                format!(
                    "your config does not grant {}: reads matching them will be refused",
                    report.ungranted().join(", ")
                ),
            )
            .with_fix(format!(
                "add to your config.toml: {}",
                report.grant_stanza().join(" ")
            )),
        );
    }
    findings
}

/// Whether a `[read_paths]` entry grants effectively unlimited read access:
/// the home directory itself, a filesystem root, or a pattern whose first
/// component already matches anything.
pub(super) fn read_path_entry_is_broad(entry: &str) -> bool {
    let pattern = entry
        .strip_prefix("glob:")
        .or_else(|| entry.strip_prefix("regex:"))
        .unwrap_or(entry);
    let pattern = pattern.replace('\\', "/");
    let trimmed = pattern.trim_end_matches('/');
    matches!(trimmed, "~" | "")
        || trimmed == "/**"
        || pattern.starts_with("**")
        || pattern.starts_with("/.*")
        || trimmed == "/.+"
}
