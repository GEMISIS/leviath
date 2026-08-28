//! How this copy of `lev` got onto the machine, and which channel it tracks.
//!
//! Split out of `update.rs` by concern: everything here is pure over a path,
//! a home directory and what `brew --prefix` said, so every arm is testable
//! without a Homebrew, a Scoop or a second machine. See the parent module for
//! why the install method is detected rather than recorded.

use std::path::{Path, PathBuf};

use super::INSTALL_URL;

// ─── Channels ─────────────────────────────────────────────────────────────────

/// A release channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Channel {
    /// The weekly stable release. What crates.io and `brew install leviath` track.
    Stable,
    /// The promoted build a week ahead of stable.
    Beta,
    /// The nightly build.
    Alpha,
}

impl Channel {
    /// The name the install script and the docs use.
    pub fn id(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
            Self::Alpha => "alpha",
        }
    }

    /// The Homebrew formula and Scoop package for this channel. They share a
    /// naming scheme on purpose, so one function answers for both.
    pub fn package(self) -> &'static str {
        match self {
            Self::Stable => "leviath",
            Self::Beta => "leviath-beta",
            Self::Alpha => "leviath-alpha",
        }
    }

    /// The channel a package name carries, or `None` for a name this build does
    /// not ship (someone's own formula, or one from a future channel).
    pub fn from_package(name: &str) -> Option<Self> {
        [Self::Stable, Self::Beta, Self::Alpha]
            .into_iter()
            .find(|c| c.package() == name)
    }
}

// ─── Install-method detection ─────────────────────────────────────────────────

/// How this copy of `lev` got onto the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// Homebrew, under the named formula.
    Homebrew {
        /// The formula, which is also what carries the channel.
        formula: String,
    },
    /// Scoop, under the named package.
    Scoop {
        /// The package, which carries the channel the same way a formula does.
        package: String,
    },
    /// `cargo install`, so the binary was compiled locally.
    Cargo,
    /// The hosted install script, or something else that dropped a plain binary
    /// where the install script puts one.
    Script {
        /// The channel to re-install. Never detected: see the module docs.
        channel: Channel,
    },
    /// Somewhere no supported installer writes.
    Unknown {
        /// Where the binary actually is, so the report can say it.
        path: PathBuf,
    },
}

impl InstallMethod {
    /// The short name used in `--json`.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Homebrew { .. } => "homebrew",
            Self::Scoop { .. } => "scoop",
            Self::Cargo => "cargo",
            Self::Script { .. } => "script",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// The channel this install tracks, where that is knowable.
    ///
    /// `cargo install leviath-cli` resolves crates.io, and each stable deploy
    /// publishes there from the same commit the binaries were built at, so a
    /// cargo install is a stable install by construction.
    pub fn channel(&self) -> Option<Channel> {
        match self {
            Self::Homebrew { formula } => Channel::from_package(formula),
            Self::Scoop { package } => Channel::from_package(package),
            Self::Cargo => Some(Channel::Stable),
            Self::Script { channel } => Some(*channel),
            Self::Unknown { .. } => None,
        }
    }

    /// The one-line description the report opens with.
    pub fn describe(&self) -> String {
        let channel = match self.channel() {
            Some(c) => format!(", {} channel", c.id()),
            // A formula this build does not ship, or a path nothing claims.
            None => String::new(),
        };
        match self {
            Self::Homebrew { formula } => format!("Homebrew (formula {formula}{channel})"),
            Self::Scoop { package } => format!("Scoop (package {package}{channel})"),
            Self::Cargo => format!("cargo install (crates.io{channel})"),
            Self::Script { .. } => format!("the install script ({INSTALL_URL}{channel})"),
            Self::Unknown { path } => {
                format!("something else - the binary is at {}", path.display())
            }
        }
    }
}

/// The component of `path` immediately after the first one equal to `marker`.
///
/// This is how both package managers record what they installed: Homebrew lays
/// a binary out as `<prefix>/Cellar/<formula>/<version>/bin/lev`, and Scoop as
/// `<root>/apps/<package>/current/lev.exe`. The name in that slot is the real
/// answer to "which channel is this", and it is on disk rather than inferred.
pub(super) fn component_after(path: &Path, marker: &str) -> Option<String> {
    let mut components = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned());
    components.by_ref().find(|c| c == marker)?;
    components.next()
}

/// Whether `path` has a component equal to `marker`, ignoring case.
///
/// Scoop's root is a user-chosen directory that is conventionally but not
/// reliably lowercase, and Windows paths are case-insensitive anyway.
pub(super) fn has_component(path: &Path, marker: &str) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(marker))
}

/// Homebrew prefixes that mean Homebrew and nothing else, for the case where
/// the binary is the `bin/lev` symlink rather than the Cellar path behind it.
///
/// `/usr/local` is deliberately absent even though it is Homebrew's own prefix
/// on Intel macOS: it is also where the install script and a hand-unpacked
/// tarball put a binary, so treating everything under it as Homebrew would send
/// a script install to `brew upgrade`. Under that prefix the Cellar component is
/// the only evidence that counts.
pub(super) const UNAMBIGUOUS_BREW_PREFIXES: &[&str] =
    &["/opt/homebrew", "/home/linuxbrew/.linuxbrew"];

/// Absolute directories the installers write to. The Linux installer hard-codes
/// `/usr/local/bin`; `/usr/bin` is where the manual tarball instructions end up
/// for anyone who moved it there instead.
pub(super) const SCRIPT_DESTINATIONS: &[&str] = &["/usr/local/bin", "/usr/bin"];

/// Every directory a plain-binary install lands in, including the two that are
/// home-relative: `~/.local/bin`, and the `%LOCALAPPDATA%\Leviath\bin` that
/// `install.ps1` writes on Windows.
///
/// A loose binary in one of these is a script install. A loose binary anywhere
/// else is not something to guess about, because re-running an installer aims
/// at a fixed destination and would leave the copy actually on `PATH` untouched.
pub(super) fn script_destinations(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = SCRIPT_DESTINATIONS.iter().map(PathBuf::from).collect();
    if let Some(home) = home {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(
            home.join("AppData")
                .join("Local")
                .join("Leviath")
                .join("bin"),
        );
    }
    dirs
}

/// Work out how `exe` was installed.
///
/// Pure over its inputs - the resolved executable path, the home directory, the
/// answer `brew --prefix` gave (if it was asked and answered), and the channel
/// the user named - so every arm is testable without a Homebrew, a Scoop or a
/// second machine.
pub fn detect(
    exe: &Path,
    home: Option<&Path>,
    brew_prefix: Option<&Path>,
    requested: Option<Channel>,
) -> InstallMethod {
    // The channel to fall back on where the path does not name one.
    let channel = requested.unwrap_or(Channel::Stable);

    // Homebrew, from the strongest evidence down. A Cellar path names the
    // formula outright; a prefix only says "Homebrew put this here".
    if let Some(formula) = component_after(exe, "Cellar") {
        return InstallMethod::Homebrew { formula };
    }
    let under_brew = UNAMBIGUOUS_BREW_PREFIXES.iter().any(|p| exe.starts_with(p))
        || brew_prefix.is_some_and(|p| exe.starts_with(p) && !is_ambiguous_prefix(p));
    if under_brew {
        return InstallMethod::Homebrew {
            formula: channel.package().to_string(),
        };
    }

    // Scoop, the same two ways round.
    if has_component(exe, "scoop") {
        let package = component_after(exe, "apps").unwrap_or_else(|| channel.package().to_string());
        return InstallMethod::Scoop { package };
    }

    // A cargo install, which is the one method that cannot be updated in place.
    let cargo_bin = home.map(|h| h.join(".cargo").join("bin"));
    if cargo_bin.is_some_and(|dir| exe.starts_with(dir)) {
        return InstallMethod::Cargo;
    }

    let parent = exe.parent();
    let script_dir = script_destinations(home)
        .iter()
        .any(|d| parent == Some(d.as_path()));
    match script_dir {
        true => InstallMethod::Script { channel },
        false => InstallMethod::Unknown {
            path: exe.to_path_buf(),
        },
    }
}

/// Whether a prefix is too general to be evidence of anything on its own. See
/// [`UNAMBIGUOUS_BREW_PREFIXES`] for why `/usr/local` is the case that matters.
pub(super) fn is_ambiguous_prefix(prefix: &Path) -> bool {
    matches!(
        prefix.to_string_lossy().trim_end_matches('/'),
        "/usr/local" | "/usr" | "" | "/"
    )
}

/// What `brew --prefix` says, when there is a `brew` to ask.
///
/// Cached: this used to run once per `lev update`, and now also answers an HTTP
/// route, where spawning a process per request to learn a thing that cannot
/// change under a running server would be a waste worth noticing.
pub fn brew_prefix() -> Option<PathBuf> {
    static CACHED: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| {
            brew_prefix_from(
                leviath_sys::child_command("brew")
                    .arg("--prefix")
                    .output()
                    .ok(),
            )
        })
        .clone()
}

/// The part of [`brew_prefix`] that is worth testing: what to make of whatever
/// `brew --prefix` did or did not say.
///
/// Any failure is a `None` - it only ever adds evidence, and a machine without
/// Homebrew is the ordinary case rather than an error. Empty output is a `None`
/// for the same reason: an empty prefix would match every path under
/// [`detect`]'s `starts_with`, which is the opposite of no evidence.
///
/// Takes the answer rather than a closure that produces one. A generic seam
/// would be tidier to read and is measured per instantiation, so the arms this
/// machine's own `brew` does not take would go uncovered however many closures
/// the tests passed. The command is spawned once, inside the cache above, so
/// taking it eagerly costs nothing.
pub(super) fn brew_prefix_from(output: Option<std::process::Output>) -> Option<PathBuf> {
    let output = output?;
    let prefix = String::from_utf8(output.stdout).ok()?;
    let prefix = prefix.trim();
    match prefix.is_empty() {
        true => None,
        false => Some(PathBuf::from(prefix)),
    }
}
