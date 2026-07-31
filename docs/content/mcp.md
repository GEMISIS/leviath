---
title: MCP tool servers
group: Guides
order: 6
---

# MCP tool servers

Leviath connects to [Model Context Protocol](https://modelcontextprotocol.io) servers over stdio or
HTTP (streamable, with a legacy HTTP+SSE fallback), giving agents extra tools.

```bash
lev mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem /path
lev mcp list
lev mcp login <name>        # OAuth servers: opens your browser
lev mcp test <name>
lev mcp remove <name>
```

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

`lev mcp add` detects OAuth servers, binds tokens to the server origin (RFC 8414 issuer check,
HTTPS-only, capped redirects), and stores them in `~/.leviath/mcp-auth.json` (`0600`), refreshing
non-interactively. Manage servers from the [dashboard](/docs/dashboard) with `m`, or over the
[API](/docs/api) under `/api/mcp/servers`.
