//! What the setup wizard is made of: its steps, the rows it renders, and the
//! shapes an answer can take.
//!
//! Held apart from the wizard itself because these are the vocabulary the
//! renderer and the input handler both speak, while the wizard is the thing
//! that happens to move between them.

use super::*;

/// The wizard's screens, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// What the wizard is about to do, before it touches anything.
    Welcome,
    /// Pick which providers to configure.
    Providers,
    /// Enter or confirm a credential for each picked provider.
    ProviderDetail,
    /// The default provider and model new runs use.
    Defaults,
    /// Concurrency, timeouts, and the other numeric ceilings.
    Limits,
    /// Choose which bundled blueprints to install.
    Agents,
    /// Import MCP servers found in other harnesses' configs.
    Mcp,
    /// The whole plan, before anything is written.
    Review,
}

impl Step {
    /// Every step, in order.
    pub const ALL: [Step; 8] = [
        Step::Welcome,
        Step::Providers,
        Step::ProviderDetail,
        Step::Defaults,
        Step::Limits,
        Step::Agents,
        Step::Mcp,
        Step::Review,
    ];

    /// Title shown in the header.
    pub fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Welcome",
            Step::Providers => "Providers",
            Step::ProviderDetail => "Credentials",
            Step::Defaults => "Defaults",
            Step::Limits => "Limits",
            Step::Agents => "Agents",
            Step::Mcp => "MCP servers",
            Step::Review => "Review",
        }
    }

    /// Position in [`Self::ALL`].
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|s| *s == self)
            .expect("every step is in ALL")
    }
}

/// One row of the provider pick-list.
#[derive(Debug, Clone)]
pub struct ProviderRow {
    /// Which provider this row is for.
    pub provider: Provider,
    /// Whether the user has picked it.
    pub selected: bool,
    /// The credential as typed. Empty means "no value".
    pub value: String,
    /// The credential is already in the environment, under this variable, and
    /// is not being written to the config.
    pub from_env: Option<&'static str>,
    /// Reasoning effort, for the Claude Code transport only.
    pub effort: usize,
    /// What the last verification attempt concluded.
    pub outcome: Outcome,
    /// A verification is in flight.
    pub checking: bool,
}

impl ProviderRow {
    /// Whether this provider has something to verify.
    pub fn has_credential(&self) -> bool {
        match self.provider.credential {
            Credential::ApiKey => !self.value.is_empty() || self.from_env.is_some(),
            // Ollama and the Claude Code transport need no key; selecting them
            // is the whole configuration, so they are always checkable.
            Credential::BaseUrl | Credential::None => true,
        }
    }
}

/// One row of the blueprint list.
#[derive(Debug, Clone)]
pub struct AgentRow {
    /// The bundled blueprint this row offers.
    pub agent: &'static BundledAgent,
    /// What installing it would do: a fresh install, an upgrade, or nothing
    /// because the same version is already there.
    pub action: AgentAction,
    /// Whether the user has picked it.
    pub selected: bool,
}

/// One importable MCP server.
#[derive(Debug, Clone)]
pub struct McpRow {
    /// The server definition as found, before collision handling.
    pub candidate: Candidate,
    /// Which harness it came from.
    pub source: String,
    /// Whether the user has picked it.
    pub selected: bool,
    /// A server of this name is already in the Leviath config.
    pub collides: bool,
    /// The name it will actually be stored under, after collision handling.
    pub name: String,
}

/// A full-screen chooser for one of the Defaults screen's list values.
///
/// The arrows cycle those fields in place, which is fine for three providers
/// and hopeless for eighty models: you read the list one value at a time
/// through a single line, and the only way back is round again. The picker
/// shows the list, filters it as you type, and says what the value decides -
/// which the field could not, in the one line it had.
pub struct Picker {
    /// Which Defaults field this is choosing for.
    pub field: usize,
    /// Its heading, taken from the field it opened from.
    pub title: &'static str,
    /// What the value actually decides, and what it does not.
    pub explain: Vec<&'static str>,
    /// The search box. Crate-visible like the widget it holds, which is not
    /// part of any public surface.
    pub(crate) query: crate::tui::widgets::line_edit::LineEdit,
    /// Every option, in the field's own order.
    pub options: Vec<PickerOption>,
    /// Cursor into the *filtered* list, not into `options`.
    pub cursor: usize,
}

/// One row of a [`Picker`].
pub struct PickerOption {
    /// The value that would be written.
    pub value: String,
    /// Where it came from, or what it is, in a few words.
    pub detail: String,
}

impl Picker {
    /// The options matching the query, as indices into `options`.
    ///
    /// Every whitespace-separated term has to appear somewhere in the row, so
    /// "claude sonnet" finds the model whichever order the id puts them in,
    /// and a term can match the provider that reported it as easily as the
    /// name itself.
    pub fn matches(&self) -> Vec<usize> {
        let query = self.query.value().to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        self.options
            .iter()
            .enumerate()
            .filter(|(_, option)| {
                let haystack = format!("{} {}", option.value, option.detail).to_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Which option the cursor is on, if the filter left anything.
    pub fn selected(&self) -> Option<usize> {
        self.matches().get(self.cursor).copied()
    }

    /// Move within the filtered list, clamped to it.
    ///
    /// Clamping rather than wrapping: at eighty models, wrapping from the top
    /// to the bottom looks like the list jumped rather than moved.
    pub fn move_cursor(&mut self, delta: isize) {
        let count = self.matches().len();
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, count.saturating_sub(1) as isize) as usize;
    }
}

/// A thing the credential screen can do, offered as its own row.
///
/// These were shortcut keys and nothing else, which meant they existed only
/// for people who had read the footer. As rows they can be seen, moved onto
/// with the arrows, and clicked; `o` and `v` still work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailAction {
    /// Open the provider's signup or key page in a browser.
    OpenSignup,
    /// Check the credential against the provider.
    Verify,
}

impl DetailAction {
    /// The button's text, which names the provider so the row says what it
    /// will do rather than what it is called.
    pub fn label(self, provider: &str) -> String {
        match self {
            Self::OpenSignup => format!("Open the {provider} key page"),
            Self::Verify => "Check this credential".to_string(),
        }
    }
}

/// A single editable setting on the Defaults / Limits screens.
#[derive(Debug, Clone)]
pub struct Field {
    /// The setting's name, as shown.
    pub label: &'static str,
    /// One line explaining what it does, shown under the label.
    pub help: &'static str,
    /// Its current value, which also decides what a key press means.
    pub value: FieldValue,
}

/// The kinds of setting the wizard edits, and therefore the ways a key press
/// can mean something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    /// A whole number. `None` means unset.
    Number(Option<u64>),
    /// A toggle.
    Bool(bool),
    /// One of a fixed list.
    Choice {
        /// Every value this field may take, in the order they are cycled.
        options: Vec<String>,
        /// Which one is selected, by position in `options`.
        index: usize,
    },
}

impl FieldValue {
    /// How the value reads on screen.
    pub fn display(&self) -> String {
        match self {
            Self::Number(None) => "(unset)".to_string(),
            Self::Number(Some(n)) => n.to_string(),
            Self::Bool(true) => "yes".to_string(),
            Self::Bool(false) => "no".to_string(),
            Self::Choice { options, index } => match options.get(*index) {
                Some(chosen) => chosen.clone(),
                None => "(none)".to_string(),
            },
        }
    }

    /// Move a choice to `index`.
    ///
    /// A no-op for the other kinds, which have no list to move within. Total
    /// rather than fallible because the caller that has a chosen index already
    /// knows which field it came from.
    pub fn set_index(&mut self, next: usize) {
        if let Self::Choice { index, .. } = self {
            *index = next;
        }
    }

    /// The options of a choice field; empty for any other kind.
    pub fn options(&self) -> &[String] {
        match self {
            Self::Choice { options, .. } => options,
            Self::Number(_) | Self::Bool(_) => &[],
        }
    }
}

/// Asked of the background verifier.
#[derive(Debug, Clone)]
pub struct VerifyRequest {
    /// Which provider to check, matching the row that asked.
    pub provider_id: String,
    /// The credentials to check, as typed or taken from the environment.
    pub creds: leviath_runtime::provider_creds::ProviderCreds,
}

/// Answered by the background verifier.
#[derive(Debug, Clone)]
pub struct VerifyReply {
    /// Which provider this answers for. Replies can arrive out of order, so
    /// this is what routes one back to its row.
    pub provider_id: String,
    /// What the check concluded.
    pub outcome: Outcome,
}

/// Where the text being typed goes when it is committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    /// The credential of the provider at this index in `providers`.
    Credential(usize),
    /// The field at this index of the current step's fields.
    Field(usize),
}

/// An in-progress text edit.
#[derive(Debug, Clone)]
pub struct Edit {
    /// Where the text goes when it is committed.
    pub target: EditTarget,
    /// The shared single-line editor (cursor movement, masking).
    pub(crate) line: crate::tui::widgets::line_edit::LineEdit,
}

/// Why a confirmation dialog is on screen, so its Yes can be routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmPurpose {
    /// `q`/Ctrl-C with unsaved choices: quit and discard?
    QuitDiscard,
    /// Saving with the Claude Code transport selected: accept the terms risk?
    SaveTos,
    /// Leaving the Providers screen with nothing selected: continue anyway?
    NoProviders,
}

/// A pending confirmation: the dialog plus what its Yes means.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingConfirm {
    /// What answering Yes would mean.
    pub purpose: ConfirmPurpose,
    pub(crate) dialog: crate::tui::widgets::confirm::Confirm,
}

// Reasoning effort is a property of a provider row, so it belongs beside the
// row rather than with `[limits]` - which it shared a neighbourhood with only
// because of where it happened to sit in the original file.
/// Reasoning-effort levels for the Claude Code transport, from the provider
/// rather than re-typed here.
pub fn effort_options() -> &'static [&'static str] {
    &leviath_providers::claude_code::EFFORT_LEVELS
}

/// Index of `effort` in [`effort_options`], defaulting to the provider default.
pub(super) fn effort_index(effort: Option<&str>) -> usize {
    let wanted = effort.unwrap_or(leviath_providers::claude_code::DEFAULT_EFFORT);
    effort_options()
        .iter()
        .position(|e| *e == wanted)
        .unwrap_or_default()
}
