//! Named yolo profiles: `--yolo=<name>` backed by `yolo.toml` beside
//! `config.toml`.
//!
//! Bare `--yolo` is one bit: every `Ask` becomes `Allow`, the human tools are
//! not offered, checkpoints approve themselves. A profile is that bit taken
//! apart. Each table in the file names what a run may do without asking, what
//! still goes through the ordinary approval prompt, and what is refused, down
//! to individual shell commands and the paths they may touch. The person
//! launching the run picks one by name; the blueprint never sees it.
//!
//! The file is the user's, not the agent's. It is read at spawn and again when
//! a run resumes, never mid-batch, and `[security] lock_permission_files`
//! keeps a run's tools from writing it. Bare `--yolo` does not read the file
//! at all: what it does is fixed, and `default` is a reserved name so nobody
//! expects otherwise.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

pub(crate) mod decide;
pub(crate) use decide::{Decision, ToolKind};
pub(crate) mod rules;

use rules::{Compiled, ProfileSpec, RuleError};

/// The file's name, beside `config.toml`.
pub(crate) const FILE_NAME: &str = "yolo.toml";

/// The one name a profile may not have: bare `--yolo` is not configurable.
pub(crate) const RESERVED_NAME: &str = "default";

/// Where the file lives: next to whatever `config.toml` resolves to, so it
/// follows `LEVIATH_HOME` and `LEVIATH_CONFIG_PATH` alike.
pub(crate) fn yolo_path() -> PathBuf {
    let mut path = crate::config::Config::config_path();
    path.set_file_name(FILE_NAME);
    path
}

/// Why a file, a profile or a name was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum YoloError {
    /// The file does not parse, or could not be read.
    Parse(String),
    /// A profile is named in a way the flag cannot spell, or is reserved.
    Name { name: String, reason: String },
    /// An entry in a profile could not be compiled.
    Rule { profile: String, error: RuleError },
    /// `--yolo=<name>` named a profile the file does not have.
    UnknownProfile { name: String, known: Vec<String> },
    /// `--yolo=<name>` was given and there is no file at all.
    NoFile { name: String, path: PathBuf },
}

impl fmt::Display for YoloError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            YoloError::Parse(e) => write!(f, "{FILE_NAME} does not load: {e}"),
            YoloError::Name { name, reason } => write!(f, "yolo profile {name:?}: {reason}"),
            YoloError::Rule { profile, error } => write!(f, "yolo profile [{profile}]: {error}"),
            YoloError::UnknownProfile { name, known } if known.is_empty() => write!(
                f,
                "no yolo profile named {name:?}: {FILE_NAME} defines no profiles"
            ),
            YoloError::UnknownProfile { name, known } => write!(
                f,
                "no yolo profile named {name:?}; {FILE_NAME} defines: {}",
                known.join(", ")
            ),
            YoloError::NoFile { name, path } => write!(
                f,
                "--yolo={name} names a profile, but {} does not exist; `lev yolo init` writes an \
                 example to start from",
                path.display()
            ),
        }
    }
}

impl std::error::Error for YoloError {}

/// One profile, compiled and ready to decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct YoloProfile {
    /// The table header, or `default` for the built-in bare `--yolo`.
    pub name: String,
    pub spec: ProfileSpec,
    compiled: Compiled,
}

/// A profile's shape in one line each, for listings and the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProfileSummary {
    pub name: String,
    pub default: rules::Waiver,
    pub questions: rules::Human,
    pub checkpoints: rules::Human,
    pub gate: rules::Human,
    /// `[allow, ask, deny]` entry counts.
    pub tool_rules: [usize; 3],
    /// `[allow, ask, deny]` rule counts.
    pub shell_rules: [usize; 3],
}

impl YoloProfile {
    fn compile(name: &str, spec: ProfileSpec) -> Result<Self, YoloError> {
        let compiled = Compiled::compile(&spec).map_err(|error| YoloError::Rule {
            profile: name.to_string(),
            error,
        })?;
        Ok(Self {
            name: name.to_string(),
            spec,
            compiled,
        })
    }

    /// Bare `--yolo` as a profile.
    pub(crate) fn builtin_default() -> Arc<Self> {
        Arc::new(Self {
            name: RESERVED_NAME.to_string(),
            spec: ProfileSpec::builtin_default(),
            compiled: Compiled::default(),
        })
    }

    /// Whether this is the built-in bare `--yolo`, which reads no file.
    pub(crate) fn is_builtin_default(&self) -> bool {
        self.name == RESERVED_NAME
    }

    pub(crate) fn summary(&self) -> ProfileSummary {
        let t = &self.spec.tools;
        let s = &self.spec.shell;
        ProfileSummary {
            name: self.name.clone(),
            default: self.spec.default,
            questions: self.spec.questions,
            checkpoints: self.spec.checkpoints,
            gate: self.spec.gate,
            tool_rules: [t.allow.len(), t.ask.len(), t.deny.len()],
            shell_rules: [s.allow.len(), s.ask.len(), s.deny.len()],
        }
    }

    /// What this profile still puts to a person, for the pre-flight block
    /// `lev run --yolo=<name>` prints. Empty for bare `--yolo`.
    pub(crate) fn holds(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.spec.questions.is_auto() {
            lines.push(
                "the model's questions (ask_user_*, present_for_review, edit_document) come to you"
                    .to_string(),
            );
        }
        if !self.spec.checkpoints.is_auto() {
            lines.push("every stage checkpoint opens its prompt".to_string());
        }
        if !self.spec.gate.is_auto() {
            lines.push("taint-gate prompts open, where taint tracking is on".to_string());
        }
        if self.spec.default == rules::Waiver::Ask {
            lines.push(
                "any tool call the lists do not allow asks, as it would without --yolo".to_string(),
            );
        }
        let asks: Vec<String> = self
            .spec
            .tools
            .ask
            .iter()
            .cloned()
            .chain(
                self.spec
                    .shell
                    .ask
                    .iter()
                    .map(|r| format!("shell `{}`", r.command)),
            )
            .collect();
        if !asks.is_empty() {
            lines.push(format!("these ask: {}", asks.join(", ")));
        }
        lines
    }
}

/// The parsed file: every profile, compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct YoloFile {
    profiles: BTreeMap<String, Arc<YoloProfile>>,
    /// Whether a file was there to read. A missing file is an empty one for
    /// every purpose but the error `--yolo=<name>` gets.
    exists: bool,
}

impl Default for YoloFile {
    fn default() -> Self {
        Self::missing()
    }
}

impl YoloFile {
    /// No file on disk.
    pub(crate) fn missing() -> Self {
        Self {
            profiles: BTreeMap::new(),
            exists: false,
        }
    }

    /// Parse and compile the file's text.
    pub(crate) fn from_toml(text: &str) -> Result<Self, YoloError> {
        let raw: BTreeMap<String, ProfileSpec> =
            toml::from_str(text).map_err(|e| YoloError::Parse(e.to_string()))?;
        let mut profiles = BTreeMap::new();
        for (name, spec) in raw {
            validate_name(&name)?;
            profiles.insert(name.clone(), Arc::new(YoloProfile::compile(&name, spec)?));
        }
        Ok(Self {
            profiles,
            exists: true,
        })
    }

    /// Read and parse `path`; a missing file is [`missing`](Self::missing).
    pub(crate) fn load_from(path: &Path) -> Result<Self, YoloError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::missing()),
            Err(e) => Err(YoloError::Parse(e.to_string())),
        }
    }

    pub(crate) fn exists(&self) -> bool {
        self.exists
    }

    /// Profile names in file order (the file is a TOML table, so sorted).
    pub(crate) fn names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }

    pub(crate) fn get(&self, name: &str) -> Option<Arc<YoloProfile>> {
        self.profiles.get(name).cloned()
    }

    pub(crate) fn profiles(&self) -> impl Iterator<Item = &Arc<YoloProfile>> {
        self.profiles.values()
    }

    /// The profile `--yolo[=name]` asked for. `None` or an empty name is the
    /// bare flag, which never reads the file.
    pub(crate) fn resolve(
        &self,
        name: Option<&str>,
        path: &Path,
    ) -> Result<Arc<YoloProfile>, YoloError> {
        let Some(name) = name.filter(|n| !n.is_empty()) else {
            return Ok(YoloProfile::builtin_default());
        };
        if !self.exists {
            return Err(YoloError::NoFile {
                name: name.to_string(),
                path: path.to_path_buf(),
            });
        }
        self.get(name).ok_or_else(|| YoloError::UnknownProfile {
            name: name.to_string(),
            known: self.names(),
        })
    }
}

/// The file as it stands right now.
///
/// Read fresh rather than cached: a spawn and a resume are the only readers,
/// both rare, and the file is small. Reading it at those two moments is what
/// makes an edit reach the next run with no daemon restart, and reading it at
/// no other moment is what keeps a run from being re-judged mid-batch.
pub(crate) fn load_current() -> Result<YoloFile, YoloError> {
    YoloFile::load_from(&yolo_path())
}

/// The profile a spawn runs under: `None` for an attended run, the built-in
/// default for bare `--yolo`, and the named profile for `--yolo=<name>`,
/// read from the file as it stands now. A name the file does not have, or a
/// file that does not load, fails the spawn: the person asked for a specific
/// set of rules and did not get them.
pub(crate) fn resolve_for_spawn(
    yolo: bool,
    name: Option<&str>,
) -> Result<Option<Arc<YoloProfile>>, YoloError> {
    if !yolo {
        return Ok(None);
    }
    let named = name.filter(|n| !n.is_empty());
    if named.is_none() {
        return Ok(Some(YoloProfile::builtin_default()));
    }
    let path = yolo_path();
    let file = YoloFile::load_from(&path)?;
    file.resolve(named, &path).map(Some)
}

/// What a run needs to know about its home for `~` in a profile's paths.
pub(crate) fn home() -> Option<PathBuf> {
    leviath_core::home_dir()
}

/// What the profile makes of a call, or `None` when the run has no profile
/// and `configured` stands as it is.
///
/// The one function the three call sites share: the tool lane, the seeds
/// that run at spawn, and a script tool's `inherit`. They must agree, because
/// a seed can reach nothing the agent could not reach mid-run.
pub(crate) fn decide_under(
    profile: Option<&YoloProfile>,
    tool: &str,
    arguments: &serde_json::Value,
    configured: crate::config::ToolPolicy,
    launch_allowed: bool,
    kind: decide::ToolKind,
    workdir: &Path,
) -> Option<Decision> {
    let profile = profile?;
    let home = home();
    Some(profile.decide(&decide::DecideInput::for_run(
        tool,
        arguments,
        configured,
        launch_allowed,
        kind,
        workdir,
        home.as_deref(),
    )))
}

/// [`decide_under`] reduced to the policy, for the call sites that have no
/// message to put a reason in.
pub(crate) fn apply_profile(
    profile: Option<&YoloProfile>,
    tool: &str,
    arguments: &serde_json::Value,
    configured: crate::config::ToolPolicy,
    launch_allowed: bool,
    kind: decide::ToolKind,
    workdir: &Path,
) -> crate::config::ToolPolicy {
    decide_under(
        profile,
        tool,
        arguments,
        configured,
        launch_allowed,
        kind,
        workdir,
    )
    .map_or(configured, |d| d.policy)
}

/// A name the flag can spell and that is not reserved.
pub(crate) fn validate_name(name: &str) -> Result<(), YoloError> {
    if name == RESERVED_NAME {
        return Err(YoloError::Name {
            name: name.to_string(),
            reason: "`default` is what bare --yolo means and cannot be redefined; pick another \
                     name and pass it as --yolo=<name>"
                .to_string(),
        });
    }
    let spellable = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    match spellable {
        true => Ok(()),
        false => Err(YoloError::Name {
            name: name.to_string(),
            reason: "a profile name is letters, digits, `_` and `-`, so it can be passed as \
                     --yolo=<name>"
                .to_string(),
        }),
    }
}

#[cfg(test)]
mod tests;
