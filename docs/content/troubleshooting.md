---
title: Troubleshooting
group: Guides
group_order: 4
order: 2
---

# Troubleshooting

Common snags and how to clear them.

## The console can't reach my server

The browser [console](/app) talks straight to your `lev serve` endpoint, so three things must line
up:

1. **The server is running** with a token: `lev serve --token <t>`.
2. **CORS allows the site's origin**: add `--cors https://leviath.dev` (or `--cors "*"`). Without
   it, the browser blocks the request before your server ever sees it.
3. **The token matches** the one you entered in the console.

```bash
lev serve --token 6618… --cors https://leviath.dev --allow-admin
```

> [!WARNING]
> A page served over **https** can't call an **http** endpoint (mixed content). `http://127.0.0.1`
> is exempt, so localhost works; for a remote box use TLS, an SSH tunnel, or the Docker image.

## I get `401 Unauthorized`

The token is missing or wrong. REST clients send `Authorization: Bearer <token>`; WebSocket clients
pass `?token=<token>` in the URL (browsers can't set WS headers). Confirm the value matches what you
passed to `--token`.

## An admin action returns `404`

Config-write and MCP add/remove live behind `--allow-admin`. Without that flag those routes don't
exist (they 404 rather than 403). Restart with `lev serve … --allow-admin`.

## No provider configured

An agent needs at least one [provider](/docs/providers). Run `lev setup`, or point Leviath at a
local [Ollama](https://ollama.com) for a no-key start.

## An agent seems stuck in a loop

That's what [stuck detection](/docs/stages#graph) is for. Add a `condition = "stuck"` transition
with thresholds (`stuck_after_iterations`, `stuck_after_same_file_edits`, …) so the runtime escapes
the stage automatically instead of burning tokens.

## A run vanished when I closed my terminal

It didn't — `lev run` hands the agent to the [daemon](/docs/daemon), which keeps hosting it. Bring
it back with `lev ps` or `lev dash`. If the daemon itself was stopped, it reloads interrupted runs
on its next start.

## Install fails with an auth error

During the private alpha, installing needs a GitHub token with `repo` scope (`GITHUB_TOKEN` for the
scripts, `HOMEBREW_GITHUB_API_TOKEN` for Homebrew) — see [Getting Started](/docs/getting-started).

> [!NOTE]
> Still stuck? Open an issue or a private security advisory on
> [GitHub](https://github.com/Sun-Forge-AI/leviath).
