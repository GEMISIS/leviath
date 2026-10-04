//! The `README.md` at the top of every bundle: what it is, what is in it,
//! and how to use it.
//!
//! Written for whoever opens the zip next, which is as often an agent as a
//! person, so it names every file and says how to put a run back where
//! Leviath's own tools can read it.

use super::About;
use super::collect::{Bundle, Section};

/// The line every bundle carries about what it does and does not hold.
pub(crate) const PRIVACY_WARNING: &str = "This file holds your task text, the model's replies, tool \
output, the contents of files the agent read, file paths, and your blueprints. API keys, OAuth \
tokens, header values and other credentials were removed. Nothing else was. Open it and read it \
before you upload it anywhere public, and do not upload it if anything in it must stay private.";

/// Build the README.
pub(crate) fn readme(
    about: About,
    note: &str,
    run_id: Option<&str>,
    version: &str,
    build: &str,
    created_at: &str,
    bundle: &Bundle,
) -> String {
    let mut out = String::new();
    out.push_str("# Leviath bug report bundle\n\n");
    out.push_str(&format!(
        "Made by `lev rage` on {created_at} with Leviath {version} (build {build}).\n\n"
    ));
    out.push_str(&format!("**About:** {}\n\n", about.describe()));
    if let Some(id) = run_id {
        out.push_str(&format!("**Run:** `{id}`\n\n"));
    }
    out.push_str("## What happened\n\n");
    if note.trim().is_empty() {
        out.push_str("_No description was given._\n\n");
    } else {
        out.push_str(note.trim());
        out.push_str("\n\n");
    }

    out.push_str("## Before you share this\n\n");
    out.push_str(PRIVACY_WARNING);
    out.push_str("\n\n");

    out.push_str("## What is in here\n\n");
    out.push_str("| Path | What it is |\n|---|---|\n");
    for (path, what) in LAYOUT {
        out.push_str(&format!("| `{path}` | {what} |\n"));
    }
    out.push('\n');
    out.push_str("Sections in this bundle:\n\n");
    out.push_str("| Section | Files | Bytes | Redactions |\n|---|---|---|---|\n");
    for Section {
        name,
        files,
        bytes,
        redactions,
    } in bundle.sections()
    {
        out.push_str(&format!(
            "| `{name}` | {files} | {bytes} | {redactions} |\n"
        ));
    }
    if !bundle.skipped.is_empty() {
        out.push_str("\nLeft out, and why:\n\n");
        for skipped in &bundle.skipped {
            out.push_str(&format!("- `{}`: {}\n", skipped.path, skipped.reason));
        }
    }
    out.push('\n');

    out.push_str("## How to read a run\n\n");
    out.push_str(
        "`runs/<id>/run.lvr` is the run's file, rewritten with its secrets removed. To read it, \
         copy `runs/<id>` into the `runs` directory of a Leviath home. A home of its own keeps it \
         apart from your runs: point `LEVIATH_HOME` at an empty directory, such as \
         `/tmp/rage-home`, in the shell you run these in. No daemon has to be running.\n\n",
    );
    out.push_str(concat!(
        "```bash\n",
        "mkdir -p /tmp/rage-home/.leviath/runs\n",
        "cp -R runs/<id> /tmp/rage-home/.leviath/runs/\n",
        "lev run show <id>     # the spec it was resolved to\n",
        "lev timeline <id>     # where the time went\n",
        "lev stages <id>       # per-stage tokens and cost\n",
        "lev context <id>      # the context window\n",
        "lev result <id>       # what the run handed back\n",
        "```\n\n",
    ));
    out.push_str(
        "`runs/<id>/run.json` holds the same values as JSON. `spec` is what the run was resolved \
         to before it started: its graph, inputs, models and tools. `start` is the state it \
         started in, `state` the state after its last step, and `steps` every step between, each \
         with what changed and what happened in it: model calls, tool calls, answers and \
         messages. `runs/<id>/summary.json` is the run's summary, and `runs/<id>/stages/` holds \
         each stage's output and log lines.\n\n",
    );
    out.push_str(
        "To run it again, install the blueprint that ran and send the request it was started \
         with. `runs/<id>/blueprint/` is that blueprint as it was on disk, and \
         `runs/<id>/request.json` names it with the same inputs, model and output. The request \
         is attended and has no webhook, and it works in the directory you start it from:\n\n",
    );
    out.push_str(
        "```bash\nlev add runs/<id>/blueprint\nlev run --request runs/<id>/request.json\n```\n\n\
         A run whose graph was sent whole has that graph in its request, so there is nothing to \
         install. An input that named an attached file needs that file attached again.\n\n",
    );

    out.push_str("## Redactions\n\n");
    out.push_str(
        "Every key the config held, every credential-shaped environment variable, and every \
         token-shaped string (`sk-...`, `AKIA...`, bearer headers, private-key blocks, JWTs) was \
         replaced with `[REDACTED]` or `[REDACTED:<kind>]`. `manifest.json` counts them per file. \
         A run's `callback_secret` is blanked. `control.token`, `mcp-auth.json`, \
         `provider-auth.json`, `.env` files and other tools' configs are never copied.\n",
    );
    out
}

/// What each top-level path holds, for the README table.
const LAYOUT: &[(&str, &str)] = &[
    ("README.md", "This file"),
    (
        "manifest.json",
        "Every member with its size and redaction count, and what was left out",
    ),
    (
        "environment.json",
        "Leviath version and build, OS, install method, which env vars are set (names only)",
    ),
    (
        "doctor.json",
        "`lev doctor --offline`: the config check and model resolution",
    ),
    (
        "daemon.json",
        "Whether the daemon runs, its pid and build, and its run list when it answered",
    ),
    (
        "config/",
        "`config.toml` and its siblings with every key removed; `policy.toml` and rules",
    ),
    ("agents/", "Every installed blueprint"),
    ("tools/, providers/", "Drop-in Rhai scripts"),
    (
        "logs/",
        "`daemon.log`, `daemon.stdio.log`, each `serve-<name>.log` and `dashboard.log`, with their rolled copies",
    ),
    (
        "runs/<id>/",
        "The chosen run and its sub-agent runs: run file, the same as JSON, the request that starts it again, summary, stage logs, blobs, blueprint",
    ),
    (
        "blueprint/",
        "The blueprint being built, with `blueprint-check.json` saying whether it parses",
    ),
    (
        "setup/imports.json",
        "Which other tools' config files exist on this machine (paths only)",
    ),
];

impl About {
    /// The category as a sentence fragment for the README and the summary.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            About::Setup => "setting Leviath up (keys, providers, the wizard)",
            About::Run => "a run that failed, hung or misbehaved",
            About::Agent => "building a blueprint",
            About::Other => "something else",
        }
    }
}
