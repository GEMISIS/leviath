---
title: Reporting issues
description: How to pack the logs and settings a bug report needs with `lev rage`, what the zip holds, what it never holds, and how to attach it to an issue.
group: Guides
group_order: 4
order: 4
---

# Reporting issues

Something went wrong and you want help. The useful evidence is spread over your config file, the
daemon's log, the run's journal and the blueprint that ran. Collecting it by hand is slow, and it
is easy to paste an API key by mistake. `lev rage` packs all of it into one zip and takes every
key out first.

```bash
lev rage
```

A small screen asks what the problem was about. For a run, it asks which one. Then it writes
`leviath-rage-<timestamp>.zip` in the current directory and shows what went in.

> [!CAUTION]
> The zip holds your task text, the model's replies, tool output, the contents of files the agent
> read, file paths, and your blueprints. API keys, OAuth tokens, header values and other credentials
> are removed. Nothing else is. Open the zip and read it before you upload it anywhere public. If
> anything in it must stay private, do not upload it.

## What the zip holds

| Path | What it is |
|---|---|
| `README.md` | What the bundle is, your note, and how to read a run from it |
| `manifest.json` | Every file with its size and how many secrets were removed, and what was left out |
| `environment.json` | Leviath version and build, OS, install method, and the names of set env vars |
| `doctor.json` | The output of `lev doctor --offline` |
| `daemon.json` | Whether the daemon runs, its pid and build, and its run list if it answered |
| `config/` | `config.toml` and its siblings with every key removed, plus `policy.toml` and rules |
| `agents/` | Every installed blueprint |
| `tools/`, `providers/` | Your drop-in Rhai scripts |
| `logs/` | `daemon.log`, `daemon.stdio.log`, each `serve-<name>.log` and `dashboard.log`, with their rolled copies |
| `runs/<id>/` | The run you picked and its sub-agent runs: metadata, stages, context, journal, media, blueprint |
| `blueprint/` | The blueprint you were building, with a check that says whether it parses |
| `setup/imports.json` | Which other tools' config files exist on this machine, by path only |

The last three depend on what you said the problem was about. Everything else is always in.

## What the zip never holds

These files are never copied, whatever else is in the bundle:

- `control.token`, the daemon's live credential
- `mcp-auth.json` and `provider-auth.json`, the OAuth token stores
- `.env` files
- Other tools' config files, such as Claude Code's or Cursor's

Every key in your config is replaced with `<redacted>`. The names of headers stay, so a helper can
see which gateway you use, but their values go. Every text file is then searched for the values
Leviath knows to be secrets and for anything shaped like a token, such as `sk-...`, `AKIA...`, a
bearer header, a private key block or a JWT. Each hit becomes `[REDACTED]`. `manifest.json`
counts them per file, so you can see where they were.

## Attaching it to an issue

`lev rage` never uploads anything. When you have read the zip and are happy to share it:

1. Open [a new issue](https://github.com/GEMISIS/leviath/issues/new/choose) and pick **Bug report**.
2. Say what happened and what you expected. The note you typed into `lev rage` is in the zip's
   `README.md`, so you can paste it from there.
3. Drag the zip onto the text box. GitHub uploads it and adds a link.
4. Paste the `environment.json` version and platform lines into the form's fields.

Do not paste the config or the logs into the issue by hand. The zip already has them with the
keys taken out, and a hand copy is how a key ends up on the web.

## Flags

Every question on the screen has a flag, and `--non-interactive` skips the screen altogether. A
stdout that is not a terminal skips it too, so `lev rage` works from a script.

| Flag | Purpose |
|---|---|
| `--about <setup\|run\|agent\|other>` | What the problem was about. Answers the first question |
| `--run <RUN_ID>` | The run it happened in: an exact id, or a prefix only one run starts with. Implies `--about run` |
| `--agent <PATH>` | The blueprint you were building: its directory or its `agent.leviath`. Implies `--about agent` |
| `--note <TEXT>` | What happened, in your words. Lands at the top of the zip's README |
| `-o`, `--output <PATH>` | Where to write the zip. Default: `./leviath-rage-<timestamp>.zip` |
| `--no-blobs` | Leave a run's stored media parts out |
| `--non-interactive` | No screen: build the zip from the flags and print its path |

```bash
lev rage --run abc123 --note "the review stage never finished" -o report.zip
lev rage --about setup --non-interactive
```

## Where the logs come from

The daemon writes its log to `~/.leviath/daemon.log`, however it was started, and each
`lev serve` writes `~/.leviath/serve-<name>.log`. Both are capped by
`[observability] log_file_max_bytes` (5 MiB by default) and roll once to `<name>.1`.
See [the daemon page](/docs/daemon#where-it-logs). A run's own logs live under
`~/.leviath/runs/<id>/`, which is what the `runs/` section of the zip copies.

If you would rather look yourself first, start with [Troubleshooting](/docs/troubleshooting).
