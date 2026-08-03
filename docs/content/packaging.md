---
title: Packaging blueprints
group: Guides
group_order: 4
order: 4
---

# Packaging & sharing blueprints

A [blueprint](/docs/agents) is a directory with an `agent.leviath` file. To hand one to
someone else you bundle that directory into a single `.leviath-bundle` file, or share the
directory as-is. There is no hosted registry, and installation is always from a local path. `lev`
gives you four commands for the round trip: `pack` to build a bundle, `add` to install one,
`list` to see what's installed, and `remove` to take it away.

## Pack a blueprint

`lev pack` bundles a blueprint project into a distributable `.leviath-bundle` archive (a
gzip-compressed tarball of the blueprint, scripts, tests, and docs).

```bash
lev pack                              # pack the current directory
lev pack ./my-agent                   # pack a specific project directory
lev pack ./my-agent -o my-agent.leviath-bundle   # choose the output path
```

- `PATH` (optional positional): the project directory, or a path to an `agent.leviath` file.
  Defaults to the current directory.
- `-o`, `--output <FILE>`: where to write the bundle. Defaults to
  `{name}-{version}.leviath-bundle`, taken from the blueprint.

Credential-shaped files (`.env*`, `*.key`, `*.pem`, `id_rsa*`, `.ssh`, `config.toml`, and more)
are excluded from the archive automatically, so a stray secret in the project directory is not
shipped.

## Install a blueprint

`lev add` installs a blueprint into your local agents directory, `~/.leviath/agents/`. It
accepts either a bundle file or a plain agent directory.

```bash
lev add ./my-agent.leviath-bundle     # install from a bundle
lev add ./my-agent                    # install from a directory
```

- `PACKAGE` (required positional): a path to a `.leviath-bundle` file or to an agent
  directory. Anything else is rejected; `lev add` never reaches out to a network.

Installing under a name that already exists replaces the previous install.

When a blueprint asks for anything unusual, `lev add` prints an inventory of exactly what it wants
so you can look before running it. Unusual means pre-approved tools, script host access, a disabled
sandbox, or a command that runs at startup.

> [!WARNING]
> A blueprint can carry executable `.rhai` tool scripts, grant its own tool permissions, and
> declare seed commands that run at spawn, before the first prompt. Treat a third-party
> blueprint like any other code you install: read its [`agent.leviath`](/docs/agents) and the capability
> inventory `lev add` prints, and inspect it with `lev validate <name>`. See
> [Security](/docs/security) for the full trust model.

## List installed blueprints

`lev list` shows what you can run: installed blueprints from `~/.leviath/agents/`, a blueprint
in the current directory, blueprints from configured paths, and the bundled catalog.

```bash
lev list                              # everything
lev list --filter agents              # narrow the listing
```

- `-f`, `--filter <all|agents|blueprints>`: filter the output. Defaults to `all`.

## Remove a blueprint

`lev remove` deletes an installed blueprint from `~/.leviath/agents/`.

```bash
lev remove my-agent
```

- `NAME` (required positional): the installed blueprint's name (as shown by `lev list`).

## Run an installed blueprint

Once installed, run a blueprint by name, no path needed:

```bash
lev run my-agent --task "Your task here"
```

`lev run` hands the agent to the background [daemon](/docs/daemon), the same as running a local
project directory.

> [!IMPORTANT]
> An installed blueprint can only ever **tighten** its sandbox, never loosen it. The permission
> floor your own config sets always wins. Installing an agent does not let it grant itself more
> than you allow. See [Security](/docs/security) for how the sandbox and permission floor work.

See the [CLI reference](/docs/cli) for every command and flag.
