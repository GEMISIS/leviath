---
title: MCP tool servers
description: Connect Leviath to Model Context Protocol servers over stdio or HTTP, giving agents tools beyond the built-ins.
group: Get started
group_order: 1
order: 7
---

# MCP tool servers

Leviath connects to [Model Context Protocol](https://modelcontextprotocol.io) servers over stdio or
HTTP (streamable, with a legacy HTTP+SSE fallback), giving agents extra tools beyond the built-ins.

```mermaid
flowchart LR
  subgraph D["Daemon"]
    A["agent"]
  end
  A -->|tool call| B["MCP broker"]
  B -->|stdio| S1["filesystem<br/>(npx server)"]
  B -->|HTTP| S2["remote<br/>(mcp.example.com)"]
```

## Managing servers

```bash
lev mcp add filesystem --command npx \
  --arg -y --arg @modelcontextprotocol/server-filesystem --arg /path
lev mcp add remote --url https://mcp.example.com --header "Authorization=Bearer $TOK"
lev mcp list
lev mcp login <name>        # OAuth servers: opens your browser
lev mcp logout <name>       # drop the stored OAuth tokens
lev mcp test <name>
lev mcp remove <name>
```

`lev mcp add <name>` takes `--command` + repeatable `--arg` for a stdio server, or `--url`
(with optional `--header`/`--env`) for an HTTP one; `--no-login` skips the OAuth handshake.

`--header` and `--env` both want `KEY=VALUE`, split on the first `=`. Note that this is not the
`Name: value` form an HTTP header is usually written in, so `Authorization: Bearer ...` is rejected
with `--header must be KEY=VALUE`.

`--arg` passes its value through to the server's own command line, so an argument of its own that
starts with `-` is fine: `--arg -y` is the `-y` that `npx` wants, not a flag of ours.

There are two ways an HTTP server authenticates you, and Leviath picks between them by asking the
server rather than by guessing. If a `--header` you configured is enough, as it is for a server that
takes an API token of its own, `add` reports that no login is needed and stores nothing. If the
server answers with a `401` instead, the OAuth flow runs and the tokens land in the credential
store. `lev mcp login` on an already-satisfied server says so rather than failing.

That question is asked with the headers as they will actually be sent, `${VAR}` references
expanded, so a credential that comes from the environment is recognised as the credential it is.

> [!NOTE]
> GitHub's MCP server accepts either. A personal access token in an `Authorization` header needs no
> login at all, and the same endpoint runs the browser flow if you configure no header.

Or configure in `~/.leviath/config.toml`:

```toml
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path"]

[[mcp_servers]]
name = "remote"
url = "https://mcp.example.com"
headers = { Authorization = "Bearer ${MY_TOKEN}" }   # ${VAR} is expanded
```

Either way, the next run picks it up. The daemon watches `config.toml`, so a server you add, edit
or remove takes effect without `lev daemon restart` - see
[the daemon docs](/docs/daemon#config-changes-take-effect-on-the-next-run).

## Discovery and invocation

On connect, Leviath discovers the server's tools and exposes them to any stage whose
`available_tools` includes them.

### How an MCP tool is named

Always `<server>__<tool>`: the name you gave the server, two underscores, the name the server gives
the tool. A `github` server offering `create_issue` is advertised as `github__create_issue`.

The server is always part of the name, not only when something would clash. Two servers that both
offer `search` become `github__search` and `gitlab__search`, so a grant says which one it means and
keeps meaning that however your `config.toml` is ordered.

> [!NOTE]
> The separator is `__`, not a dot. The advertised name goes to the model provider, which accepts
> only `[A-Za-z0-9_-]` and rejects the whole request otherwise - so any other character, a dot
> included, is rewritten to `_`. A server named `my.tools` offering `find.all` is advertised as
> `my_tools__find_all`. In the rare case two servers' names sanitize to the same string, the second
> gets a `_2` suffix.

Calls route back to the owning server under the tool's original name, so the server never sees the
qualified form.

Tools used to be advertised bare, with the server prefixed only on a clash, so a blueprint written
against that naming grants `create_issue` where the tool is now `github__create_issue`. Such a grant
still resolves, **as long as exactly one server offers a tool by that name**. Two do and the name is
genuinely ambiguous: it resolves to nothing and the manifest has to say which. Worth updating the
manifest either way, since the ambiguity can arrive later when somebody adds a second server.

A built-in is never captured this way - `read_file` matches the built-in, whatever any server calls
its own tools.



```mermaid
sequenceDiagram
  participant Agent
  participant Broker as MCP broker
  participant Server as MCP server
  Broker->>Server: initialize + list tools
  Server-->>Broker: tool schemas
  Agent->>Broker: call tool(args)
  Broker->>Server: invoke
  Server-->>Broker: result
  Broker-->>Agent: routed to a context region
```

## Granting a whole server

`available_tools` is an exact-match list, so granting a server tool by tool means knowing what it
advertises - and that is not yours to know. It is whatever the server ships today. GitHub's server
has dozens; a house server gains one when somebody deploys. A tool added later is never
offered, and nothing says so, so the stage quietly cannot do a thing you believed it could.

Name the server instead:

```toml
[stages.triage]
available_tools = ["read_file", "gitlab__create_issue"]
available_connectors = ["github"]
```

That stage gets the built-in `read_file`, one named tool from `gitlab`, and everything `github`
advertises. The two forms mix freely, and a tool named individually *and* covered by a connector is
granted once.

The connector is resolved at spawn against what the server actually advertises then, and merged
with `available_tools`, so the two mix freely. A tool the server gains next month is offered
without touching the manifest.

A connector that resolves to nothing - the server is not installed, or did not connect this run -
grants nothing, exactly as an `available_tools` name matching nothing does. Whether a server is
present is not a property of your blueprint, so `lev validate` says nothing about connector names
either, the same way it never reports an MCP tool as unknown.

Everything else is unchanged. Connector-granted tools are ordinary tools from there on: they go
through the same `tool_permissions`, the same taint gate, and the same approval prompts as a tool
you named by hand.

> [!NOTE]
> There is no wildcard form of `available_tools`, and a connector grant is not sugar for one.
> Names are server-qualified, so `github__*` would *usually* work - but not reliably enough to
> build on: a server named `my.tools` sanitizes to `my_tools`, and a name collision appends `_2`,
> so matching the string is a guess where the connector grant is a fact. `available_connectors`
> asks Leviath which tools a server owns rather than inferring it from how they are spelled.

## OAuth, safely

`lev mcp add` detects OAuth servers, binds tokens to the server origin (RFC 8414 issuer check,
HTTPS-only, capped redirects), and stores them in `~/.leviath/mcp-auth.json` (`0600`), refreshing
non-interactively.

> [!NOTE]
> Manage servers from the [dashboard](/docs/dashboard) with `m`, or over the [API](/docs/api) under
> `/api/mcp/servers` (add/remove need `--allow-admin`).

## Serving Leviath as an MCP server

This page is about Leviath as an MCP client. The other direction exists too: `lev mcp serve`
speaks MCP over stdio so a host agent such as Claude Code, Grok, Codex, Gemini, or Hermes hands a
task to Leviath with a tool call, and `lev integrate <host>` registers it in the host and installs
a skill saying when to use it. The tools it exposes and the host-by-host setup are on
[Claude Code, Grok and other agents](/docs/host-agents); the flags are under
[`lev mcp serve`](/docs/cli#lev-mcp-serve).
