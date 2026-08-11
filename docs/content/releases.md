---
title: Releases and channels
description: How the alpha, beta, and stable channels work, and why stable ships the byte-for-byte alpha build.
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
| alpha | on a version bump, checked nightly | `alpha` (rolling) | The commit that bumped the version, fresh from CI |
| beta | weekly (Monday) | `beta` (rolling) | The alpha build that survived a week |
| stable | weekly (Thursday, approval-gated) | `latest` (rolling) + `vX.Y.Z` (immutable) | The promoted beta |

## What starts a release

A version bump does. Nothing else. Bumping `[workspace.package] version` in
the root `Cargo.toml` and merging that to `main` is the decision to ship, and
the commit carrying the bump is the one that gets built.

Alpha picks it up as soon as the merge lands, and re-checks every night in case
it missed one. Beta and stable stay on their weekly cadence, but each one asks
the same question before it does anything: is the version at the build I would
publish different from the version at the build I last published? When the
answer is no, the run finishes in seconds having touched nothing. A quiet
Monday means there was nothing new to promote, not that something failed.

So a bump merged on Tuesday is an alpha that day, a beta the following Monday,
and stable the Thursday after that. A week with no bump publishes nothing on
any channel.

Rolling tags (`alpha`, `beta`, `latest`) are recreated on every publish and
always point at the current build for that channel. Each stable deploy also
cuts an immutable versioned release. Release titles follow the same scheme:
`Leviath v0.1.2-Alpha`, `Leviath v0.1.2-Beta`, and `Leviath v0.1.2` for the immutable stable
release, while the rolling `latest` is titled with its channel.

Maintainers can re-cut a channel without a bump - to recover from an
infrastructure failure, say - by running the workflow by hand with its `force`
input. That is the only path that can land on a version already released, and
the versioned tag gets a date suffix (`v0.1.2+20260802`) when it does, so an
immutable tag is never moved.

## Installing a specific channel

**Homebrew** has one formula per channel:

```bash
brew tap gemisis/leviath https://github.com/GEMISIS/leviath-dist.git
brew install leviath          # stable
brew install leviath-beta
brew install leviath-alpha
```

**The install script** takes the channel from an environment variable:

```bash
LEVIATH_CHANNEL=beta curl -fsSL https://leviath.dev/install.sh | sh   # or alpha, stable
```

**Scoop** mirrors the Homebrew layout:

```powershell
scoop bucket add leviath https://github.com/GEMISIS/leviath-dist.git
scoop install leviath            # or leviath-beta, leviath-alpha
```

**Cargo** installs the released crates.io version, which tracks the stable
channel: each stable deploy publishes any crate version not yet on crates.io,
from the same commit the binaries were built at.

```bash
cargo install leviath-cli
```

The install scripts and package manifests live in the
[distribution repo](https://github.com/GEMISIS/leviath-dist).

### Running inside a container

Every channel ships seven archives: glibc and **musl** builds for Linux on x64 and arm64, plus
macOS on both and Windows on x64.

Reach for the musl ones when you are dropping `lev` into an image you did not build. The glibc
binaries link against the release runner's C library and so need `GLIBC_2.38` or newer, which is
Ubuntu 24.04 and up; anything older fails at exec with a `version not found` message before it runs
a line of Leviath. The musl archives are statically linked and need nothing from the image at all.

```bash
# The rolling tag for the channel you want: latest (stable), beta, or alpha.
CHANNEL=latest

# glibc: fine on a modern host, fails on an older container image
curl -fsSLO "https://github.com/GEMISIS/leviath/releases/download/$CHANNEL/leviath-linux-x64.tar.gz"

# musl: runs anywhere
curl -fsSLO "https://github.com/GEMISIS/leviath/releases/download/$CHANNEL/leviath-linux-x64-musl.tar.gz"
```

The musl archives were added after the current stable release, so they are on `alpha` and `beta`
today and reach `latest` with the next stable promotion. `gh release view <channel>` lists what a
channel actually carries.

They are the same binary otherwise. The glibc builds stay the default because they use the
platform's own resolver and NSS configuration, which is what you want on a machine you control.

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
