---
title: Security & sandboxing
group: Concepts
group_order: 2
order: 8
---

# Security: sandboxed execution and taint tracking

By default an agent's shell commands run directly on your machine. When you want isolation, opt in
per agent or per stage.

## Sandboxes

```toml
[sandbox]
kind    = "container"     # "container" | "namespace" | "none"
engine  = "docker"        # docker | podman | any Docker-CLI-compatible
image   = "debian:bookworm-slim"
network = false

[stages.analyze.sandbox]  # per-stage override
kind = "none"             # run discovery on the host…
```

- **Containers** (Docker/Podman): the daemon keeps a warm container per agent and tears it down at
  reap. Every capability is dropped, privilege regain is forbidden, processes and memory are bounded,
  and file tools keep working over the bind-mounted workdir.
- **Namespaces**: a lighter option with no container runtime; isolates PIDs and (with
  `network = false`) connectivity. It shares the host filesystem, so reach for a container when you
  want real containment.

> [!IMPORTANT]
> An *installed* agent can only ever **tighten** its sandbox: it can raise the walls, never lower
> them. A blueprint you install can't quietly turn isolation off.

## Reading outside the workdir

An agent's file tools are confined to its workdir. Some agents legitimately need to see more:
a planner that reads run archives, a reviewer that reads design docs kept next to the repo. For
that, a blueprint can declare extra read paths:

```toml
[read_paths]
allow = [
    "~/.leviath/runs",          # an exact path grants its whole subtree
    "../shared-docs",           # relative entries resolve against the run's workdir
    "glob:~/design-docs/**",    # glob patterns; * stays in one component, ** crosses
    "regex:/data/archives/.*",  # regex patterns, auto-anchored (^...$)
]
```

Declaring is not granting. The manifest travels with the agent package, and a package can only
tighten what your config allows. Declared paths stay inert until your `config.toml` grants them:

```toml
# Grant specific paths, for one agent...
[agent_read_paths.cto]
allow = ["~/.leviath/runs", "glob:~/design-docs/**"]

# ...or machine-wide, for any agent that declares them:
[security]
read_paths = ["~/.leviath/runs"]

# Or trust every blueprint's declarations wholesale (off by default):
[security]
allow_blueprint_read_paths = true
```

A grant applies to a path only when the running blueprint also declares it, so listing a
directory in your config does not open it to agents that never asked. When an agent declares
paths nothing grants, it still runs; the reads are refused and a spawn warning shows the exact
stanza to add. `lev add` and `lev validate` both report what an agent asks for.

The rules that keep this safe:

- **Read-only.** Only `read_file`, `read_files`, and `list_dir` can leave the workdir.
  `write_file` and `edit_file` are confined to the workdir no matter what is granted.
- **Symlinks cannot widen a grant.** Every access is resolved to its real path first, and the
  real path must match a declared and granted entry. A symlink planted inside a granted
  directory that points at `~/.ssh` is refused.
- **Patterns match the real path**, written with `/` on every OS (on Windows, matching is
  case-insensitive and the `\\?\` prefix is handled for you). On macOS note that `/tmp` is
  really `/private/tmp`; `~/` entries avoid the problem since the home directory is stable.
- **Regexes are anchored.** `regex:/data/runs` matches exactly that path, not
  `/data/runs-anything`; end a pattern with `/.*` to grant a subtree. A relative regex is
  refused; use `glob:` for workdir-relative patterns.
- **Taint rises.** When a grant is active, the read tools are classified `Private` for that
  agent, so taint tracking treats out-of-workdir content with more suspicion, not less.
- Rhai script tools have their own `read_file` and it stays workdir-confined; `[read_paths]`
  applies to the built-in file tools only.

Pick the run's workdir itself with `lev run <agent> --workdir <dir>` (defaults to the directory
you ran the command from).

## Taint tracking (experimental)

A deterministic sensitivity model (**Public / Internal / Private**) tags every
[context region](/docs/context), set by the runtime and never by model output. Any tool that can
carry bytes off the machine is gated: before it fires, the runtime checks the tool's clearance
against the sensitivity of the data in play.

```mermaid
flowchart TD
  C["Tool call<br/>(e.g. http_post)"] --> E{"Can it exfiltrate?"}
  E -->|no| RUN["Run"]
  E -->|yes| L{"Data taint ≤<br/>tool clearance?"}
  L -->|yes| RUN
  L -->|no| P{"Policy?"}
  P -->|allow| RUN
  P -->|deny| BLK["Blocked before it fires"]
  P -->|ask| Q["Prompt: allow once /<br/>for session / deny"]
```

Taint recovers as entries evict, and an unrecognized tool **fails closed**. Configure with a
`[security]` block, layer on allowlists and Rhai policy rules, and dry-run any tool:

```bash
lev policy list
lev policy add send_email --target "*.example.com" --max-sensitivity internal
lev policy test bash --target example.com
```

`lev policy add` and `lev policy test` take the tool name as a **positional** argument
(`lev policy add <tool> …`, `lev policy test <tool> --target …`).

## Threat model

`lev serve` runs LLM-driven tools, so treat it as trusted-network only unless hardened. See
[SECURITY.md](https://github.com/GEMISIS/leviath/blob/main/SECURITY.md) for the full threat
model, what Leviath defends against, and how to report a vulnerability (GitHub private advisories).
