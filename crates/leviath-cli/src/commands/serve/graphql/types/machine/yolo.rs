//! The yolo profiles this machine has configured: what an unattended run
//! waives by default, and how many rules each profile carries.

use async_graphql::{ID, SimpleObject};

/// One named yolo profile, summarised.
///
/// The counts rather than the rules: a settings list shows how much a profile
/// waives, and `lev yolo show` prints the rules themselves.
#[derive(Debug, SimpleObject)]
pub(crate) struct YoloProfile {
    /// `yoloProfile:<name>`. One file holds the profiles, one profile per
    /// name, so the name is the whole key.
    #[graphql(owned)]
    pub(crate) id: ID,
    /// The profile's name, as `--yolo=<name>` spells it.
    pub(crate) name: String,
    /// What tools with no explicit rule do.
    pub(crate) default: YoloWaiver,
    /// What happens to the run's own questions.
    pub(crate) questions: YoloHuman,
    /// What happens at blueprint checkpoints.
    pub(crate) checkpoints: YoloHuman,
    /// What happens at the taint gate.
    pub(crate) gate: YoloHuman,
    /// How many tool rules it carries: allow, ask, deny.
    pub(crate) tool_rules: Vec<i32>,
    /// How many shell rules it carries: allow, ask, deny.
    pub(crate) shell_rules: Vec<i32>,
}

/// What a profile does with a tool no rule names.
///
/// Two values, not three: a profile waives prompts, it never adds a refusal.
/// A tool a profile does not reach is decided by the config's own permissions,
/// which can still deny it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum YoloWaiver {
    /// Runs without asking.
    Allow,
    /// Stops and asks, as it would with no profile.
    Ask,
}

/// Whether one human-in-the-loop mechanism still reaches a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum YoloHuman {
    /// Reaches a person and waits.
    Ask,
    /// Answers itself and carries on.
    Auto,
}

/// The yolo profiles, and where they are read from.
#[derive(Debug, SimpleObject)]
pub(crate) struct YoloProfiles {
    /// The file the profiles are read from.
    pub(crate) path: String,
    /// Whether that file exists. False with no error means `--yolo=<name>` has
    /// nothing to name yet.
    pub(crate) exists: bool,
    /// Why the file does not load, when it does not. The profiles are then
    /// empty, and a spawn naming one is refused with this same message.
    pub(crate) error: Option<String>,
    /// The profiles themselves.
    pub(crate) profiles: Vec<YoloProfile>,
}
