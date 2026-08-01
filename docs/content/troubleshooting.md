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

## An admin action returns `405`

Config-write and MCP add/remove live behind `--allow-admin`. Without that flag the mutating method
isn't mounted, so it returns **405 Method Not Allowed** (the read routes still work). Restart with
`lev serve … --allow-admin`.

## No provider configured

An agent needs at least one [provider](/docs/providers). Run `lev setup`, or point Leviath at a
local [Ollama](https://ollama.com) for a no-key start.

## A run says `running` but never does anything

Check its provider first. If a stage's model list names only providers you haven't configured,
`lev run` refuses the spawn outright and tells you which ones it tried — configure one with
`lev setup`, or add it to `config.toml` and restart the daemon.

A run that gets past that and still can't dispatch (say you removed a provider key after it
started) is failed after `[limits] stall_timeout_secs`, 60 seconds by default. Its `meta.json`
records the reason. Set the limit to `0` to wait indefinitely instead.

Waiting for a busy model is *not* this. An agent queued behind other in-flight requests to the same
model is working as intended and is never failed, however long the queue takes — raise
`[limits] max_concurrent_inferences` if you want more of them running at once.

## Every run looks busy but nothing finishes

Run `lev ps` and read the line under the table. If there isn't one, the daemon thinks it is
getting somewhere and the problem is in a particular run rather than the daemon as a whole.

A `lanes:` line means the tool lane is full with batches queued behind it. On its own that is just
a busy factory. What matters is the second half:

```
lanes: tools 8/8 busy, 3 parked, 12 queued  ·  no progress for 14 cycles (7m)
```

A cycle is 30 seconds, so that reads as seven minutes in which work was waiting for the tool lane
and not one run moved. `parked` is not part of the problem: a batch waiting on a person or on
another run holds no capacity. `busy` with a `queued` figure that never falls is.

The usual cause is too little tool capacity for the shape of the workload. Raise
`[limits] max_concurrent_tools` and restart the daemon. Left alone, the daemon widens the lane
itself after `[limits] dead_cycles_before_relief` cycles (10 by default) and says so in the log at
`error` level; it never cancels anything, and it stops after one extra lane's worth.

The same numbers are exported as `leviath.tool_lane.*` and
`leviath.scheduler.dead_cycles.total` if you have [observability](/docs/observability) on. A
healthy daemon sits at zero dead cycles.

## An agent seems stuck in a loop

That's what [stuck detection](/docs/stages#graph) is for. Add a `condition = "stuck"` transition
with thresholds (`stuck_after_iterations`, `stuck_after_same_file_edits`, …) so the runtime escapes
the stage automatically instead of burning tokens.

## A run vanished when I closed my terminal

It didn't. `lev run` hands the agent to the [daemon](/docs/daemon), which keeps hosting it. Bring
it back with `lev ps` or `lev dash`. If the daemon itself was stopped, it reloads interrupted runs
on its next start.

## Install fails with an auth error

No token is needed, because the repos and release assets are public. A 401/403 during install usually
means leftovers from the private alpha: remove any
`url."https://…@github.com/GEMISIS/".insteadOf` rewrite from `~/.gitconfig`, unset stale
`GITHUB_TOKEN` / `HOMEBREW_GITHUB_API_TOKEN` exports (an expired token *fails* requests that
would succeed anonymously), and retry.

> [!NOTE]
> Still stuck? Open an issue or a private security advisory on
> [GitHub](https://github.com/GEMISIS/leviath).
