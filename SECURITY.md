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
  It cannot grant itself tools you denied, disable taint tracking, or weaken a
  sandbox you configured - not by turning it off, and not by widening it: it
  cannot add a bind-mount you did not grant, re-enable a network you isolated,
  or replace the engine binary. `lev add` prints what a package asks for before
  you run it.
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
  your `~/.ssh`. The one exception is deliberate and yours to make: an agent
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

**Known gaps.** None currently tracked. Everything the threat model claims is
implemented and tested on every supported platform. If you find something this
document claims but the code does not do, that is a vulnerability and we want to
hear about it - see the top of this file.

## Where secrets live

| What | Where | Mode |
|---|---|---|
| Provider API keys | `~/.leviath/config.toml`, or the OS keychain | `0600` |
| MCP OAuth access + refresh tokens | `~/.leviath/mcp-auth.json`, or the OS keychain | `0600` |
| Run artifacts (prompts, conversations) | `~/.leviath/runs/<id>/` | `0600` in a `0700` dir |
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
