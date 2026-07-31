---
title: Releases and channels
group: Guides
group_order: 4
order: 5
---

# Releases and channels

Leviath ships through three channels. A binary is built exactly once, on the
alpha channel, and then promoted unchanged: beta re-publishes the alpha
artifacts, and stable re-publishes the beta artifacts after verifying their
checksums. What you install from stable is byte-for-byte the build that went
through both earlier channels.

| Channel | Cadence | GitHub release tag | What it is |
|---|---|---|---|
| alpha | nightly | `alpha` (rolling) | Last night's `main`, fresh from CI |
| beta | weekly (Monday) | `beta` (rolling) | The alpha build that survived a week of nightlies |
| stable | weekly (Thursday, approval-gated) | `latest` (rolling) + `vX.Y.Z` (immutable) | The promoted beta |

Rolling tags (`alpha`, `beta`, `latest`) are recreated on every publish and
always point at the current build for that channel. Each stable deploy also
cuts an immutable versioned release; if the version was not bumped since the
last deploy, the tag gets a date suffix (`v0.1.0+20260731`) so history is
never overwritten. Release titles follow the same scheme: `Leviath
v0.1.0-Alpha`, `Leviath v0.1.0-Beta`, and `Leviath v0.1.0` for stable.

## Installing a specific channel

**Homebrew** has one formula per channel:

```bash
brew install leviath          # stable
brew install leviath-beta
brew install leviath-alpha
```

**Linux install script** takes a channel flag:

```bash
curl -fsSL https://raw.githubusercontent.com/GEMISIS/leviath-dist/main/install.sh \
  | bash -s -- --channel stable    # or beta, alpha
```

**Scoop** mirrors the Homebrew layout:

```powershell
scoop bucket add leviath https://github.com/GEMISIS/leviath-dist.git
scoop install leviath            # or leviath-beta, leviath-alpha
```

**Cargo** installs the released crates.io version, which corresponds to the
current stable line:

```bash
cargo install leviath-cli
```

The install scripts and package manifests live in the
[distribution repo](https://github.com/GEMISIS/leviath-dist).

## Verifying a download

Every release carries a `SHA256SUMS` file generated at build time and
re-verified at each promotion, and builds are attested with GitHub's build
provenance:

```bash
gh attestation verify leviath-linux-x64.tar.gz --repo GEMISIS/leviath
```

See [SECURITY.md](https://github.com/GEMISIS/leviath/blob/main/SECURITY.md)
for the full supply-chain story.

## Docs follow the binaries

This documentation site is published per channel, rendered from the exact
commit each channel's binaries were built from. If you are on beta, the beta
docs describe your binary, not whatever `main` looks like today.
