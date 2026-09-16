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
        "Copy `runs/<id>` into `~/.leviath/runs/` on a machine with `lev` installed (or into \
         `$LEVIATH_HOME/.leviath/runs/` to keep it apart from your own runs). Then:\n\n",
    );
    out.push_str("```bash\nlev context <id>     # the context window at every step\nlev timeline <id>    # where the time went\nlev stages <id>      # per-stage tokens and cost\nlev result <id>      # what the run handed back\n```\n\n");
    out.push_str(
        "`run.lvr` is the run's journal: the bytes `LVR1`, a two-byte version, then frames of an \
         eight-byte big-endian length and a JSON record. The first record is the header with the \
         run's metadata; the rest are every inference request and reply, every tool call and its \
         result, and the context window as it changed. The copy here was re-written with its \
         secrets removed, so a JSON reader gets the same records `lev` does.\n\n",
    );
    out.push_str(
        "`runs/<id>/blueprint/` is the agent that ran, as it was on disk. `lev add <that dir>` \
         installs it, and `lev run <name> --task \"...\"` with the task from `meta.json` reproduces \
         the run, model differences aside.\n\n",
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
        "The chosen run and its sub-agent runs: metadata, stages, context, journal, blobs, blueprint",
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
