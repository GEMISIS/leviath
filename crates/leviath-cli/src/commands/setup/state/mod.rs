//! The wizard's state: what step we're on, what's been chosen, and how a
//! choice turns back into a [`SetupPlan`].
//!
//! Deliberately free of drawing and of key handling - those are `render` and
//! `input`. Everything here is ordinary data and pure transitions, so the whole
//! flow is testable without a terminal.

use std::collections::HashMap;

use leviath_mcp::MCPServerConfig;
use tokio::sync::mpsc;

use super::catalog::{self, Credential, Provider};
use super::import::{self, Candidate};
use super::plan::SetupPlan;
use super::verify::Outcome;
use crate::bundled::{AgentAction, BundledAgent};
use crate::config::Config;

// Sections of the former single-file wizard state. Glob re-exported so every
// existing `state::Wizard` path keeps working.
mod limits;
use limits::*;
mod types;
pub(crate) use types::*;

/// The whole wizard.
pub struct Wizard {
    /// The screen currently shown.
    pub step: Step,
    /// Selected row within the current step.
    pub cursor: usize,
    /// Every provider the wizard offers, picked or not.
    pub providers: Vec<ProviderRow>,
    /// Which selected provider the credential screen is showing.
    pub detail: usize,
    /// The Defaults screen's settings.
    pub defaults: Vec<Field>,
    /// The Limits screen's settings.
    pub limits: Vec<Field>,
    /// Every bundled blueprint the wizard offers.
    pub agents: Vec<AgentRow>,
    /// MCP servers found in other harnesses' configs.
    pub mcp: Vec<McpRow>,
    /// Harnesses whose config could not be read, shown rather than swallowed
    /// so an empty MCP list is distinguishable from a failed scan.
    pub mcp_scan_errors: Vec<String>,
    /// The text edit in progress, if any. While this is set, typing goes here
    /// rather than to the screen's own key bindings.
    pub edit: Option<Edit>,
    /// Show credentials in clear text.
    pub reveal: bool,
    /// The help overlay is on screen.
    pub show_help: bool,
    /// A confirmation dialog is on screen, and (after Ctrl-C) it is the only
    /// thing keys mean until it is answered.
    pub confirm: Option<PendingConfirm>,
    /// The user has changed something since the wizard opened, so quitting
    /// silently would discard real choices.
    pub dirty: bool,
    /// The user has acknowledged the Claude Code transport's terms risk. A
    /// hard gate on saving: the transport cannot be written to the config
    /// without it, and deselecting the transport withdraws it.
    pub claude_code_tos_accepted: bool,
    /// The user asked to leave and the loop should stop.
    pub should_quit: bool,
    /// Set once the plan has been applied, so the loop knows to stop.
    pub finished: bool,
    /// A one-line status message.
    pub message: Option<String>,
    /// The config as loaded *from the file*, which is what the plan is
    /// diffed against and built on.
    pub base: Config,
    /// Credentials present only in the environment. Shown, never written.
    pub env_only: HashMap<&'static str, String>,
    /// Opens a provider's signup page. Injected rather than called directly:
    /// `lev dash` once had a unit test launch a real browser, and this is the
    /// same shape of hazard.
    pub opener: leviath_mcp::BrowserOpener,
    /// Where a credential check is sent. Verification runs off the UI thread
    /// so a slow provider cannot freeze the wizard.
    pub verify_tx: mpsc::UnboundedSender<VerifyRequest>,
    verify_rx: Option<mpsc::UnboundedReceiver<VerifyRequest>>,
    reply_tx: mpsc::UnboundedSender<VerifyReply>,
    /// Where finished checks arrive, drained once per tick.
    pub reply_rx: mpsc::UnboundedReceiver<VerifyReply>,
    /// Tick counter, for the spinner.
    pub ticks: u64,
    /// First visible row of the current step, so a screen taller than the
    /// terminal can still be reached. Head-anchored, unlike the log panel's
    /// tail-anchored `ScrollState` the log panel uses: a wizard
    /// screen is read from the top, and the cursor decides what must be shown.
    pub scroll: usize,
    /// Whether the tuning screen is on the path.
    ///
    /// Off by default. Every one of those limits has a working default, so
    /// walking a first-time user through them taught them that setup is long
    /// rather than that Leviath is configurable. Turned on from the Defaults
    /// screen, it slots the `Limits` step back into the flow.
    pub show_advanced: bool,
    /// How far the help overlay is scrolled. See the dashboard's field for
    /// why it is a `Cell`.
    pub help_scroll: std::cell::Cell<usize>,
    /// The open chooser for a Defaults value, if one is open.
    pub picker: Option<Picker>,
}

/// The environment variables the wizard reports as already-supplying a
/// credential, paired with their provider.
fn env_credentials(lookup: &dyn Fn(&str) -> Option<String>) -> HashMap<&'static str, String> {
    catalog::providers()
        .iter()
        .filter_map(|p| {
            let var = p.env_var?;
            let value = lookup(var)?;
            (!value.is_empty()).then_some((var, value))
        })
        .collect()
}

impl Wizard {
    /// Build the wizard from the config *file* and the surrounding environment.
    ///
    /// `base` must come from reading the file, not from `Config::load()`:
    /// `load` folds `$ANTHROPIC_API_KEY` and friends into the struct, and the
    /// old wizard then re-serialized the whole thing - silently writing a key
    /// the user had deliberately kept in their environment into
    /// `~/.leviath/config.toml`. Environment-supplied credentials are tracked
    /// separately in `env_only` and shown as such.
    pub fn new(
        base: Config,
        env_lookup: &dyn Fn(&str) -> Option<String>,
        candidates: Vec<(String, Candidate)>,
        scan_errors: Vec<String>,
        agents_dir: &std::path::Path,
        opener: leviath_mcp::BrowserOpener,
    ) -> Self {
        let env_only = env_credentials(env_lookup);

        let providers = catalog::providers()
            .into_iter()
            .map(|provider| {
                let stored = catalog::stored_credential(&base, provider.id);
                let from_env = provider
                    .env_var
                    .filter(|v| stored.is_none() && env_only.contains_key(v));
                ProviderRow {
                    selected: catalog::is_configured(&base, provider.id) || from_env.is_some(),
                    value: stored.unwrap_or_default(),
                    from_env,
                    effort: effort_index(base.providers.claude_code_effort.as_deref()),
                    outcome: Outcome::Skipped,
                    checking: false,
                    provider,
                }
            })
            .collect();

        let agents = crate::bundled::plan_agent_actions(agents_dir)
            .into_iter()
            .map(|(agent, action)| AgentRow {
                selected: action.preselect(),
                agent,
                action,
            })
            .collect();

        let mcp = candidates
            .into_iter()
            .map(|(source, candidate)| {
                let collides =
                    import::already_configured(&base.mcp_servers, &candidate.config.name);
                let name = import::dedup_name(&base.mcp_servers, &candidate.config.name);
                McpRow {
                    // A server already configured under this name is offered
                    // unchecked: the user has it, and silently adding a second
                    // copy under a suffixed name is not what "import" means.
                    selected: !collides,
                    source,
                    collides,
                    name,
                    candidate,
                }
            })
            .collect();

        let (verify_tx, verify_rx) = mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = mpsc::unbounded_channel();

        let mut wizard = Self {
            step: Step::Welcome,
            cursor: 0,
            providers,
            detail: 0,
            defaults: Vec::new(),
            limits: limits_fields(&base),
            agents,
            mcp,
            mcp_scan_errors: scan_errors,
            edit: None,
            reveal: false,
            show_help: false,
            confirm: None,
            dirty: false,
            claude_code_tos_accepted: false,
            should_quit: false,
            finished: false,
            message: None,
            base,
            env_only,
            opener,
            verify_tx,
            verify_rx: Some(verify_rx),
            reply_tx,
            reply_rx,
            ticks: 0,
            scroll: 0,
            show_advanced: false,
            help_scroll: std::cell::Cell::new(0),
            picker: None,
        };
        wizard.rebuild_defaults();
        wizard
    }

    /// Hand the background verifier loop its channel ends. Returns `None` if
    /// already taken.
    pub fn take_verify_ends(
        &mut self,
    ) -> Option<(
        mpsc::UnboundedReceiver<VerifyRequest>,
        mpsc::UnboundedSender<VerifyReply>,
    )> {
        self.verify_rx.take().map(|rx| (rx, self.reply_tx.clone()))
    }

    // ── Rows and navigation ─────────────────────────────────────────────────

    /// Providers the user picked, in table order.
    pub fn selected_providers(&self) -> Vec<usize> {
        self.providers
            .iter()
            .enumerate()
            .filter(|(_, r)| r.selected)
            .map(|(i, _)| i)
            .collect()
    }

    /// The provider row the credential screen is currently showing.
    pub fn detail_row(&self) -> Option<usize> {
        self.selected_providers().get(self.detail).copied()
    }

    /// Whether the Claude Code transport is one of the picked providers.
    pub fn claude_code_selected(&self) -> bool {
        self.providers
            .iter()
            .any(|r| r.selected && r.provider.id == "claude-code")
    }

    /// Whether saving must first ask the user to acknowledge Anthropic's
    /// terms. Enabling the transport routes inference through a subscription
    /// session, so the risk is confirmed once, explicitly, rather than being
    /// buried in a paragraph nobody reads.
    pub fn needs_tos_confirmation(&self) -> bool {
        self.claude_code_selected() && !self.claude_code_tos_accepted
    }

    /// The fields the current step edits, if it edits fields.
    pub fn fields(&self) -> &[Field] {
        match self.step {
            Step::Defaults => &self.defaults,
            Step::Limits => &self.limits,
            _ => &[],
        }
    }

    pub(super) fn fields_mut(&mut self) -> Option<&mut Vec<Field>> {
        match self.step {
            Step::Defaults => Some(&mut self.defaults),
            Step::Limits => Some(&mut self.limits),
            _ => None,
        }
    }

    /// The credential screen's action rows, after the credential itself.
    ///
    /// They exist as rows rather than as shortcut keys alone because that is
    /// how they become discoverable: a row can be seen, moved onto, and
    /// clicked, and `o` and `v` still work for anyone who knows them.
    pub fn detail_actions(&self) -> Vec<DetailAction> {
        let Some(index) = self.detail_row() else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        if self.providers[index].provider.signup_url.is_some() {
            actions.push(DetailAction::OpenSignup);
        }
        actions.push(DetailAction::Verify);
        actions
    }

    /// How many selectable rows the current step has.
    pub fn row_count(&self) -> usize {
        match self.step {
            Step::Welcome | Step::Review => 0,
            Step::Providers => self.providers.len(),
            Step::ProviderDetail => match self.detail_row() {
                Some(_) => 1 + self.detail_actions().len(),
                None => 0,
            },
            Step::Defaults => self.defaults.len(),
            Step::Limits => self.limits.len(),
            Step::Agents => self.agents.len(),
            Step::Mcp => self.mcp.len(),
        }
    }

    /// How many cursor positions the current step has: its rows plus the
    /// Continue/action button that every step ends with.
    pub fn nav_rows(&self) -> usize {
        self.row_count() + 1
    }

    /// Whether the cursor sits on the step's Continue/action button (the
    /// virtual row after the last real one).
    pub fn on_continue(&self) -> bool {
        self.cursor == self.row_count()
    }

    /// The label of the current step's Continue/action button. It carries
    /// state (selection counts, what screen is next) so advancing is never a
    /// surprise.
    pub fn continue_label(&self) -> String {
        match self.step {
            Step::Welcome => "Get started".to_string(),
            Step::Review => "Apply and finish".to_string(),
            Step::Providers => {
                let count = self.selected_providers().len();
                if count == 0 {
                    "Continue (no providers selected)".to_string()
                } else {
                    format!("Continue: {} ({count} selected)", self.next_step_title())
                }
            }
            Step::ProviderDetail => {
                let selected = self.selected_providers();
                match selected.get(self.detail + 1) {
                    Some(&next) => format!("Next: {}", self.providers[next].provider.display),
                    None => format!("Continue: {}", self.next_step_title()),
                }
            }
            Step::Defaults | Step::Limits | Step::Agents | Step::Mcp => {
                format!("Continue: {}", self.next_step_title())
            }
        }
    }

    /// The title of the step `next_step` would land on.
    fn next_step_title(&self) -> &'static str {
        let mut index = self.step.index();
        while index + 1 < Step::ALL.len() {
            index += 1;
            let step = Step::ALL[index];
            if !self.is_empty_step(step) {
                return step.title();
            }
        }
        Step::Review.title()
    }

    /// Move the selection, clamped to the step's rows plus its button.
    pub fn move_cursor(&mut self, delta: isize) {
        let count = self.nav_rows();
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, count as isize - 1) as usize;
    }

    /// How many rows a page key moves. Smaller than most windows on purpose:
    /// a page that overshoots the pane is indistinguishable from a jump.
    pub const PAGE: isize = 8;

    /// Scroll by whole rows.
    ///
    /// Where there is something to select, this moves the selection and lets
    /// the renderer follow it, so the view and the cursor can never disagree
    /// about what the user is looking at. Welcome and Review have no rows, so
    /// there the offset moves on its own.
    pub fn scroll_by(&mut self, rows: isize) {
        if self.row_count() > 0 {
            self.move_cursor(rows);
            return;
        }
        self.scroll = self.scroll.saturating_add_signed(rows);
    }

    /// Jump to the top of the current step.
    pub fn scroll_home(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Jump to the end of the current step, which is always its button.
    ///
    /// The offset is set past any possible content and clamped when drawn,
    /// because the number of lines a step occupies depends on the window it is
    /// drawn into and is not known here.
    pub fn scroll_end(&mut self) {
        self.cursor = self.nav_rows().saturating_sub(1);
        self.scroll = usize::MAX;
    }

    /// Advance to the next step, skipping ones with nothing to show. Skipping
    /// the credential screen is announced rather than silent: it looks exactly
    /// like a bug when a screen the breadcrumb promises never appears.
    pub fn next_step(&mut self) {
        let mut index = self.step.index();
        while index + 1 < Step::ALL.len() {
            index += 1;
            let step = Step::ALL[index];
            if !self.is_empty_step(step) {
                self.enter(step);
                return;
            }
            if step == Step::ProviderDetail {
                self.message =
                    Some("Skipped Credentials: no selected provider needs setup.".to_string());
            }
        }
        // Past the last step: the Review screen's action is to save.
        self.enter(Step::Review);
    }

    /// Go back a step, skipping empty ones. No-op on the first.
    pub fn prev_step(&mut self) {
        let mut index = self.step.index();
        while index > 0 {
            index -= 1;
            let step = Step::ALL[index];
            if !self.is_empty_step(step) {
                self.enter(step);
                return;
            }
        }
    }

    /// Whether a step has nothing worth showing, and should be skipped.
    ///
    /// Only the two discovery-driven screens can be empty: nobody should have
    /// to press Enter through "no MCP servers found" on a clean machine.
    fn is_empty_step(&self, step: Step) -> bool {
        match step {
            Step::Mcp => self.mcp.is_empty() && self.mcp_scan_errors.is_empty(),
            Step::ProviderDetail => self.detail_row().is_none(),
            // Not empty so much as not asked for. Routing it through the same
            // predicate keeps `next_step`, `prev_step` and the Continue
            // button's own label agreeing about what comes next, which they
            // would not if the skip were special-cased at one call site.
            Step::Limits => !self.show_advanced,
            _ => false,
        }
    }

    /// Switch to `step`, resetting per-step state.
    pub fn enter(&mut self, step: Step) {
        self.step = step;
        self.cursor = 0;
        self.scroll = 0;
        self.edit = None;
        if step == Step::Defaults {
            // The model picker is populated by verification, which may have
            // finished since the last visit.
            self.rebuild_defaults();
        }
    }

    /// Within the credential screen, move to the next selected provider;
    /// returns false when there is no next one.
    pub fn next_detail(&mut self) -> bool {
        if self.detail + 1 < self.selected_providers().len() {
            self.detail += 1;
            self.cursor = 0;
            self.edit = None;
            return true;
        }
        false
    }

    /// The reverse of [`Self::next_detail`].
    pub fn prev_detail(&mut self) -> bool {
        if self.detail > 0 {
            self.detail -= 1;
            self.cursor = 0;
            self.edit = None;
            return true;
        }
        false
    }

    // ── Verification ────────────────────────────────────────────────────────

    /// Ask the background verifier about the provider at `index`.
    ///
    /// A provider with nothing to check is left alone rather than queued: a
    /// blank API key would fail with a message about the key rather than saying
    /// the obvious, that none was given.
    pub fn request_verification(&mut self, index: usize) {
        let Some(row) = self.providers.get_mut(index) else {
            return;
        };
        if !row.has_credential() {
            row.outcome = Outcome::Skipped;
            return;
        }
        let id = row.provider.id.to_string();
        let key = if row.value.is_empty() {
            self.env_only
                .get(row.provider.env_var.unwrap_or_default())
                .cloned()
        } else {
            Some(row.value.clone())
        };
        let base_url = (row.provider.credential == Credential::BaseUrl).then(|| {
            if row.value.is_empty() {
                catalog::DEFAULT_OLLAMA_URL.to_string()
            } else {
                row.value.clone()
            }
        });
        row.checking = true;

        let creds = leviath_runtime::provider_creds::ProviderCreds {
            name: id.clone(),
            api_key: base_url.is_none().then_some(key).flatten(),
            base_url,
            model_capabilities: HashMap::new(),
            request_timeout_secs: Some(20),
            rate_limit: None,
            options: HashMap::new(),
        };
        // A closed receiver means the background task is gone; the row simply
        // stays "checking" and nothing else breaks.
        let _ = self.verify_tx.send(VerifyRequest {
            provider_id: id,
            creds,
        });
    }

    /// Ask about every selected provider at once.
    pub fn verify_all(&mut self) {
        for index in self.selected_providers() {
            self.request_verification(index);
        }
    }

    /// Take whatever the background verifier has answered.
    pub fn drain_verifications(&mut self) {
        let mut landed = false;
        while let Ok(reply) = self.reply_rx.try_recv() {
            if let Some(row) = self
                .providers
                .iter_mut()
                .find(|r| r.provider.id == reply.provider_id)
            {
                row.checking = false;
                row.outcome = reply.outcome;
                landed = true;
            }
        }
        // A reply carries the model list, and the picker was built from
        // whatever had arrived when the screen opened. Without this, moving on
        // from a credential and straight into Defaults shows an empty picker
        // for the provider that was verified half a second ago. Rebuilding
        // keeps the current selection, so nothing the user chose is lost.
        if landed && self.step == Step::Defaults {
            self.rebuild_defaults();
        }
    }

    /// What choosing one of these values actually decides.
    ///
    /// Written against `leviath_runtime::pipeline::resolve`, which is the code
    /// that reads them. The line about a blueprint winning is the one worth
    /// having: a user who sets a default and then watches an agent run on
    /// something else has been told, by every other tool, that a default is a
    /// default.
    fn precedence_explanation(provider: bool) -> Vec<&'static str> {
        if provider {
            vec![
                "Where your runs go by default. A stage that lists this provider among",
                "its models is served by it first, ahead of the blueprint's own order.",
                "",
                "This field does nothing on its own. Until you also set a default model",
                "below, the only thing it can do is reorder a list that already names",
                "this provider, so on a machine keyed for one provider that no blueprint",
                "mentions, every stage still goes somewhere else.",
                "",
                "It never overrides a blueprint that pins its provider: a stage with",
                "allow_user_default = false ignores this, and so does a provider you",
                "have not given a credential.",
            ]
        } else {
            vec![
                "The model your default provider is asked for. The two travel together:",
                "this name is never sent to a different provider, so an OpenAI model is",
                "never asked of Anthropic.",
                "",
                "Setting it is what makes the default provider above take effect. The",
                "pair is offered to every stage that allows a user default, and moves to",
                "the front of the models that stage lists.",
                "",
                "Leaving it unset does not mean your provider picks a model. It means",
                "each blueprint uses the models it names, and a stage that names none",
                "falls back to a model built into Leviath, whoever you configured.",
            ]
        }
    }

    /// Every model id any provider reported, deduplicated, for the picker.
    pub fn discovered_models(&self) -> Vec<String> {
        let mut models: Vec<String> = self
            .providers
            .iter()
            .filter(|r| r.selected)
            .flat_map(|r| r.outcome.models().iter().cloned())
            .collect();
        models.sort();
        models.dedup();
        models
    }

    // ── Forms ───────────────────────────────────────────────────────────────

    /// Rebuild the Defaults screen. Called on entry, since both the provider
    /// list and the discovered models can change between visits.
    pub fn rebuild_defaults(&mut self) {
        let chosen = self.current_default_provider();
        let providers: Vec<String> = self
            .selected_providers()
            .iter()
            .map(|i| self.providers[*i].provider.id.to_string())
            .collect();
        // Fall back to whatever is configured when nothing is selected, so the
        // field is never empty.
        let providers = if providers.is_empty() {
            vec![self.base.default_provider.clone()]
        } else {
            providers
        };
        let index = providers.iter().position(|p| *p == chosen).unwrap_or(0);

        let mut models = vec![Self::NO_DEFAULT_MODEL.to_string()];
        models.extend(self.discovered_models());
        let current_model = self
            .current_default_model()
            .unwrap_or_else(|| Self::NO_DEFAULT_MODEL.to_string());
        if !models.contains(&current_model) {
            models.push(current_model.clone());
        }
        let model_index = models
            .iter()
            .position(|m| *m == current_model)
            .unwrap_or_default();

        let timeout = self.current_request_timeout();
        self.defaults = vec![
            Field {
                label: "Default provider",
                help: "Preferred by any stage that allows a user default. Takes effect once a \
                       default model is set below.",
                value: FieldValue::Choice {
                    options: providers,
                    index,
                },
            },
            Field {
                label: "Default model",
                help: "Offered to every stage that allows a user default, paired with the \
                       provider above. Listed from what your providers reported.",
                value: FieldValue::Choice {
                    options: models,
                    index: model_index,
                },
            },
            Field {
                label: "Request timeout (seconds)",
                help: "How long to wait on one inference. Unset uses the provider default.",
                value: FieldValue::Number(timeout),
            },
            Field {
                label: "Show advanced tuning",
                help: "Adds a screen of concurrency, retry and context limits. Every one of \
                       them already has a default that works.",
                value: FieldValue::Bool(self.show_advanced),
            },
        ];
        // Re-pick the concurrency default now that the provider choice is
        // settled. Doing this only on an arrow press missed the commonest
        // Ollama case entirely: when it is the *only* provider selected it is
        // already at index 0, nobody ever presses an arrow, and the limit
        // stayed at the hosted-API default of 8.
        self.apply_provider_concurrency_default();
    }

    /// Where the provider choice sits on the Defaults screen.
    pub const PROVIDER_FIELD: usize = 0;

    /// Where the advanced-tuning toggle sits on the Defaults screen.
    pub const ADVANCED_FIELD: usize = 3;

    /// The model field's "no default" option.
    ///
    /// It read "(provider default)", which is a thing that does not exist: no
    /// provider default model is consulted anywhere at run time. A stage that
    /// names no model of its own falls back to a model built into Leviath, not
    /// to anything your provider chose, so the old label promised a mechanism
    /// and delivered the opposite of what it said.
    pub const NO_DEFAULT_MODEL: &'static str = "(each blueprint decides)";

    /// Open the chooser for the Defaults field the cursor is on.
    ///
    /// The options come from the caller because it has already matched on the
    /// field's kind: re-reading them here would add a shape this cannot be in.
    pub(super) fn open_picker(&mut self, title: &'static str, options: Vec<String>, index: usize) {
        let field = self.cursor;
        let options = options
            .into_iter()
            .map(|value| {
                let detail = if field == Self::PROVIDER_FIELD {
                    self.provider_detail(&value)
                } else {
                    self.model_detail(&value)
                };
                PickerOption { value, detail }
            })
            .collect();
        self.picker = Some(Picker {
            field,
            title,
            explain: Self::precedence_explanation(field == Self::PROVIDER_FIELD),
            query: crate::tui::widgets::line_edit::LineEdit::new(String::new(), false),
            options,
            // Opening on the current value rather than at the top: the list is
            // long, and "where am I now" is the first thing you look for.
            cursor: index,
        });
    }

    /// What a provider id is, for the chooser's second column.
    fn provider_detail(&self, id: &str) -> String {
        match self.providers.iter().find(|r| r.provider.id == id) {
            Some(row) => row.provider.display.to_string(),
            // A provider that is configured but not in the catalog: it came
            // from the config file, so it is still a legitimate choice.
            None => "from your config".to_string(),
        }
    }

    /// Which providers reported a model, so the row says where it came from.
    fn model_detail(&self, model: &str) -> String {
        // The first row is the absence of a model, not a model, so the
        // question "who reported it" does not apply to it.
        if model == Self::NO_DEFAULT_MODEL {
            return "no default; every blueprint uses the models it names".to_string();
        }
        let reported: Vec<&str> = self
            .providers
            .iter()
            .filter(|r| r.selected && r.outcome.models().iter().any(|m| m == model))
            .map(|r| r.provider.display)
            .collect();
        if reported.is_empty() {
            return "not reported by a provider you selected".to_string();
        }
        format!("reported by {}", reported.join(", "))
    }

    /// Take the chooser's answer, writing it back into the field it came from.
    pub(super) fn commit_picker(&mut self, picker: Picker) {
        let Some(chosen) = picker.selected() else {
            // An empty filter has nothing to choose; closing without a change
            // is the only honest outcome.
            return;
        };
        // The chooser's options were built from this field's, one for one and
        // in order, so the match index *is* the field's index. Indexing rather
        // than looking up: the field is where it was when the chooser opened,
        // and nothing rebuilds the form while one is on screen.
        self.defaults[picker.field].value.set_index(chosen);
        self.dirty = true;
        // The concurrency default follows the provider, so an Ollama-first
        // setup does not inherit a number meant for hosted APIs.
        if picker.field == Self::PROVIDER_FIELD {
            self.apply_provider_concurrency_default();
        }
    }

    /// The default provider as it currently stands in the form, or the base
    /// config's value before the form exists.
    fn current_default_provider(&self) -> String {
        match self.defaults.first().map(|f| &f.value) {
            Some(FieldValue::Choice { options, index }) => match options.get(*index) {
                Some(chosen) => chosen.clone(),
                None => self.base.default_provider.clone(),
            },
            _ => self.base.default_provider.clone(),
        }
    }

    fn current_default_model(&self) -> Option<String> {
        match self.defaults.get(1).map(|f| &f.value) {
            Some(FieldValue::Choice { options, index }) => options.get(*index).cloned(),
            _ => self.base.default_model.clone(),
        }
    }

    fn current_request_timeout(&self) -> Option<u64> {
        match self.defaults.get(2).map(|f| &f.value) {
            Some(FieldValue::Number(n)) => *n,
            _ => self.base.request_timeout_secs,
        }
    }

    /// Set the concurrency default that suits the chosen provider.
    ///
    /// A local Ollama serves one model at a time, so eight concurrent
    /// inferences against it queue and thrash rather than going faster. Only
    /// applied while the field still holds the general default, so a number the
    /// user typed is never overwritten.
    pub fn apply_provider_concurrency_default(&mut self) {
        let ollama = self.current_default_provider() == "ollama";
        let general = Config::default().limits.max_concurrent_inferences;
        let local = Some(catalog::OLLAMA_MAX_CONCURRENT_INFERENCES as u64);
        let general = general.map(|n| n as u64);

        // `limits[0]` is the concurrency field, built as a `Number` by
        // `limits_fields`, so this is a total match rather than a fallible
        // lookup with an arm nothing can reach.
        let Some(FieldValue::Number(current)) = self.limits.first_mut().map(|f| &mut f.value)
        else {
            return;
        };
        if ollama && *current == general {
            *current = local;
        } else if !ollama && *current == local {
            *current = general;
        }
    }

    // ── Confirmations ───────────────────────────────────────────────────────

    /// `q`/Ctrl-C with unsaved choices: ask before discarding them.
    pub fn open_quit_confirm(&mut self) {
        use ratatui::text::Line;
        self.confirm = Some(PendingConfirm {
            purpose: ConfirmPurpose::QuitDiscard,
            dialog: crate::tui::widgets::confirm::Confirm::new(
                "Quit setup?",
                vec![Line::from(
                    "Nothing has been written yet. Your choices so far will be discarded.",
                )],
                "Quit",
                "Stay",
            ),
        });
    }

    /// Saving with the Claude Code transport selected: the terms risk is
    /// confirmed once, explicitly, on a dialog with real buttons.
    pub fn open_tos_confirm(&mut self) {
        use ratatui::text::Line;
        self.confirm = Some(PendingConfirm {
            purpose: ConfirmPurpose::SaveTos,
            dialog: crate::tui::widgets::confirm::Confirm::new(
                "Claude Code terms of service",
                vec![
                    Line::from("Anthropic's terms prohibit third-party developers from offering"),
                    Line::from("claude.ai subscription auth for their products without prior"),
                    Line::from("approval. The Claude Code transport routes inference through"),
                    Line::from("your subscription via the CLI's OAuth session."),
                    Line::from(""),
                    Line::from("For unambiguous compliance, use a direct Anthropic API key."),
                    Line::from(""),
                    Line::from("Accepting means you take responsibility for compliance."),
                ],
                "Accept and save",
                "Cancel",
            )
            .danger(),
        });
    }

    /// Leaving the Providers screen with nothing selected: Leviath cannot run
    /// an agent without a provider, so this is almost always a slip.
    pub fn open_no_providers_confirm(&mut self) {
        use ratatui::text::Line;
        self.confirm = Some(PendingConfirm {
            purpose: ConfirmPurpose::NoProviders,
            dialog: crate::tui::widgets::confirm::Confirm::new(
                "No providers selected",
                vec![
                    Line::from("Without a provider, Leviath cannot run any agent."),
                    Line::from("Select one with Space or Enter, or continue anyway to"),
                    Line::from("configure providers later."),
                ],
                "Continue anyway",
                "Go back",
            ),
        });
    }

    /// Commit an edited text buffer into wherever it belongs.
    pub fn commit_edit(&mut self) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        match edit.target {
            EditTarget::Credential(index) => {
                if let Some(row) = self.providers.get_mut(index) {
                    row.value = edit.line.value().trim().to_string();
                    // A typed credential replaces the environment's, and the
                    // row stops claiming the environment supplies it.
                    if !row.value.is_empty() {
                        row.from_env = None;
                    }
                    row.outcome = Outcome::Skipped;
                }
            }
            EditTarget::Field(index) => {
                let Some(fields) = self.fields_mut() else {
                    return;
                };
                if let Some(field) = fields.get_mut(index) {
                    match &mut field.value {
                        FieldValue::Number(n) => {
                            let trimmed = edit.line.value().trim();
                            *n = if trimmed.is_empty() {
                                None
                            } else {
                                trimmed.parse().ok().or(*n)
                            };
                        }
                        // Booleans and choices are never text-edited.
                        FieldValue::Bool(_) | FieldValue::Choice { .. } => {}
                    }
                }
            }
        }
    }

    // ── Producing the plan ──────────────────────────────────────────────────

    /// Fold every choice into the config that will be written.
    pub fn build_config(&self) -> Config {
        let mut config = self.base.clone();

        for row in &self.providers {
            match row.provider.credential {
                Credential::None => {}
                _ if !row.selected => catalog::set_credential(&mut config, row.provider.id, None),
                // An environment-supplied credential is left out of the file:
                // the user put it in their environment on purpose, and
                // `Config::load` already reads it back from there.
                _ if row.value.is_empty() => {
                    catalog::set_credential(&mut config, row.provider.id, None)
                }
                Credential::BaseUrl if row.value == catalog::DEFAULT_OLLAMA_URL => {
                    // Storing the default would pin it; leaving it unset lets
                    // the built-in default (and `$OLLAMA_HOST`) apply.
                    catalog::set_credential(&mut config, row.provider.id, None)
                }
                _ => catalog::set_credential(&mut config, row.provider.id, Some(row.value.clone())),
            }
        }

        // The transport is always in the table, so this reads its row when
        // selected rather than guarding a lookup that cannot miss.
        let transport = self
            .providers
            .iter()
            .find(|r| r.provider.id == "claude-code")
            .filter(|r| r.selected);
        config.providers.claude_code_enabled = transport.is_some();
        if let Some(row) = transport {
            config.providers.claude_code_effort =
                Some(effort_options()[row.effort.min(effort_options().len() - 1)].to_string());
        }

        config.default_provider = self.current_default_provider();
        config.default_model = self
            .current_default_model()
            .filter(|m| m != Self::NO_DEFAULT_MODEL);
        config.request_timeout_secs = self.current_request_timeout();

        apply_limits_fields(&mut config, &self.limits);

        for row in self.mcp.iter().filter(|r| r.selected) {
            let mut server = row.candidate.config.clone();
            server.name = row.name.clone();
            config.mcp_servers.push(server);
        }

        config
    }

    /// The plan this wizard describes.
    pub fn build_plan(&self) -> SetupPlan {
        SetupPlan {
            config: self.build_config(),
            agents: self
                .agents
                .iter()
                .filter(|r| r.selected)
                .map(|r| r.agent)
                .collect(),
        }
    }

    /// Lines for the review screen.
    pub fn review_lines(&self) -> Vec<String> {
        let plan = self.build_plan();
        let changes = super::plan::changes(&self.base, &plan);
        if changes.is_empty() {
            vec!["Nothing would change.".to_string()]
        } else {
            changes
        }
    }

    /// MCP rows carrying a credential copied verbatim out of another tool's
    /// config, which importing would duplicate into `~/.leviath/config.toml`.
    pub fn selected_inline_secrets(&self) -> Vec<String> {
        self.mcp
            .iter()
            .filter(|r| r.selected && !r.candidate.inline_secrets.is_empty())
            .map(|r| format!("{}: {}", r.name, r.candidate.inline_secrets.join(", ")))
            .collect()
    }
}

/// Merge every scan into the flat `(source, candidate)` list the wizard takes,
/// alongside the human-readable errors.
pub fn candidates_from_scans(scans: Vec<import::Scan>) -> (Vec<(String, Candidate)>, Vec<String>) {
    let mut candidates = Vec::new();
    let mut errors = Vec::new();
    for scan in scans {
        match scan.result {
            Ok(found) => candidates.extend(
                found
                    .into_iter()
                    .map(|c| (scan.source.display.to_string(), c)),
            ),
            Err(message) => errors.push(format!("{}: {message}", scan.source.display)),
        }
    }
    (candidates, errors)
}

/// Build an [`MCPServerConfig`] list from selected rows. Exposed for tests and
/// for any future non-terminal front-end.
pub fn selected_servers(rows: &[McpRow]) -> Vec<MCPServerConfig> {
    rows.iter()
        .filter(|r| r.selected)
        .map(|r| {
            let mut server = r.candidate.config.clone();
            server.name = r.name.clone();
            server
        })
        .collect()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::bundled::BUNDLED_AGENTS;

    /// A wizard over tempdirs and a fixed environment, with a browser opener
    /// that records rather than launches.
    pub(in crate::commands::setup) fn test_wizard(agents_dir: &std::path::Path) -> Wizard {
        Wizard::new(
            Config::default(),
            &|_| None,
            Vec::new(),
            Vec::new(),
            agents_dir,
            std::sync::Arc::new(|_| true),
        )
    }

    fn candidate(name: &str) -> Candidate {
        Candidate {
            config: MCPServerConfig::stdio(name, "npx", vec![]),
            scope: String::new(),
            inline_secrets: Vec::new(),
        }
    }

    // ─── Step ───────────────────────────────────────────────────────────────

    #[test]
    fn every_step_is_titled_and_ordered() {
        for (index, step) in Step::ALL.iter().enumerate() {
            assert!(!step.title().is_empty(), "{step:?} has no title");
            assert_eq!(step.index(), index);
        }
    }

    // ─── construction ───────────────────────────────────────────────────────

    #[test]
    fn a_fresh_install_starts_with_nothing_selected_and_every_agent_queued() {
        let dir = tempfile::tempdir().unwrap();

        let wizard = test_wizard(dir.path());

        assert_eq!(wizard.step, Step::Welcome);
        assert!(wizard.selected_providers().is_empty());
        assert_eq!(wizard.agents.len(), BUNDLED_AGENTS.len());
        assert!(
            wizard.agents.iter().all(|r| r.selected),
            "a fresh install should offer to install everything"
        );
        assert!(
            wizard
                .agents
                .iter()
                .all(|r| r.action == AgentAction::Install)
        );
    }

    #[test]
    fn already_installed_agents_are_listed_but_not_reselected() {
        let dir = tempfile::tempdir().unwrap();
        for agent in BUNDLED_AGENTS {
            crate::bundled::install_bundled(agent, dir.path()).unwrap();
        }

        let wizard = test_wizard(dir.path());

        assert!(
            wizard.agents.iter().all(|r| !r.selected),
            "nothing needs doing, so nothing should be pre-checked"
        );
    }

    #[test]
    fn a_configured_provider_starts_selected_with_its_credential() {
        let dir = tempfile::tempdir().unwrap();
        let base = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-stored".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };

        let wizard = Wizard::new(
            base,
            &|_| None,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        let row = wizard
            .providers
            .iter()
            .find(|r| r.provider.id == "anthropic")
            .expect("anthropic is in the table");
        assert!(row.selected);
        assert_eq!(row.value, "sk-ant-stored");
        assert!(row.from_env.is_none());
    }

    #[test]
    fn a_key_that_lives_only_in_the_environment_is_shown_and_never_written() {
        // The bug this closes: `Config::load` folds env keys into the struct,
        // so a wizard that re-serializes the whole thing silently writes a key
        // the user deliberately kept in their environment into
        // ~/.leviath/config.toml.
        let dir = tempfile::tempdir().unwrap();

        let wizard = Wizard::new(
            Config::default(),
            &|name| (name == "ANTHROPIC_API_KEY").then(|| "sk-ant-from-env".to_string()),
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        let row = wizard
            .providers
            .iter()
            .find(|r| r.provider.id == "anthropic")
            .expect("anthropic is in the table");
        assert!(row.selected, "the provider is usable, so it is selected");
        assert_eq!(row.from_env, Some("ANTHROPIC_API_KEY"));
        assert!(row.value.is_empty());

        let written = wizard.build_config();
        assert!(
            written.providers.anthropic_api_key.is_none(),
            "an environment-supplied key must not be copied into the config"
        );
    }

    #[test]
    fn a_stored_key_wins_over_the_environment() {
        // Both present: the file is what setup is editing, so that is what is
        // shown and kept.
        let dir = tempfile::tempdir().unwrap();
        let base = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-stored".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };

        let wizard = Wizard::new(
            base,
            &|_| Some("sk-ant-from-env".to_string()),
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        let row = &wizard.providers[0];
        assert!(row.from_env.is_none());
        assert_eq!(row.value, "sk-ant-stored");
    }

    #[test]
    fn an_empty_environment_variable_does_not_count_as_a_credential() {
        let dir = tempfile::tempdir().unwrap();

        let wizard = Wizard::new(
            Config::default(),
            &|_| Some(String::new()),
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        assert!(wizard.env_only.is_empty());
        assert!(wizard.selected_providers().is_empty());
    }

    // ─── MCP rows ───────────────────────────────────────────────────────────

    #[test]
    fn an_importable_server_is_preselected_and_named_as_found() {
        let dir = tempfile::tempdir().unwrap();

        let wizard = Wizard::new(
            Config::default(),
            &|_| None,
            vec![("Claude Code".to_string(), candidate("fs"))],
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        assert_eq!(wizard.mcp.len(), 1);
        assert!(wizard.mcp[0].selected);
        assert!(!wizard.mcp[0].collides);
        assert_eq!(wizard.mcp[0].name, "fs");
        assert_eq!(wizard.mcp[0].source, "Claude Code");
    }

    #[test]
    fn a_server_already_configured_is_offered_unchecked_under_a_free_name() {
        // The user already has it. Silently adding a second copy under a
        // suffixed name is not what "import" means.
        let dir = tempfile::tempdir().unwrap();
        let base = Config {
            mcp_servers: vec![MCPServerConfig::stdio("fs", "npx", vec![])],
            ..Config::default()
        };

        let wizard = Wizard::new(
            base,
            &|_| None,
            vec![("Cursor".to_string(), candidate("fs"))],
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        assert!(!wizard.mcp[0].selected);
        assert!(wizard.mcp[0].collides);
        assert_eq!(wizard.mcp[0].name, "fs-2");

        // Selecting it anyway stores it under the free name, leaving the
        // original alone.
        let mut wizard = wizard;
        wizard.mcp[0].selected = true;
        let config = wizard.build_config();
        let names: Vec<&str> = config.mcp_servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["fs", "fs-2"]);
    }

    #[test]
    fn selected_servers_renames_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = Wizard::new(
            Config::default(),
            &|_| None,
            vec![
                ("A".to_string(), candidate("keep")),
                ("B".to_string(), candidate("drop")),
            ],
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );
        wizard.mcp[1].selected = false;
        wizard.mcp[0].name = "renamed".to_string();

        let servers = selected_servers(&wizard.mcp);

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "renamed");
    }

    #[test]
    fn inline_secrets_are_reported_only_for_selected_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut secretive = candidate("leaky");
        secretive.inline_secrets = vec!["API_TOKEN".to_string()];

        let mut wizard = Wizard::new(
            Config::default(),
            &|_| None,
            vec![
                ("A".to_string(), secretive),
                ("B".to_string(), candidate("clean")),
            ],
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        assert_eq!(wizard.selected_inline_secrets(), vec!["leaky: API_TOKEN"]);
        wizard.mcp[0].selected = false;
        assert!(wizard.selected_inline_secrets().is_empty());
    }

    // ─── navigation ─────────────────────────────────────────────────────────

    #[test]
    fn the_cursor_stays_inside_the_current_step() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Providers);

        wizard.move_cursor(-5);
        assert_eq!(wizard.cursor, 0);
        wizard.move_cursor(100);
        assert_eq!(
            wizard.cursor,
            wizard.providers.len(),
            "clamped to the Continue button after the last row"
        );
        assert!(wizard.on_continue());
    }

    #[test]
    fn a_step_with_no_rows_pins_the_cursor_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Welcome);
        wizard.cursor = 4;

        wizard.move_cursor(1);

        assert_eq!(wizard.cursor, 0);
        assert_eq!(wizard.row_count(), 0);
    }

    #[test]
    fn the_continue_label_names_where_it_goes() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());

        wizard.enter(Step::Welcome);
        assert_eq!(wizard.continue_label(), "Get started");

        wizard.enter(Step::Providers);
        assert_eq!(wizard.continue_label(), "Continue (no providers selected)");
        wizard.providers[0].selected = true;
        wizard.providers[1].selected = true;
        assert_eq!(
            wizard.continue_label(),
            "Continue: Credentials (2 selected)"
        );

        // On the credential walk: the next selected provider, then the next step.
        wizard.enter(Step::ProviderDetail);
        assert_eq!(
            wizard.continue_label(),
            format!("Next: {}", wizard.providers[1].provider.display)
        );
        wizard.detail = 1;
        assert_eq!(wizard.continue_label(), "Continue: Defaults");

        wizard.enter(Step::Limits);
        assert_eq!(wizard.continue_label(), "Continue: Agents");

        wizard.enter(Step::Review);
        assert_eq!(wizard.continue_label(), "Apply and finish");
    }

    #[test]
    fn next_step_title_past_the_last_step_falls_back_to_review() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Review);
        assert_eq!(wizard.next_step_title(), "Review");
    }

    #[test]
    fn empty_discovery_steps_are_skipped_in_both_directions() {
        // Nobody should have to press Enter through "no MCP servers found" on a
        // clean machine, or through a credentials screen with no providers
        // picked.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        assert!(wizard.mcp.is_empty());

        wizard.enter(Step::Providers);
        wizard.next_step();
        assert_eq!(wizard.step, Step::Defaults, "credentials screen was empty");

        wizard.enter(Step::Agents);
        wizard.next_step();
        assert_eq!(wizard.step, Step::Review, "MCP screen was empty");

        wizard.prev_step();
        assert_eq!(wizard.step, Step::Agents);
    }

    #[test]
    fn a_nonempty_discovery_step_is_visited() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = Wizard::new(
            Config::default(),
            &|_| None,
            vec![("A".to_string(), candidate("fs"))],
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        wizard.enter(Step::Agents);
        wizard.next_step();

        assert_eq!(wizard.step, Step::Mcp);
    }

    #[test]
    fn a_scan_error_alone_is_enough_to_show_the_mcp_step() {
        // "We couldn't read your Zed config" is worth a screen even with no
        // servers to import.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = Wizard::new(
            Config::default(),
            &|_| None,
            Vec::new(),
            vec!["Zed: unreadable".to_string()],
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        wizard.enter(Step::Agents);
        wizard.next_step();

        assert_eq!(wizard.step, Step::Mcp);
    }

    #[test]
    fn the_first_step_has_nowhere_to_go_back_to() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());

        wizard.prev_step();

        assert_eq!(wizard.step, Step::Welcome);
    }

    #[test]
    fn advancing_past_the_last_step_stays_on_review() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Review);

        wizard.next_step();

        assert_eq!(wizard.step, Step::Review);
    }

    #[test]
    fn the_credential_screen_walks_the_selected_providers() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].selected = true;
        wizard.providers[1].selected = true;

        assert_eq!(wizard.detail_row(), Some(0));
        assert!(wizard.next_detail());
        assert_eq!(wizard.detail_row(), Some(1));
        assert!(!wizard.next_detail(), "there is no third provider");
        assert!(wizard.prev_detail());
        assert_eq!(wizard.detail_row(), Some(0));
        assert!(!wizard.prev_detail());
    }

    #[test]
    fn the_credential_screen_has_no_row_when_nothing_is_selected() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::ProviderDetail);

        assert!(wizard.detail_row().is_none());
        assert_eq!(wizard.row_count(), 0);
    }

    // ─── verification ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn verification_is_requested_for_a_provider_with_a_credential() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (mut requests, _replies) = wizard.take_verify_ends().expect("first take");
        wizard.providers[0].selected = true;
        wizard.providers[0].value = "sk-ant-x".to_string();

        wizard.request_verification(0);

        assert!(wizard.providers[0].checking);
        let request = requests.try_recv().expect("a request was queued");
        assert_eq!(request.provider_id, "anthropic");
        assert_eq!(request.creds.api_key.as_deref(), Some("sk-ant-x"));
    }

    #[tokio::test]
    async fn a_blank_api_key_is_not_queued_for_checking() {
        // Failing with "check the key" when none was given says nothing useful.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (mut requests, _replies) = wizard.take_verify_ends().expect("first take");
        wizard.providers[0].selected = true;

        wizard.request_verification(0);

        assert!(!wizard.providers[0].checking);
        assert_eq!(wizard.providers[0].outcome, Outcome::Skipped);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_environment_supplied_key_is_what_gets_checked() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = Wizard::new(
            Config::default(),
            &|name| (name == "ANTHROPIC_API_KEY").then(|| "sk-ant-env".to_string()),
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );
        let (mut requests, _replies) = wizard.take_verify_ends().expect("first take");

        wizard.request_verification(0);

        let request = requests.try_recv().expect("a request was queued");
        assert_eq!(request.creds.api_key.as_deref(), Some("sk-ant-env"));
    }

    #[tokio::test]
    async fn ollama_is_checked_by_url_with_no_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (mut requests, _replies) = wizard.take_verify_ends().expect("first take");
        let index = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");

        wizard.request_verification(index);
        let request = requests.try_recv().expect("a request was queued");
        assert!(request.creds.api_key.is_none());
        assert_eq!(
            request.creds.base_url.as_deref(),
            Some(catalog::DEFAULT_OLLAMA_URL),
            "an empty field means the default endpoint"
        );

        wizard.providers[index].value = "http://box:11434".to_string();
        wizard.request_verification(index);
        let request = requests.try_recv().expect("a second request was queued");
        assert_eq!(request.creds.base_url.as_deref(), Some("http://box:11434"));
    }

    #[tokio::test]
    async fn verify_all_covers_every_selected_provider() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (mut requests, _replies) = wizard.take_verify_ends().expect("first take");
        wizard.providers[0].selected = true;
        wizard.providers[0].value = "sk-ant".to_string();
        let ollama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");
        wizard.providers[ollama].selected = true;

        wizard.verify_all();

        let mut seen = Vec::new();
        while let Ok(request) = requests.try_recv() {
            seen.push(request.provider_id);
        }
        assert_eq!(seen, vec!["anthropic", "ollama"]);
    }

    #[tokio::test]
    async fn an_out_of_range_verification_request_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (mut requests, _replies) = wizard.take_verify_ends().expect("first take");

        wizard.request_verification(999);

        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn replies_land_on_the_right_provider_and_feed_the_model_picker() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (_requests, replies) = wizard.take_verify_ends().expect("first take");
        wizard.providers[0].selected = true;
        wizard.providers[0].checking = true;

        replies
            .send(VerifyReply {
                provider_id: "anthropic".to_string(),
                outcome: Outcome::Reachable {
                    models: vec!["claude-opus-5".to_string()],
                },
            })
            .unwrap();
        // A reply for something not in the table is ignored rather than panicking.
        replies
            .send(VerifyReply {
                provider_id: "not-a-provider".to_string(),
                outcome: Outcome::Skipped,
            })
            .unwrap();
        wizard.drain_verifications();

        assert!(!wizard.providers[0].checking);
        assert_eq!(wizard.discovered_models(), vec!["claude-opus-5"]);
    }

    #[tokio::test]
    async fn a_late_reply_refills_the_model_picker() {
        // Moving straight from a credential into Defaults gets there before the
        // check comes back, so the picker was built from an empty model list
        // and stayed that way - caught by driving the real TUI against a live
        // API key.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (_requests, replies) = wizard.take_verify_ends().expect("first take");
        wizard.providers[0].selected = true;
        wizard.enter(Step::Defaults);
        assert_eq!(
            wizard.defaults[1].value.options(),
            [Wizard::NO_DEFAULT_MODEL.to_string()],
            "nothing has been reported yet"
        );

        replies
            .send(VerifyReply {
                provider_id: "anthropic".to_string(),
                outcome: Outcome::Reachable {
                    models: vec!["claude-opus-5".to_string()],
                },
            })
            .unwrap();
        wizard.drain_verifications();

        assert!(
            wizard.defaults[1]
                .value
                .options()
                .contains(&"claude-opus-5".to_string()),
            "the picker should have refilled"
        );
    }

    #[tokio::test]
    async fn a_late_reply_does_not_disturb_another_screen() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (_requests, replies) = wizard.take_verify_ends().expect("first take");
        wizard.providers[0].selected = true;
        wizard.enter(Step::Limits);
        wizard.limits[0].value = FieldValue::Number(Some(3));

        replies
            .send(VerifyReply {
                provider_id: "anthropic".to_string(),
                outcome: Outcome::Reachable {
                    models: vec!["m".to_string()],
                },
            })
            .unwrap();
        wizard.drain_verifications();

        assert_eq!(wizard.limits[0].value, FieldValue::Number(Some(3)));
    }

    #[test]
    fn the_verification_channel_ends_can_only_be_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());

        assert!(wizard.take_verify_ends().is_some());
        assert!(wizard.take_verify_ends().is_none());
    }

    #[test]
    fn models_from_unselected_providers_are_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].outcome = Outcome::Reachable {
            models: vec!["hidden".to_string()],
        };

        assert!(wizard.discovered_models().is_empty());
    }

    // ─── forms ──────────────────────────────────────────────────────────────

    #[test]
    fn the_provider_choice_is_a_radio_over_what_was_actually_selected() {
        // A free-text prompt lets a typo through and only fails at the
        // first agent run.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let ollama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");
        wizard.providers[ollama].selected = true;
        wizard.enter(Step::Defaults);

        assert_eq!(wizard.defaults[0].value.options(), ["ollama".to_string()]);
    }

    #[test]
    fn the_provider_choice_falls_back_to_the_configured_one_when_nothing_is_picked() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Defaults);

        assert_eq!(
            wizard.defaults[0].value.display(),
            Config::default().default_provider
        );
    }

    #[test]
    fn the_model_picker_is_filled_from_verification_and_keeps_a_stored_value() {
        let dir = tempfile::tempdir().unwrap();
        let base = Config {
            default_model: Some("hand-typed".to_string()),
            ..Config::default()
        };
        let mut wizard = Wizard::new(
            base,
            &|_| None,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );
        wizard.providers[0].selected = true;
        wizard.providers[0].outcome = Outcome::Reachable {
            models: vec!["claude-opus-5".to_string()],
        };

        wizard.enter(Step::Defaults);

        let options = wizard.defaults[1].value.options();
        assert!(options.contains(&Wizard::NO_DEFAULT_MODEL.to_string()));
        assert!(options.contains(&"claude-opus-5".to_string()));
        assert_eq!(
            wizard.defaults[1].value.display(),
            "hand-typed",
            "a model already in the config must survive"
        );
    }

    #[test]
    fn only_a_choice_field_has_options() {
        assert_eq!(
            FieldValue::Choice {
                options: vec!["a".into()],
                index: 0
            }
            .options(),
            ["a".to_string()]
        );
        assert!(FieldValue::Number(Some(1)).options().is_empty());
        assert!(FieldValue::Bool(true).options().is_empty());
    }

    #[test]
    fn field_values_read_naturally() {
        assert_eq!(FieldValue::Number(None).display(), "(unset)");
        assert_eq!(FieldValue::Number(Some(7)).display(), "7");
        assert_eq!(FieldValue::Bool(true).display(), "yes");
        assert_eq!(FieldValue::Bool(false).display(), "no");
        assert_eq!(
            FieldValue::Choice {
                options: vec!["a".into()],
                index: 0
            }
            .display(),
            "a"
        );
        assert_eq!(
            FieldValue::Choice {
                options: vec![],
                index: 0
            }
            .display(),
            "(none)"
        );
    }

    /// Moving a choice is total: the chooser hands back an index and the
    /// field it came from takes it, while a kind with no list ignores it
    /// rather than making every caller check first.
    #[test]
    fn setting_an_index_moves_a_choice_and_leaves_other_kinds_alone() {
        let mut choice = FieldValue::Choice {
            options: vec!["a".into(), "b".into()],
            index: 0,
        };
        choice.set_index(1);
        assert_eq!(choice.display(), "b");

        let mut number = FieldValue::Number(Some(7));
        number.set_index(1);
        assert_eq!(number.display(), "7");
    }

    #[test]
    fn picking_ollama_drops_the_concurrency_default_to_one() {
        // A local box serves one model at a time; eight concurrent inferences
        // against one Ollama instance queue and thrash rather than going faster.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let ollama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");
        wizard.providers[ollama].selected = true;
        wizard.enter(Step::Defaults);

        wizard.apply_provider_concurrency_default();

        assert_eq!(
            wizard.limits[0].value,
            FieldValue::Number(Some(catalog::OLLAMA_MAX_CONCURRENT_INFERENCES as u64))
        );
    }

    #[test]
    fn ollama_as_the_only_provider_still_lowers_the_concurrency_limit() {
        // Regression: re-picking the default only when an arrow key moves the
        // provider choice misses this case. With Ollama the sole selection it
        // is already at index 0, no arrow is ever pressed, and the limit stays
        // at the hosted-API default of 8 - caught by driving the real TUI.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let ollama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");
        wizard.providers[ollama].selected = true;

        wizard.enter(Step::Defaults);

        assert_eq!(wizard.defaults[0].value.display(), "ollama");
        assert_eq!(
            wizard.build_config().limits.max_concurrent_inferences,
            Some(catalog::OLLAMA_MAX_CONCURRENT_INFERENCES)
        );
    }

    #[test]
    fn switching_back_off_ollama_restores_the_general_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let ollama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");
        wizard.providers[ollama].selected = true;
        wizard.enter(Step::Defaults);
        wizard.apply_provider_concurrency_default();

        wizard.providers[ollama].selected = false;
        wizard.providers[0].selected = true;
        wizard.rebuild_defaults();
        wizard.apply_provider_concurrency_default();

        assert_eq!(
            wizard.limits[0].value,
            FieldValue::Number(
                Config::default()
                    .limits
                    .max_concurrent_inferences
                    .map(|n| n as u64)
            )
        );
    }

    #[test]
    fn a_hand_typed_concurrency_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let ollama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");
        wizard.providers[ollama].selected = true;
        wizard.enter(Step::Defaults);
        wizard.limits[0].value = FieldValue::Number(Some(3));

        wizard.apply_provider_concurrency_default();

        assert_eq!(wizard.limits[0].value, FieldValue::Number(Some(3)));
    }

    // ─── editing ────────────────────────────────────────────────────────────

    #[test]
    fn committing_a_credential_clears_its_stale_verification() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].outcome = Outcome::Reachable {
            models: vec!["m".into()],
        };
        wizard.edit = Some(Edit {
            target: EditTarget::Credential(0),
            line: crate::tui::widgets::line_edit::LineEdit::new("  sk-ant-new  ".to_string(), true),
        });

        wizard.commit_edit();

        assert_eq!(wizard.providers[0].value, "sk-ant-new");
        assert_eq!(
            wizard.providers[0].outcome,
            Outcome::Skipped,
            "the old result was for a different key"
        );
        assert!(wizard.edit.is_none());
    }

    #[test]
    fn typing_a_credential_supersedes_the_environments() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = Wizard::new(
            Config::default(),
            &|_| Some("sk-ant-env".to_string()),
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );
        assert!(wizard.providers[0].from_env.is_some());
        wizard.edit = Some(Edit {
            target: EditTarget::Credential(0),
            line: crate::tui::widgets::line_edit::LineEdit::new("sk-ant-typed".to_string(), true),
        });

        wizard.commit_edit();

        assert!(wizard.providers[0].from_env.is_none());
        assert_eq!(
            wizard.build_config().providers.anthropic_api_key.as_deref(),
            Some("sk-ant-typed")
        );
    }

    #[test]
    fn committing_numbers_handles_blank_and_unparseable_input() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Limits);

        wizard.edit = Some(Edit {
            target: EditTarget::Field(0),
            line: crate::tui::widgets::line_edit::LineEdit::new("16".to_string(), false),
        });
        wizard.commit_edit();
        assert_eq!(wizard.limits[0].value, FieldValue::Number(Some(16)));

        // Garbage keeps the previous value rather than silently unsetting it.
        wizard.edit = Some(Edit {
            target: EditTarget::Field(0),
            line: crate::tui::widgets::line_edit::LineEdit::new("not a number".to_string(), false),
        });
        wizard.commit_edit();
        assert_eq!(wizard.limits[0].value, FieldValue::Number(Some(16)));

        // Blank means unset, which is a real and different choice.
        wizard.edit = Some(Edit {
            target: EditTarget::Field(0),
            line: crate::tui::widgets::line_edit::LineEdit::new("   ".to_string(), false),
        });
        wizard.commit_edit();
        assert_eq!(wizard.limits[0].value, FieldValue::Number(None));
    }

    #[test]
    fn committing_with_nothing_open_or_out_of_range_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.commit_edit();

        wizard.edit = Some(Edit {
            target: EditTarget::Credential(999),
            line: crate::tui::widgets::line_edit::LineEdit::new("x".to_string(), false),
        });
        wizard.commit_edit();

        wizard.enter(Step::Limits);
        wizard.edit = Some(Edit {
            target: EditTarget::Field(999),
            line: crate::tui::widgets::line_edit::LineEdit::new("x".to_string(), false),
        });
        wizard.commit_edit();

        // A step with no fields at all.
        wizard.enter(Step::Welcome);
        wizard.edit = Some(Edit {
            target: EditTarget::Field(0),
            line: crate::tui::widgets::line_edit::LineEdit::new("x".to_string(), false),
        });
        wizard.commit_edit();

        assert!(wizard.edit.is_none());
    }

    #[test]
    fn every_step_reports_a_sensible_row_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = Wizard::new(
            Config::default(),
            &|_| None,
            vec![("A".to_string(), candidate("fs"))],
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );
        wizard.providers[0].selected = true;

        for step in Step::ALL {
            wizard.enter(step);
            let rows = wizard.row_count();
            match step {
                Step::Welcome | Step::Review => assert_eq!(rows, 0, "{step:?}"),
                _ => assert!(rows > 0, "{step:?} has no rows"),
            }
            // Fields exist for exactly the two form screens.
            let fields = wizard.fields().len();
            match step {
                Step::Defaults | Step::Limits => assert_eq!(fields, rows, "{step:?}"),
                _ => assert_eq!(fields, 0, "{step:?} should have no fields"),
            }
        }
    }

    #[test]
    fn committing_onto_a_defaults_field_reaches_that_form_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].selected = true;
        wizard.enter(Step::Defaults);

        wizard.edit = Some(Edit {
            target: EditTarget::Field(2),
            line: crate::tui::widgets::line_edit::LineEdit::new("45".to_string(), false),
        });
        wizard.commit_edit();

        assert_eq!(wizard.defaults[2].value, FieldValue::Number(Some(45)));
        assert_eq!(wizard.build_config().request_timeout_secs, Some(45));
    }

    #[test]
    fn clearing_a_credential_leaves_the_environment_marker_alone() {
        // Blanking the field is how a user says "use what's in my environment".
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = Wizard::new(
            Config::default(),
            &|_| Some("sk-ant-env".to_string()),
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );
        wizard.edit = Some(Edit {
            target: EditTarget::Credential(0),
            line: crate::tui::widgets::line_edit::LineEdit::new("   ".to_string(), true),
        });

        wizard.commit_edit();

        assert!(wizard.providers[0].value.is_empty());
        assert_eq!(wizard.providers[0].from_env, Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn a_defaults_form_with_no_choice_field_falls_back_to_the_base_config() {
        // Defensive: a future reorder must not silently drop the setting.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.defaults[0].value = FieldValue::Bool(true);

        assert_eq!(
            wizard.build_config().default_provider,
            Config::default().default_provider
        );
    }

    #[test]
    fn an_empty_choice_list_falls_back_to_the_base_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.defaults[0].value = FieldValue::Choice {
            options: Vec::new(),
            index: 0,
        };

        assert_eq!(
            wizard.build_config().default_provider,
            Config::default().default_provider
        );
    }

    #[test]
    fn the_concurrency_default_is_left_alone_when_the_form_is_not_a_number() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.limits[0].value = FieldValue::Bool(true);

        wizard.apply_provider_concurrency_default();

        assert_eq!(wizard.limits[0].value, FieldValue::Bool(true));
    }

    #[test]
    fn a_text_buffer_committed_onto_a_toggle_leaves_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Limits);
        let before = wizard.limits[3].value.clone();

        wizard.edit = Some(Edit {
            target: EditTarget::Field(3),
            line: crate::tui::widgets::line_edit::LineEdit::new("yes".to_string(), false),
        });
        wizard.commit_edit();

        assert_eq!(wizard.limits[3].value, before);
    }

    // ─── building the config ────────────────────────────────────────────────

    #[test]
    fn deselecting_a_provider_clears_its_credential() {
        let dir = tempfile::tempdir().unwrap();
        let base = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-stored".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let mut wizard = Wizard::new(
            base,
            &|_| None,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        wizard.providers[0].selected = false;

        assert!(wizard.build_config().providers.anthropic_api_key.is_none());
    }

    #[test]
    fn ollamas_default_url_is_left_unset_rather_than_pinned() {
        // Storing the default would freeze it and shadow $OLLAMA_HOST.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let ollama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");
        wizard.providers[ollama].selected = true;
        wizard.providers[ollama].value = catalog::DEFAULT_OLLAMA_URL.to_string();

        assert!(wizard.build_config().ollama_base_url.is_none());

        wizard.providers[ollama].value = "http://box:11434".to_string();
        assert_eq!(
            wizard.build_config().ollama_base_url.as_deref(),
            Some("http://box:11434")
        );
    }

    #[test]
    fn the_claude_code_transport_carries_its_effort_only_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let index = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "claude-code")
            .expect("the transport is offered");

        assert!(!wizard.build_config().providers.claude_code_enabled);

        wizard.providers[index].selected = true;
        wizard.providers[index].effort = effort_options().len() - 1;
        let config = wizard.build_config();
        assert!(config.providers.claude_code_enabled);
        assert_eq!(
            config.providers.claude_code_effort.as_deref(),
            Some(*effort_options().last().expect("levels exist"))
        );
    }

    #[test]
    fn an_out_of_range_effort_index_clamps_rather_than_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let index = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "claude-code")
            .expect("the transport is offered");
        wizard.providers[index].selected = true;
        wizard.providers[index].effort = 99;

        let config = wizard.build_config();

        assert_eq!(
            config.providers.claude_code_effort.as_deref(),
            Some(*effort_options().last().expect("levels exist"))
        );
    }

    #[test]
    fn the_stored_effort_selects_the_matching_option() {
        let dir = tempfile::tempdir().unwrap();
        let base = Config {
            providers: crate::config::ProviderConfig {
                claude_code_effort: Some("max".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let wizard = Wizard::new(
            base,
            &|_| None,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        let index = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "claude-code")
            .expect("the transport is offered");
        assert_eq!(effort_options()[wizard.providers[index].effort], "max");
    }

    #[test]
    fn an_unrecognised_stored_effort_falls_back_to_the_first_level() {
        assert_eq!(effort_index(Some("not-a-level")), 0);
        assert_eq!(
            effort_options()[effort_index(None)],
            leviath_providers::claude_code::DEFAULT_EFFORT
        );
    }

    #[test]
    fn the_provider_default_model_is_stored_as_unset() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].selected = true;
        wizard.enter(Step::Defaults);

        // Index 0 of the model field is always the "no default" option.
        assert!(wizard.build_config().default_model.is_none());
    }

    #[test]
    fn limits_are_written_back_including_the_zero_guard() {
        // A zero here would deadlock every tool batch, so it falls back to the
        // default rather than being stored.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Limits);
        wizard.limits[0].value = FieldValue::Number(Some(2));
        wizard.limits[1].value = FieldValue::Number(Some(0));
        wizard.limits[2].value = FieldValue::Number(None);
        wizard.limits[3].value = FieldValue::Bool(true);
        wizard.limits[4].value = FieldValue::Bool(false);
        wizard.limits[5].value = FieldValue::Bool(false);
        wizard.limits[6].value = FieldValue::Number(Some(11));
        wizard.limits[7].value = FieldValue::Number(Some(22));
        wizard.limits[8].value = FieldValue::Number(Some(33));
        wizard.limits[9].value = FieldValue::Number(Some(44));

        let config = wizard.build_config();

        assert_eq!(config.limits.max_concurrent_inferences, Some(2));
        assert_eq!(
            config.limits.max_concurrent_tools,
            Config::default().limits.max_concurrent_tools
        );
        assert!(config.limits.default_max_iterations.is_none());
        assert!(config.limits.exact_token_counting);
        assert!(!config.batch_tool_hint);
        assert!(!config.shell_hint);
        // Every remaining field gets a distinct value, so a form that grows a
        // row without renumbering `apply_limits_fields` fails here rather than
        // silently dropping whichever field the duplicated index shadowed.
        assert_eq!(config.limits.stall_timeout_secs, 11);
        assert_eq!(config.limits.dead_cycles_before_relief, 22);
        assert_eq!(config.limits.finished_retention_secs, 33);
        assert_eq!(config.limits.wedge_timeout_secs, 44);
    }

    #[test]
    fn every_limits_field_is_written_back() {
        // The index in `apply_limits_fields` is positional and hand-written, so
        // an inserted row shifts every arm below it. Round-tripping the form
        // through itself catches a gap or a duplicate without naming indices.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Limits);
        let count = wizard.limits.len();

        let before = wizard.build_config();
        let seeded = limits_fields(&before);
        assert_eq!(seeded.len(), count, "the form is built from the config");

        // Flip every toggle and give every number a distinct non-default
        // value, then read the form back out of the config it produced: a
        // field that no arm writes comes back with its original value. The
        // limits form is toggles and numbers only, so the second arm is the
        // number case rather than an unexercised catch-all.
        for (i, field) in wizard.limits.iter_mut().enumerate() {
            field.value = match &field.value {
                FieldValue::Bool(b) => FieldValue::Bool(!b),
                _ => FieldValue::Number(Some(i as u64 + 11)),
            };
        }
        let expected: Vec<FieldValue> = wizard.limits.iter().map(|f| f.value.clone()).collect();
        let after = limits_fields(&wizard.build_config());

        for (i, (got, want)) in after.iter().zip(&expected).enumerate() {
            assert_eq!(
                &got.value, want,
                "field {i} ({}) did not survive the round trip",
                got.label
            );
        }
    }

    /// The four timing limits share one rule: an explicit number is stored,
    /// including `0` (which means "never" for each of them), while leaving a
    /// field blank keeps the shipped default rather than disabling anything.
    #[test]
    fn the_watchdog_limits_store_zero_and_keep_the_default_when_blank() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Limits);
        wizard.limits[6].value = FieldValue::Number(Some(0));
        wizard.limits[7].value = FieldValue::Number(Some(0));
        wizard.limits[8].value = FieldValue::Number(Some(0));
        wizard.limits[9].value = FieldValue::Number(Some(300));

        let config = wizard.build_config();

        assert_eq!(config.limits.stall_timeout_secs, 0);
        assert_eq!(config.limits.dead_cycles_before_relief, 0);
        assert_eq!(config.limits.finished_retention_secs, 0);
        assert_eq!(config.limits.wedge_timeout_secs, 300);

        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Limits);
        wizard.limits[6].value = FieldValue::Number(None);
        wizard.limits[7].value = FieldValue::Number(None);
        wizard.limits[8].value = FieldValue::Number(None);
        wizard.limits[9].value = FieldValue::Number(None);

        let config = wizard.build_config();

        let default = Config::default();
        assert_eq!(
            config.limits.stall_timeout_secs,
            default.limits.stall_timeout_secs
        );
        assert_eq!(
            config.limits.dead_cycles_before_relief,
            default.limits.dead_cycles_before_relief
        );
        assert_eq!(
            config.limits.finished_retention_secs,
            default.limits.finished_retention_secs
        );
        assert_eq!(
            config.limits.wedge_timeout_secs,
            default.limits.wedge_timeout_secs
        );
    }

    #[test]
    fn a_field_of_the_wrong_kind_is_ignored_when_writing_limits() {
        // Defensive: nothing builds this shape today, but a future edit that
        // reorders the form must not silently write a boolean into a count.
        let mut config = Config::default();
        apply_limits_fields(
            &mut config,
            &[Field {
                label: "Max concurrent inferences",
                help: "",
                value: FieldValue::Bool(true),
            }],
        );

        assert_eq!(
            config.limits.max_concurrent_inferences,
            Config::default().limits.max_concurrent_inferences
        );
    }

    #[test]
    fn the_plan_carries_only_the_selected_agents() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        for row in wizard.agents.iter_mut().skip(1) {
            row.selected = false;
        }

        let plan = wizard.build_plan();

        assert_eq!(plan.agents.len(), 1);
        assert_eq!(plan.agents[0].name, BUNDLED_AGENTS[0].name);
    }

    #[test]
    fn the_review_says_so_when_nothing_would_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        for row in wizard.agents.iter_mut() {
            row.selected = false;
        }

        assert_eq!(wizard.review_lines(), vec!["Nothing would change."]);
    }

    #[test]
    fn the_review_lists_real_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].selected = true;
        wizard.providers[0].value = "sk-ant-x".to_string();

        let lines = wizard.review_lines();

        assert!(lines.iter().any(|l| l.contains("credential set")));
        assert!(lines.iter().any(|l| l.contains("to install")));
    }

    // ─── scan merging ───────────────────────────────────────────────────────

    #[test]
    fn scans_flatten_into_candidates_and_labelled_errors() {
        let scans = vec![
            import::Scan {
                source: import::Source {
                    id: "a",
                    display: "Harness A",
                    path: std::path::PathBuf::from("/a"),
                    layout: import::Layout::ClaudeCode,
                    allows_comments: false,
                },
                result: Ok(vec![candidate("fs")]),
            },
            import::Scan {
                source: import::Source {
                    id: "b",
                    display: "Harness B",
                    path: std::path::PathBuf::from("/b"),
                    layout: import::Layout::CodexToml,
                    allows_comments: false,
                },
                result: Err("unreadable".to_string()),
            },
        ];

        let (candidates, errors) = candidates_from_scans(scans);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, "Harness A");
        assert_eq!(errors, vec!["Harness B: unreadable"]);
    }
}
