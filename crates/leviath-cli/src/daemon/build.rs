//! Which build a running daemon is, and how it compares with this `lev`.
//!
//! A daemon writes its build to `daemon.build` when it starts (see
//! [`super::setup::write_build_marker`]). A `lev` from another build reads it
//! to decide two things: whether a spawn command may replace the daemon (see
//! [`super::mode::DaemonMode`]), and what to tell the person on every
//! command while the two differ, since each build reads the home its own way
//! and a `lev` talking to the other build's daemon shows that build's view of
//! it.
//!
//! The marker is one line: the build id, then the version and the time of
//! the commit it was built from. A release that wrote only the id is one that
//! predates the version, so it is older than any build that writes all three.

use std::cmp::Ordering;

/// When the commit this binary was built from was made, in unix seconds, as
/// `build.rs` read it from git. Empty where git was not there to ask.
const COMMITTED_AT: &str = env!("LEVIATH_COMMITTED_AT");

/// One build of `lev`, as a build marker records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    /// The build id: the short commit hash, and a hash of any uncommitted
    /// changes.
    pub id: String,
    /// The release version. `None` for a daemon of a release that recorded
    /// only its id.
    pub version: Option<String>,
    /// When the commit it was built from was made, in unix seconds. `None`
    /// where it did not say.
    pub committed_at: Option<u64>,
}

/// How a running daemon's build stands to this `lev`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Standing {
    /// The same build.
    Same,
    /// The daemon is the older build: a lower version, or the same version
    /// built from an earlier commit. One that did not record its version is
    /// from before markers carried one, so it is older too.
    Older,
    /// The daemon is newer than this `lev`, which must not replace it.
    Newer,
    /// Another build of the same version that cannot be put in order: both
    /// built from one commit (a working tree edited between them), or one
    /// side does not know when its commit was made, as a build made without
    /// git does not.
    Unordered,
}

impl Standing {
    /// How the daemon relates to this `lev`, in the words a message uses:
    /// `older than`, `newer than`, and so on.
    fn relation(self) -> &'static str {
        match self {
            Self::Same => "the same build as",
            Self::Older => "older than",
            Self::Newer => "newer than",
            Self::Unordered => "another build of the same version as",
        }
    }
}

impl Build {
    /// This binary's build.
    pub fn current() -> Self {
        Self {
            id: super::setup::CURRENT_BUILD.to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            committed_at: COMMITTED_AT.parse().ok(),
        }
    }

    /// What a daemon of this build writes to its marker: the id, the
    /// version and the commit time, separated by spaces.
    pub fn marker(&self) -> String {
        let at = self
            .committed_at
            .map_or_else(String::new, |t| t.to_string());
        format!(
            "{} {} {at}",
            self.id,
            self.version.as_deref().unwrap_or_default()
        )
        .trim_end()
        .to_string()
    }

    /// Read a marker back. A marker of one word is a release that recorded
    /// only its build id.
    pub fn parse(marker: &str) -> Self {
        let mut words = marker.split_whitespace();
        Self {
            id: words.next().unwrap_or("unknown").to_string(),
            version: words.next().map(str::to_string),
            committed_at: words.next().and_then(|w| w.parse().ok()),
        }
    }

    /// `0.6.4 (build 1298d88c)`, or `an earlier release (build 839f0344)`
    /// for one that did not record its version.
    pub fn describe(&self) -> String {
        match &self.version {
            Some(version) => format!("{version} (build {})", self.id),
            None => format!("an earlier release (build {})", self.id),
        }
    }

    /// How the daemon built as `self` stands to the `lev` built as `cli`.
    ///
    /// Versions decide first, then commit times. A daemon that did not say
    /// its version is from before markers carried one, so it is older. Two
    /// builds of one version whose commit times tie, or where either side
    /// lacks one, are [`Standing::Unordered`]: neither is known to be older.
    pub fn standing(&self, cli: &Build) -> Standing {
        if self.id == cli.id {
            return Standing::Same;
        }
        let (Some(daemon), Some(ours)) = (&self.version, &cli.version) else {
            return Standing::Older;
        };
        let by_time = match (self.committed_at, cli.committed_at) {
            (Some(daemon), Some(cli)) => daemon.cmp(&cli),
            _ => Ordering::Equal,
        };
        match Release::parse(daemon)
            .cmp(&Release::parse(ours))
            .then(by_time)
        {
            Ordering::Less => Standing::Older,
            Ordering::Greater => Standing::Newer,
            Ordering::Equal => Standing::Unordered,
        }
    }
}

/// A version, ordered the way semver orders releases: `0.6.10` after
/// `0.6.9`, and a pre-release before its release, so `0.7.0-beta.1` comes
/// before `0.7.0`. Build metadata after a `+` is ignored.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Release {
    /// The major, minor and patch numbers. A part that is missing or is not
    /// a number counts as 0.
    core: [u64; 3],
    /// Whether this is a release rather than a pre-release, which sorts it
    /// after every pre-release of the same numbers.
    released: bool,
    /// The pre-release's dot-separated parts.
    pre: Vec<PreReleasePart>,
}

/// One dot-separated part of a pre-release. A number sorts before a word,
/// numbers by value and words by their text, as semver says.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PreReleasePart {
    /// A part made only of digits.
    Number(u64),
    /// Any other part.
    Word(String),
}

impl Release {
    /// Read `version`. It never fails: what does not parse counts as 0.
    fn parse(version: &str) -> Self {
        let (version, _metadata) = version.split_once('+').unwrap_or((version, ""));
        let (numbers, pre) = version.split_once('-').unwrap_or((version, ""));
        let mut parts = numbers.split('.').map(|part| part.parse().unwrap_or(0));
        let core = [(); 3].map(|()| parts.next().unwrap_or(0));
        let pre: Vec<_> = pre
            .split('.')
            .filter(|part| !part.is_empty())
            .map(PreReleasePart::parse)
            .collect();
        Self {
            core,
            released: pre.is_empty(),
            pre,
        }
    }
}

impl PreReleasePart {
    /// Read one part: digits are a number, anything else a word.
    fn parse(part: &str) -> Self {
        match part.parse() {
            Ok(number) => Self::Number(number),
            Err(_) => Self::Word(part.to_string()),
        }
    }
}

/// What a spawn command must do about the daemon whose marker reads
/// `marker`, already known to be running, and what it says while doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replace {
    /// Whether to stop it and start this build's daemon in its place.
    pub replace: bool,
    /// What to tell the person, if anything.
    pub say: Option<String>,
}

/// Decide whether a spawn command in `mode` replaces the running daemon,
/// whose marker reads `marker` (`None` when it wrote none), with this build's.
/// A daemon left running while it is another build is reported by
/// [`mixed_notice`], which every command but `lev daemon` prints first.
pub fn replace(marker: Option<&str>, cli: &Build, mode: super::mode::DaemonMode) -> Replace {
    let daemon = Build::parse(marker.unwrap_or_default());
    let standing = daemon.standing(cli);
    let replace = mode.replaces(standing);
    Replace {
        replace,
        say: replace.then(|| {
            format!(
                "leviath daemon is {}, {} this lev ({}); restarting it on this build...",
                daemon.describe(),
                standing.relation(),
                cli.describe()
            )
        }),
    }
}

/// What every other command says while the running daemon, whose marker
/// reads `marker`, is another build than this `lev`; `None` while they
/// match. `running` is whether a daemon answers at all: a marker left by one
/// that has exited says nothing.
pub fn mixed_notice(running: bool, marker: Option<&str>, cli: &Build) -> Option<String> {
    let daemon = Build::parse(marker.unwrap_or_default());
    let standing = daemon.standing(cli);
    (running && standing != Standing::Same).then(|| mixed_line(&daemon, cli, standing))
}

/// [`mixed_notice`] for the daemon of this home and this binary: what to say
/// before a command when they are different builds.
pub fn mixed_notice_here() -> Option<String> {
    let running = super::setup::control_address()
        .is_some_and(|id| leviath_runtime::control_socket::is_daemon_running(&id));
    let marker = super::setup::read_build_marker();
    mixed_notice(running, marker.as_deref(), &Build::current())
}

/// What `lev daemon status` says about the running daemon's build, whose
/// marker reads `marker`, when it is not this `lev`'s: how they stand, and
/// what to do. `None` while they match or no daemon runs.
pub fn status_line(running: bool, marker: Option<&str>, cli: &Build) -> Option<String> {
    let daemon = Build::parse(marker.unwrap_or_default());
    let standing = daemon.standing(cli);
    let what_to_do = match standing {
        Standing::Same => return None,
        Standing::Newer => "use the newer lev, or `lev daemon restart` to run this one's build",
        Standing::Older | Standing::Unordered => "`lev daemon restart` replaces it with this lev's",
    };
    running.then(|| {
        format!(
            "build: {}, {} this lev ({}); {what_to_do}",
            daemon.describe(),
            standing.relation(),
            cli.describe()
        )
    })
}

/// [`status_line`] for the daemon of this home and this binary.
pub fn status_line_here(running: bool) -> Option<String> {
    let marker = super::setup::read_build_marker();
    status_line(running, marker.as_deref(), &Build::current())
}

/// The warning for a daemon and a `lev` of different builds: which is which,
/// what it means for what this command shows, and what to do.
fn mixed_line(daemon: &Build, cli: &Build, standing: Standing) -> String {
    let what_to_do = match standing {
        Standing::Newer => {
            "Use the newer lev, or run `lev daemon restart` with this one to replace the daemon \
             with its own build"
        }
        _ => {
            "Run `lev daemon restart` to replace it with this lev's build, or use the lev that \
             matches it"
        }
    };
    format!(
        "warning: this daemon is {}, {} this lev ({}). Each build reads the home its own way, so \
         what this command shows may be the other build's view of it. {what_to_do}; run one \
         version of lev and its daemon at a time.",
        daemon.describe(),
        standing.relation(),
        cli.describe()
    )
}

#[cfg(test)]
#[path = "build_tests.rs"]
mod tests;
