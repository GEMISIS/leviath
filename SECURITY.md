# Security

## Reporting a vulnerability

Report privately, not as a public issue: use [GitHub's private vulnerability
reporting](https://github.com/GEMISIS/leviath/security/advisories/new).
Everything about a report happens there - filing, discussion, the fix, and
the advisory.

Please include what you need to and nothing you don't - a description of the
issue, the version or commit, and enough to reproduce it. A proof of concept
helps but is not required to file.

We'll acknowledge within 3 business days and give you an assessment within 10.
If we agree it's a vulnerability we'll tell you our intended fix and timeline,
and credit you in the advisory unless you'd rather we didn't.

## Supported versions

The `latest` release channel. Leviath is pre-1.0 and ships as a rolling
release, so fixes land on `main` and go out through alpha → beta → latest rather
than as backports to older tags.

## Threat model

Leviath runs LLM-driven tools - shell commands, file writes, HTTP requests, MCP
servers, and user-authored Rhai scripts - on your machine. Being clear about
what that does and does not defend against matters more than a list of features.

**What we defend against.** These are treated as real attackers, and a bypass is
a vulnerability:

- **A malicious or compromised agent package.** An `agent.leviath` you installed
  can only *tighten* what your `~/.leviath/config.toml` allows, never loosen it.
  For a tool you have not configured there is no setting of yours to clamp
  against, so a package may raise it no higher than Leviath's own default - with
  one named exception, `web_search` and `web_fetch`, which read-only research
  agents pre-approve and which cannot write or execute. Anything beyond that
  needs `[security] allow_blueprint_permissions`, or the tool named under
  `[agent_tool_permissions.<agent>]`, which is you saying "I trust this one".
  A package cannot grant itself tools you denied, run an unapproved command at
  spawn through a region seed, disable taint tracking, or weaken a sandbox you
  configured - not by turning it off, and not by widening it: it cannot add a
  bind-mount you did not grant, re-enable a network you isolated, or replace the
  engine binary. `lev add` prints what a package asks for before you run it.
- **Prompt injection reaching an agent's tools.** A model told by a fetched web
  page to exfiltrate your keys should fail. Script tools cannot read
  credential-shaped environment variables without an explicit allowlist entry,
  and outbound fetches cannot reach loopback, private, or link-local addresses -
  including cloud metadata endpoints - without `[security]
  allow_local_network`. The same address check covers every URL Leviath is
  *given* rather than chooses, including a completion webhook posted through
  `lev serve`.
- **A hostile MCP server.** OAuth discovery is bound to the server's own origin,
  the issuer is cross-checked per RFC 8414 §3.3, and the whole chain requires
  HTTPS off loopback. A server cannot redirect your browser or your token
  somewhere else, and redirects are capped so it cannot chain the daemon around
  your network. A server entry's `${VAR}` headers follow the same
  credential-name allowlist as script tools, so one cannot be written to post
  your API keys to a URL of its own choosing.
- **Another local user.** Every control-channel caller must quote a token the
  daemon mints at startup into its own owner-only directory, so a connection
  that cannot read your files cannot drive your agents. On Unix the socket is
  additionally `0600` and the daemon checks the peer's uid with the kernel.
  Secret files are created owner-only rather than tightened afterwards - POSIX
  `0600` on Unix, an ACL granting only you on Windows - and run artifacts are
  owner-only too.
- **A repository the agent is pointed at.** File tools resolve symlinks and
  refuse paths that leave the workspace, so a checked-in symlink cannot read
  your `~/.ssh`. A shell redirect answers to the same fence: `> path` outside
  the workspace is refused exactly as `write_file` refuses it, so the shell is
  not a spelling that gets further. No flag lifts that, `--yolo` included -
  it is containment rather than permission. The one exception is deliberate and yours to make: an agent
  may declare extra directories under `[read_paths]`, and those declarations
  are inert until your config grants them. When granted they are read-only,
  and every access is checked against the symlink-resolved real path, so a
  planted symlink inside a granted directory still cannot reach outside it.
- **The supply chain.** `Cargo.lock` is committed, `cargo-deny` gates advisories
  and licences on every PR, all GitHub Actions are SHA-pinned, and release
  binaries carry signed build provenance. Every install path - the shell
  installer, the PowerShell installer, the Homebrew formulae and the Scoop
  manifest - verifies the download against the release's `SHA256SUMS` and
  refuses to install on a mismatch. That catches a corrupted download, a swapped
  asset, or a mirror serving something else; it is not a substitute for
  verifying the provenance attestation, which is the stronger check and is
  tracked separately.

**Where the boundary is.** Not gaps we haven't got to - these are the edges of
what a tool like this can be responsible for, and it is worth being explicit
about them:

- **The model doing something unwise with permissions you granted it.** If you
  allow `shell`, an agent can run any command you can. Leviath's job is to make
  that grant explicit and scoped, not to second-guess it.
- **`--yolo`.** It waives approval prompts by design. It does *not* override a
  configured `deny` - that stays terminal - but everything else runs unattended.
  That is the point of the flag.
- **A compromised provider or model.** Leviath sends your context to whichever
  API you configured. Choosing that endpoint is your decision and we cannot
  audit what happens on the other side of it.
- **Which sandbox mode you pick.** Tools run on your machine unless you opt into
  `[sandbox]`. The `container` kind isolates the filesystem. The `namespace`
  kind isolates PIDs and optionally the network but **shares the host root
  filesystem** - it is not a filesystem sandbox, and both the docs and the code
  say so where it is defined. Pick `container` if the filesystem is what you
  need isolated.
- **A target with neither POSIX modes nor Windows ACLs.** Unix and Windows are
  both implemented; anything else gets whatever the platform does by default,
  and `leviath-sys`'s fallback module says so rather than pretending otherwise.

**Known gaps.** The ones we know about, so nobody has to find them twice.
Each is a place where the code is weaker than a reader of this document might
assume; none is a case of the code doing something other than what it says.

- **The Windows control pipe checks no peer.** On Unix the daemon refuses a
  control connection from another user, having read the peer's uid from the
  kernel. The Windows pipe has no such check and sets no security descriptor,
  so what stands between another local user and the daemon there is the
  control token alone (owner-only file, fresh per daemon). The code says the
  same where it accepts the connection.
- **`[sandbox]` isolates shell commands, not file tools.** The boundary
  covers the `shell` tool, seed commands and a Rhai tool's `shell()`. File
  tools stay on the host and rely on workdir confinement; a Rhai tool's
  `http_get`/`http_post` and `read_file`/`write_file` run on the host too.
  Pick `container` when the filesystem is what you need isolated, and know
  that a script tool reaches the network from the host either way.
- **A run's webhook secret is on disk in plaintext.** `callback_secret` is
  persisted in the run's `meta.json` so the daemon can still sign a webhook
  for a run it reloaded after a restart. It is stripped from every API
  response, and the file is `0600` in a `0700` directory, which is the same
  protection the provider keys in `config.toml` get. It is not encrypted at
  rest, and it stays on disk until the run is deleted.
- **`lev serve` does not check the `Host` header.** A DNS-rebinding page can
  therefore make a browser send requests to a local server under an origin
  the server would accept. The bearer token is what stops that from being
  useful: every API route requires it and a rebinding page does not have it.
  Do not embed the token in a page served from anywhere else, and do not use
  `--cors "*"` on a machine that browses the web.
- **OTLP log records carry the run's output unredacted.** With
  `[telemetry]` on, every output and runtime log line is exported as it was
  written. Nothing redacts a credential a tool happened to print on its way
  out, so point the exporter only at a collector you would trust with the
  run's transcript.
- **`POST /api/update` runs the installer it fetches.** On a script install
  the update step is `curl -fsSL https://leviath.dev/install.sh | sh`, over
  TLS and unpinned; the script then verifies the release checksums, but the
  script itself is trusted on the strength of the TLS connection. It is
  behind `--allow-admin` for that reason. A pinned install goes through your
  package manager instead.

If you find something this document claims but the code does not do, that is a
vulnerability and we want to hear about it - see the top of this file.

**What the scanners say.** CodeQL runs on every push and the count is kept at
zero, with nothing dismissed. One class needs explaining so it is not
re-triaged: CodeQL's Rust model treats every parameter of an axum handler as
request data, the shared `State` included, so a file location read off the
server state counts as a path-injection finding even when the operator set it
at startup. `lev serve` therefore keeps the config and token-store locations
behind a resolver installed at startup (`McpAdmin::paths()`, and
`UpdateJobs::with_env` for the update route). A new admin route reads paths
through that resolver, never through a field on the state. `CONTRIBUTING.md`
has the recipe for running the same queries locally.

## Where secrets live

| What | Where | Mode |
|---|---|---|
| Provider API keys | `~/.leviath/config.toml`, or the OS keychain | `0600` |
| MCP OAuth access + refresh tokens | `~/.leviath/mcp-auth.json`, or the OS keychain | `0600` |
| Run artifacts (prompts, conversations) | `~/.leviath/runs/<id>/` | `0600` in a `0700` dir |
| A run's webhook signing secret (`callback_secret`) | `~/.leviath/runs/<id>/meta.json` | same as the run; never served, see known gaps |
| Control socket | `~/.leviath/control.sock` (Unix) | `0600`, same-uid peers only, token required |
| Control pipe | `\\.\pipe\leviath-control-…` (Windows) | token required |
| Control token | `~/.leviath/control.token` | owner-only; fresh per daemon |
| API server token | `LEVIATH_API_TOKEN` or `--token` | not persisted |

Prefer `LEVIATH_API_TOKEN` over `--token`: an argument is visible in `ps` to
every local user for the lifetime of the process.

### Using the OS keychain instead

By default secrets live in the `0600` files above, which is the same shape as
comparable tools and the only arrangement that works headless, in containers,
over SSH, and on CI. To move them into the OS credential store - macOS Keychain,
Windows Credential Manager, or the Secret Service elsewhere - so that a stolen
`~/.leviath` directory yields nothing:

```toml
# ~/.leviath/config.toml
[security]
credential_store = "keychain"
```

Then move the secrets you already have:

```bash
lev auth migrate          # config file -> OS keychain
lev auth migrate --dry-run  # show what would move
lev auth migrate --to-file  # and back again
lev auth status           # which backend, and what it holds
```

Both kinds of secret move: provider API keys and MCP OAuth grants. In keychain
mode `mcp-auth.json` keeps only the *server names*, since the OS stores cannot be
enumerated and `lev mcp list` still has to be able to say what is logged in - a
server name is not a secret, the access and refresh tokens are.

`lev auth migrate` writes the destination and reads each secret back before
removing the source, so a store that accepts writes but does not persist them
cannot cost you your API keys. Nothing silently falls back to the file: if the
keychain is configured but unreachable, a write fails loudly rather than putting
plaintext tokens on disk.

This is opt-in rather than the default because an unavailable keychain is not a
degraded experience but a broken one - every inference fails at once - and the
environments Leviath is most useful in are the least likely to have a working
credential store. `lev auth status` reports whether this machine actually has
one. Builds can also omit credential-store support entirely (the `keychain`
feature), in which case `lev auth status` says so rather than offering a
migration that cannot work.

## Hardening a deployment

Running `lev serve` where others can reach it, or on shared infrastructure:

```bash
lev serve \
  --workdir-root /srv/agent-workspaces \   # agents cannot escape this root
  --no-remote-yolo \                       # requests cannot waive approvals
  --cors https://your-dashboard.example    # omit entirely for non-browser clients
# --allow-admin is off by default: the MCP admin endpoints write a spawn
# command into config and are remote code execution by construction.
```

And in `~/.leviath/config.toml`:

```toml
[security]
allow_seed_commands = false   # no manifest command runs before the first prompt
allow_local_network = false   # the default; agent fetches cannot reach your LAN
allow_env_vars = []           # the default; scripts read no credential-shaped vars

[tool_permissions]
shell = "ask"                 # a ceiling no installed agent can raise

[sandbox]
kind = "container"            # a manifest cannot turn this off
```

## Verifying a release

The installers do the checksum half for you - `install.sh`, `install.ps1`, the
Homebrew formulae and the Scoop manifest all verify against the release's
`SHA256SUMS` and refuse to install on a mismatch. To check by hand, or to verify
the attestation as well:

```bash
# Asset names are `leviath-<platform>-<arch>.<ext>`, e.g.:
gh attestation verify leviath-linux-x64.tar.gz --repo GEMISIS/leviath
sha256sum -c SHA256SUMS
```

The attestation is the stronger check, and the reason the checksum alone is not
enough: anyone who can write a release can rewrite both a binary *and* its
checksum, but the attestation is signed by GitHub's OIDC identity for the build
workflow and cannot be forged from inside the release. The installers do not
run it automatically yet, so `gh attestation verify` is a manual step; wiring
it into the installers is planned.

### Windows code signing

`lev.exe` ships **unsigned**. Authenticode signing needs a certificate from a
CA Microsoft trusts, and every such certificate costs money; Leviath is free
and is not paying for one. The consequence is that a new build has no
reputation with Defender or SmartScreen and may be flagged - a false positive
on an unknown file, which the build provenance attestation above answers: it
proves *which workflow* built the file, which is a stronger statement than a
publisher signature makes.

The release pipeline has no signing step. If signing is ever wanted, the free
path for open-source projects is the SignPath Foundation (see CONTRIBUTING).
