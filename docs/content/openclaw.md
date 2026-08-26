---
title: OpenClaw
description: Wiring Leviath into OpenClaw as a custom acpx harness, so a gateway agent can hand one task to a multi-stage run.
group: Integrations
group_order: 5
order: 3
---

# Running Leviath from OpenClaw

[OpenClaw](https://docs.openclaw.ai/) is an open-source gateway agent. It runs as a persistent
daemon, connects to messaging platforms like Slack, Discord, and Telegram, keeps memory across
sessions, and schedules recurring work. Those are the parts Leviath does not do.

OpenClaw also runs external coding harnesses over the
[Agent Client Protocol](/docs/agent-client-protocol), which Leviath speaks. So this is a
configuration change on the OpenClaw side, not an adapter you have to write.

## The vocabulary

Three OpenClaw words show up below. The **gateway** is the single process everything flows through.
A **harness** is an external coding agent OpenClaw drives instead of answering in its own loop.
**acpx** is the plugin that talks to those harnesses. OpenClaw's
[harness documentation](https://docs.openclaw.ai/tools/acp-agents) covers all three properly.

## Register Leviath with acpx

acpx keeps its own registry of harnesses in `~/.acpx/config.json`. Add Leviath to it:

```json
{
  "agents": {
    "leviath": {
      "command": "lev",
      "args": ["agent-client", "--agent", "coder"]
    }
  }
}
```

Per acpx's [custom agents guide](https://github.com/openclaw/acpx/blob/main/docs/custom-agents.md),
a name you define wins over the built-in registry, so `leviath` is yours to claim. A repository can
override the same map in its own `.acpxrc.json`.

## Let OpenClaw use it

OpenClaw reads `~/.openclaw/openclaw.json`, in JSON5, so comments and trailing commas are fine. The
`acp` block decides which harnesses are reachable at all:

```json5
{
  acp: {
    enabled: true,
    backend: "acpx",
    defaultAgent: "leviath",
    allowedAgents: ["leviath"],
  },
}
```

`allowedAgents` is an allowlist. A name missing from it is refused even when acpx knows it. Then
point an OpenClaw agent at that runtime:

```json5
{
  agents: {
    entries: {
      builder: {
        runtime: {
          type: "acp",
          acp: { agent: "leviath" },
        },
      },
    },
  },
}
```

`runtime.acp.cwd` sets the directory the session opens in. Leviath takes it as the run's working
directory, so it is also what the file tools are confined to.

## Approvals have somewhere to go here

By default Leviath asks before a tool call that changes something. Where that question goes depends
on what the host advertised during the protocol's `initialize` handshake.

acpx answers it. Its [permissions guide](https://github.com/openclaw/acpx/blob/main/docs/permissions.md)
gives three modes. `--approve-reads` auto-approves reads and prompts for everything else, and is the
default. `--approve-all` approves everything. `--deny-all` refuses everything.

The default is the one to plan around. Nobody is sitting at a prompt during a gateway turn, so an
escalated request is denied for that turn. A run that wanted to write a file gets a no and carries
on without it. acpx exits with code 5 when every request was denied and none approved, which is how
you spot it from outside.

The fix that travels with your config is on the Leviath side, because it lives in the `args` you
already declared. Name the tools you are happy to run unattended:

```json
{
  "agents": {
    "leviath": {
      "command": "lev",
      "args": [
        "agent-client", "--agent", "coder",
        "--allow", "read_file",
        "--allow", "list_dir",
        "--allow", "shell"
      ]
    }
  }
}
```

`--yolo` in place of those flags approves every tool call instead. Neither one can lift a `deny` in
your Leviath config, which stays authoritative. Running `acpx` yourself, `--approve-all` gets you
the same result from the other end.

[Security](/docs/security) covers the policy those flags sit on top of, and
[Interaction](/docs/interaction) covers what a parked question looks like from Leviath's side.

## Choosing which agent runs

`--agent coder` pins one blueprint for every session using that acpx entry. Declare one entry per
blueprint when you want the choice to be OpenClaw's:

```json
{
  "agents": {
    "leviath-coder": {
      "command": "lev",
      "args": ["agent-client", "--agent", "coder", "--yolo"]
    },
    "leviath-reviewer": {
      "command": "lev",
      "args": ["agent-client", "--agent", "reviewer", "--yolo"]
    }
  }
}
```

Add both names to `allowedAgents`, then set `runtime.acp.agent` per OpenClaw agent.

Drop `--agent` entirely and Leviath looks for an `agent.leviath` in the session's working directory
instead. That is the better default when different projects want different blueprints.

## Checking it works

Three steps, each proving one link in the chain. Run them in order, because a failure at step two
tells you nothing about step three.

```bash
lev agent-client --agent coder --yolo   # 1. does Leviath start
acpx leviath "list the files here"      # 2. does the acpx name resolve
```

Step one looks like a hang and is not one. The command is waiting for JSON-RPC on stdin, and its
diagnostics go to stderr, because stdout belongs to the protocol. Stop it with Ctrl-C.

Step three is a prompt from OpenClaw itself, sent to an agent whose `runtime.acp.agent` is
`leviath`. Output streams back as the run progresses.

If a turn connects and then goes quiet, the run is most likely parked on a tool approval. Answer it
with `lev respond`, or widen the approvals as described above.

## What Leviath does not bring

OpenClaw keeps doing what it is for, and Leviath does not duplicate it. Leviath has no view of your
channels, no memory that outlives a run, no scheduler, and no way to reach Slack or Telegram.
Sub-agents and fan-out happen inside one Leviath run, not across your gateway.

Adding Leviath pays off when the hard part is what happens inside a single task. If the hard part is
being reachable, remembering, and picking the moment, that is OpenClaw's job and it is better at it.
