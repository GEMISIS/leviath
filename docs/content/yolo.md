---
title: Yolo profiles
description: Name a set of approval rules in yolo.toml so an unattended run builds and tests on its own, and still stops before anything risky.
group: Reference
group_order: 3
order: 4
---

# Yolo profiles

An unattended run stops at every tool call that needs approval, and waits for a person who is
not there. Bare `--yolo` fixes that by approving every call the config does not deny. That is a
single bit, and it is blunt for a run let loose on a real repository.

A yolo profile is that bit taken apart. You give a set of approval rules a name in `yolo.toml`,
then pass the name when you launch. Some calls run unprompted, some still raise the ordinary
approval prompt, and some are refused outright.

```bash
lev yolo init                     # write an example yolo.toml to start from
lev run coder --yolo=careful      # run under the profile named "careful"
```

The equals sign is required. `lev run --yolo careful` still means "run the agent `careful`
under bare `--yolo`", so the name has to attach to the flag.

> [!TIP]
> Want a walkthrough rather than a field list? See
> [Write your first yolo profile](/docs/first-yolo-profile).

## Where the file lives

`yolo.toml` sits beside `config.toml`, under the data root. When `LEVIATH_CONFIG_PATH` points
somewhere else, `yolo.toml` follows it. `lev yolo init` writes the file and tells you the path.

Each top-level table is a name you can pass to `--yolo=`. `default` is reserved, because bare
`--yolo` is not configurable. Names are letters, digits, `_` and `-`.

## What a profile looks like

This is the example `lev yolo init` writes, with its comments trimmed. The
[live copy](/schema/yolo.example.toml) is the one the tests check.

```toml
[careful]
default     = "ask"     # allow | ask: a call no rule below names, when the config would ask
questions   = "ask"     # ask | auto: ask_user_*, present_for_review, edit_document
checkpoints = "ask"     # ask | auto: stage checkpoints
gate        = "auto"    # ask | auto: taint-gate prompts, where taint tracking is on

[careful.tools]
allow = ["@builtin"]
ask   = ["web_fetch", "install_global_tool"]
deny  = []

[[careful.shell.allow]]
command = "cargo *"                  # leading words, one glob per word

[[careful.shell.allow]]
command = "rm -r*"
args    = ["target/**", "-*"]        # every remaining word must match one of these

[[careful.shell.ask]]
command = "git push*"

[[careful.shell.deny]]
command = "curl"

[build-only]
default = "ask"
[build-only.tools]
allow = ["read_file", "read_files", "list_dir"]
[[build-only.shell.allow]]
command = "cargo build*"
```

## The keys

| Key | Values | What it decides |
| --- | --- | --- |
| `default` | `allow`, `ask` | A call no rule names, when the config would ask. Required |
| `tools` | `allow`, `ask`, `deny` lists | Verdicts by tool name, glob or group |
| `shell` | `allow`, `ask`, `deny` rule tables | Verdicts by shell command line |
| `questions` | `ask`, `auto` | Tools that wait on a person. Defaults to `auto` |
| `checkpoints` | `ask`, `auto` | Stage checkpoints. Defaults to `auto` |
| `gate` | `ask`, `auto` | Taint-gate prompts. Defaults to `auto` |

`default` has no fallback on purpose. It is the key that says what the profile is for. A file
that omits it fails at load, rather than quietly becoming bare `--yolo`.

There is no `deny` value for `default`. A profile that refused everything it did not list
would be a denylist by omission. A call you want refused goes in a `deny` list, where you can
read it.

## How a verdict is reached

Rules are checked in this order, and the first match wins.

| Order | Condition | Outcome |
| --- | --- | --- |
| 1 | The config denies the tool | Refused. No profile lifts a deny |
| 2 | A `tools` or `shell` `deny` entry matches | Refused |
| 3 | An `ask` entry matches | The ordinary prompt, unless `--allow` named the tool |
| 4 | An `allow` entry matches | Runs unprompted |
| 5 | The config already allows the tool | Runs unprompted |
| 6 | Nothing matched | The profile's `default` |

Two consequences are worth reading twice. Rows 2 and 3 sit above row 5, so an `ask` or `deny`
entry tightens even a tool the config allows. Row 3 yields to `--allow` on the command line,
because the flag is the most specific thing you said, but row 2 does not.

`lev yolo test` reports the verdict and names the rule behind it:

```bash
lev yolo test careful --tool shell --command "rm -r target"
```

## Tools lists

An entry in a `tools` list is one of three things. A tool name in any of its spellings, so
`bash` covers `shell`. A glob, when it contains `*`, `?` or `[`. Or one of the groups
`@builtin`, `@subagent`, `@scripts`, `@mcp` and `@all`, described in
[tool groups](/docs/tools#tool-groups).

A glob is the way to name a set of [MCP](/docs/mcp) tools, since those are always spelled
`<server>__<tool>`. A `tracker` server's tools all match `tracker__*`.

## Shell rules

Shell rules refine the `shell` tool, so one profile can run your build and still stop at a
push. Each `[[<name>.shell.<verdict>]]` entry takes a `command` and an optional `args`.

`command` is the leading words of the line, one glob per word. `rm -r*` covers `rm -rf`, and
`cargo *` covers every cargo subcommand. `args` is a list of globs that every remaining word
must satisfy. Leave `args` off to accept any remaining words, or set it to `[]` to accept none.

A word starting with `-` is matched as text. Every other word is treated as a path, and is
matched only as the path it really names. Leviath expands `~`, joins the word to the run's
workdir, folds `..`, and follows symlinks as far as the path exists. So `~/scratch/../.ssh`
does not pass a `~/scratch/**` pattern, and neither does a link pointing out of that tree. A
relative pattern means "under the workdir", and an absolute one means what it says.

A line is judged one command at a time and takes the verdict of its strictest command. So
`cargo test && curl x` is only as free as `curl`. A command no rule names takes the tool-level
verdict for `shell`. A command that redirects to a file also answers to whatever the profile
says about `write_file`.

Some lines cannot be matched by an `allow` rule at all, because their words do not decide what
runs:

| Line | Why no rule can let it through |
| --- | --- |
| `PATH=x cargo build` | It binds a variable in front of its program |
| `rm -r $DIR` | One of its words is an expansion, so its real target is unknown |
| `trap "..." EXIT`, `alias`, `function`, `eval`, `source` | The program is one no rule can name |
| ``cargo build `whoami` `` | The line cannot be parsed ahead of time |

The first three report that the rules cannot vouch for the command. The last reports that the
line cannot be read at all. Either way they take the tool-level `shell` verdict when the profile
has no shell rules. When it has any, they ask, because the rules cannot be checked and a person
can.

An explicit rule does not rescue them. A profile with `command = "trap"` in its `allow` list
still asks about `trap "..." EXIT`.

## The human knobs

`questions`, `checkpoints` and `gate` decide whether a mechanism reaches a person. All three
default to `auto`, because a profile is a kind of `--yolo` until it says otherwise.

`questions = "ask"` keeps the tools that wait on a person advertised, and their calls come to
you. Those are `ask_user_*`, `present_for_review` and `edit_document`. Under `auto` they are
not offered, and a stray call is answered for the model.

`checkpoints = "ask"` opens every stage checkpoint. Under `auto` they approve themselves,
apart from any the blueprint marks `unattended = "ask"`. `gate` is the same choice for
[taint-gate](/docs/security) prompts.

## When a profile is read

A profile is read when a run spawns under its name, and again when that run resumes. It is
never re-read mid-batch. An edit therefore reaches your next `lev run`, and a parked run you
`lev resume`, but never the run that is already going. A rule an agent somehow changed cannot
apply to the run that changed it.

`questions`, `checkpoints` and `gate` are decided when the run is built, so they reach the next
run only.

A name the file does not have fails the spawn before the daemon is asked, and lists the names
the file does have. Sub-agents, fan-out workers and recovered runs inherit the profile by name,
so a child of a `careful` run is `careful` rather than bare `--yolo`.

## What a profile can never loosen

A profile adds holds. It never removes one that is already there.

| Hold | Who sets it |
| --- | --- |
| A denied tool | `tool_permissions` in the config |
| `required_tools` | The blueprint |
| `unattended = "ask"` | The blueprint, per interaction point |

With `[security] lock_permission_files` on, which is the default, no run's tools may write
`yolo.toml`. See [configuration](/docs/configuration) for that setting.

## The other surfaces

| Command | What it does |
| --- | --- |
| `lev yolo list` | Every profile in the file, with a summary of each |
| `lev yolo show <name>` | One profile as TOML, plus what it holds |
| `lev yolo test <name>` | The verdict for a call, and the rule behind it |
| `lev yolo init` | Write the example file |

Full flags are in the [CLI reference](/docs/cli#lev-yolo). The same four questions are on the
[API](/docs/api#yolo-profiles).

On the new-run screen in [`lev dash`](/docs/dashboard), `Ctrl-Y` steps through off, plain
`--yolo`, then each profile in file order, then off again. Switching it on asks you to confirm,
because that step is the one that removes prompts. Every step after it adds them back, so none
of those confirm.
