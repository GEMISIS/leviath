//! The wizard's state: what step we're on, what's been chosen, and how a
//! choice turns back into a [`SetupPlan`].
//!
//! Deliberately free of drawing and of key handling - those are `render` and
//! `input`. Everything here is ordinary data and pure transitions, so the whole
//! flow is testable without a terminal.

use std::collections::HashMap;

#[cfg(test)]
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
mod endpoints;
pub(crate) use endpoints::*;
mod lanes;
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
    /// The OpenAI-compatible endpoints, under whichever preset row each sits.
    pub endpoints: Vec<EndpointRow>,
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
    /// Where a browser sign-in is sent. Its own lane, for the reason
    /// [`SigninRequest`] gives.
    pub signin_tx: mpsc::UnboundedSender<SigninRequest>,
    signin_rx: Option<mpsc::UnboundedReceiver<SigninRequest>>,
    signin_reply_tx: mpsc::UnboundedSender<SigninEvent>,
    /// Where the sign-in lane reports, drained on the same tick as the checks.
    pub signin_reply_rx: mpsc::UnboundedReceiver<SigninEvent>,
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
    pub(crate) picker: Option<Picker>,
    /// Which Defaults field the open chooser is choosing for.
    pub(crate) picker_field: usize,
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

/// Who is signed in to `provider`, as a line to show, or `None` when nobody
/// is.
///
/// Read when the wizard is built; a sign-in taken from inside the wizard
/// updates the row from what the lane reports rather than by re-reading this.
///
/// A grant whose token carries no email still counts as signed in. It is the
/// grant that decides whether the provider can answer, and reporting "not
/// signed in" because a claim was missing would offer a second sign-in that
/// replaces a working one.
fn signed_in_as(path: Option<&std::path::Path>, provider: &str) -> Option<String> {
    let grant = leviath_providers::codex::ProviderAuthStore::load(path?)
        .ok()?
        .get(provider)
        .cloned()?;
    Some(super::signin::describe(&grant))
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
    pub(crate) fn new(
        base: Config,
        env_lookup: &dyn Fn(&str) -> Option<String>,
        candidates: Vec<(String, Candidate)>,
        scan_errors: Vec<String>,
        agents_dir: &std::path::Path,
        opener: leviath_mcp::BrowserOpener,
        remembered: crate::ui_state::SetupUi,
    ) -> Self {
        let env_only = env_credentials(env_lookup);

        let providers = catalog::providers()
            .into_iter()
            .map(|provider| {
                let stored = catalog::stored_credential(&base, provider.id);
                let from_env = provider
                    .env_var
                    .filter(|v| stored.is_none() && env_only.contains_key(v));
                let signed_in = match provider.credential {
                    catalog::Credential::Signin => signed_in_as(
                        leviath_providers::codex::ProviderAuthStore::default_path().as_deref(),
                        provider.id,
                    ),
                    _ => None,
                };
                ProviderRow {
                    selected: catalog::is_configured(&base, provider.id) || from_env.is_some(),
                    value: stored.unwrap_or_default(),
                    from_env,
                    outcome: Outcome::Skipped,
                    checking: false,
                    signed_in,
                    signing_in: false,
                    authorize_url: None,
                    provider,
                }
            })
            .collect();

        let agents = crate::bundled::plan_agent_actions(agents_dir)
            .into_iter()
            .map(|(agent, action)| AgentRow {
                // Still offered, just not pre-checked, when this exact version
                // was turned down before. Keyed by version so a newer bundled
                // blueprint is a fresh offer rather than something an old "no
                // thanks" keeps hidden.
                selected: action.preselect()
                    && remembered
                        .declined_agents
                        .get(agent.name)
                        .map(String::as_str)
                        != Some(agent.version),
                agent,
                action,
            })
            .collect();

        let mcp =
            candidates
                .into_iter()
                .map(|(source, candidate)| {
                    let collides =
                        import::already_configured(&base.mcp_servers, &candidate.config.name);
                    let name = import::dedup_name(&base.mcp_servers, &candidate.config.name);
                    McpRow {
                        // A server already configured under this name is offered
                        // unchecked: the user has it, and silently adding a second
                        // copy under a suffixed name is not what "import" means.
                        // So is one they have already said no to - still listed, so
                        // they can change their mind, but not proposed again.
                        selected: !collides
                            && !remembered.declined_mcp.contains(
                                &crate::ui_state::mcp_decline_key(&source, &candidate.config.name),
                            ),
                        source,
                        collides,
                        name,
                        candidate,
                    }
                })
                .collect();

        let (verify_tx, verify_rx) = mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = mpsc::unbounded_channel();
        let (signin_tx, signin_rx) = mpsc::unbounded_channel();
        let (signin_reply_tx, signin_reply_rx) = mpsc::unbounded_channel();

        let endpoints = Self::endpoints_from_config(&base);
        let mut wizard = Self {
            step: Step::Welcome,
            cursor: 0,
            providers,
            endpoints,
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
            signin_tx,
            signin_rx: Some(signin_rx),
            signin_reply_tx,
            signin_reply_rx,
            ticks: 0,
            scroll: 0,
            show_advanced: false,
            help_scroll: std::cell::Cell::new(0),
            picker: None,
            picker_field: 0,
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

    /// Hand the background sign-in lane its channel ends. Returns `None` if
    /// already taken.
    pub fn take_signin_ends(
        &mut self,
    ) -> Option<(
        mpsc::UnboundedReceiver<SigninRequest>,
        mpsc::UnboundedSender<SigninEvent>,
    )> {
        self.signin_rx
            .take()
            .map(|rx| (rx, self.signin_reply_tx.clone()))
    }

    // ── Rows and navigation ─────────────────────────────────────────────────

    /// Providers the user picked, in table order.
    pub(crate) fn selected_providers(&self) -> Vec<usize> {
        self.providers
            .iter()
            .enumerate()
            .filter(|(_, r)| r.selected)
            .map(|(i, _)| i)
            .collect()
    }

    /// The provider row the credential screen is currently showing.
    pub(crate) fn detail_row(&self) -> Option<usize> {
        self.selected_providers().get(self.detail).copied()
    }

    /// The fields the current step edits, if it edits fields.
    pub(crate) fn fields(&self) -> &[Field] {
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
    pub(crate) fn detail_actions(&self) -> Vec<DetailAction> {
        let Some(index) = self.detail_row() else {
            return Vec::new();
        };
        // An endpoint preset's screen is its entries' own rows, each with its
        // check and remove buttons; see `endpoint_row_count`.
        if self.is_endpoint_preset(index) {
            return Vec::new();
        }
        let row = &self.providers[index];
        if row.provider.credential == Credential::Signin {
            return Self::signin_actions(row);
        }
        let mut actions = Vec::new();
        if row.provider.signup_url.is_some() {
            actions.push(DetailAction::OpenSignup);
        }
        actions.push(DetailAction::Verify);
        actions
    }

    /// The buttons on a browser sign-in's screen, in the order they are
    /// offered.
    ///
    /// Nothing is typed here, so these are the whole screen and the first of
    /// them is what the cursor lands on. That decides the order: the thing
    /// somebody opening this card came to do goes first, which is the sign-in
    /// while there is none and the check once there is. Sign out and the plans
    /// page are both further down because neither is why anyone is here.
    ///
    /// Sign out is offered only when there is something to forget, and the
    /// check only when there is something to check: an unsigned-in row would
    /// answer "no credential" to a question the line above it already
    /// answered.
    fn signin_actions(row: &ProviderRow) -> Vec<DetailAction> {
        let mut actions = Vec::new();
        if row.signed_in.is_some() {
            actions.push(DetailAction::Verify);
        }
        actions.push(DetailAction::SignIn);
        if row.signed_in.is_some() {
            actions.push(DetailAction::SignOut);
        }
        // Where to get a subscription, for somebody who does not have one yet.
        actions.extend(row.provider.signup_url.map(|_| DetailAction::OpenSignup));
        actions
    }

    /// Whether the credential screen's first row is the credential itself.
    ///
    /// It is for everything typed or defaulted. A browser sign-in has nothing
    /// to type, so its status is a line rather than a row and the buttons
    /// start at the top: without this, Enter on a sign-in provider opened a
    /// text editor over a credential that does not exist.
    pub(crate) fn detail_has_credential_row(&self, index: usize) -> bool {
        self.providers[index].provider.credential != Credential::Signin
    }

    /// Which action the cursor is on, accounting for whether a credential row
    /// sits above them.
    pub(crate) fn detail_action_at(&self, index: usize, cursor: usize) -> Option<DetailAction> {
        let offset = usize::from(self.detail_has_credential_row(index));
        self.detail_actions()
            .get(cursor.checked_sub(offset)?)
            .copied()
    }

    /// How many selectable rows the current step has.
    pub(crate) fn row_count(&self) -> usize {
        match self.step {
            Step::Welcome | Step::Review => 0,
            Step::Providers => self.providers.len(),
            Step::ProviderDetail => match self.detail_row() {
                Some(index) if self.is_endpoint_preset(index) => self.endpoint_row_count(index),
                Some(index) => {
                    usize::from(self.detail_has_credential_row(index)) + self.detail_actions().len()
                }
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
    pub(crate) fn nav_rows(&self) -> usize {
        self.row_count() + 1
    }

    /// Whether the cursor sits on the step's Continue/action button (the
    /// virtual row after the last real one).
    pub(crate) fn on_continue(&self) -> bool {
        self.cursor == self.row_count()
    }

    /// The label of the current step's Continue/action button. It carries
    /// state (selection counts, what screen is next) so advancing is never a
    /// surprise.
    pub(crate) fn continue_label(&self) -> String {
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
    pub(crate) fn move_cursor(&mut self, delta: isize) {
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
    pub(crate) fn scroll_by(&mut self, rows: isize) {
        if self.row_count() > 0 {
            self.move_cursor(rows);
            return;
        }
        self.scroll = self.scroll.saturating_add_signed(rows);
    }

    /// Jump to the top of the current step.
    pub(crate) fn scroll_home(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Jump to the end of the current step, which is always its button.
    ///
    /// The offset is set past any possible content and clamped when drawn,
    /// because the number of lines a step occupies depends on the window it is
    /// drawn into and is not known here.
    pub(crate) fn scroll_end(&mut self) {
        self.cursor = self.nav_rows().saturating_sub(1);
        self.scroll = usize::MAX;
    }

    /// Advance to the next step, skipping ones with nothing to show. Skipping
    /// the credential screen is announced rather than silent: it looks exactly
    /// like a bug when a screen the breadcrumb promises never appears.
    pub(crate) fn next_step(&mut self) {
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
    pub(crate) fn prev_step(&mut self) {
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
    pub(crate) fn enter(&mut self, step: Step) {
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
    pub(crate) fn next_detail(&mut self) -> bool {
        if self.detail + 1 < self.selected_providers().len() {
            self.detail += 1;
            self.cursor = 0;
            self.edit = None;
            return true;
        }
        false
    }

    /// The reverse of [`Self::next_detail`].
    pub(crate) fn prev_detail(&mut self) -> bool {
        if self.detail > 0 {
            self.detail -= 1;
            self.cursor = 0;
            self.edit = None;
            return true;
        }
        false
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
    pub(crate) fn discovered_models(&self) -> Vec<String> {
        let mut models: Vec<String> = self
            .providers
            .iter()
            .filter(|r| r.selected)
            .flat_map(|r| r.outcome.models().iter().cloned())
            .collect();
        let selected = self.selected_endpoint_names();
        models.extend(
            self.endpoints
                .iter()
                .filter(|e| selected.contains(&e.name))
                .flat_map(|e| e.model_choices()),
        );
        models.sort();
        models.dedup();
        models
    }

    // ── Forms ───────────────────────────────────────────────────────────────

    /// Rebuild the Defaults screen. Called on entry, since both the provider
    /// list and the discovered models can change between visits.
    pub(crate) fn rebuild_defaults(&mut self) {
        let chosen = self.current_default_provider();
        // The built-ins by id, and an endpoint preset by each entry under it:
        // the entry's name is what `default_provider` has to hold.
        let providers: Vec<String> = self
            .selected_providers()
            .iter()
            .flat_map(|&i| match self.is_endpoint_preset(i) {
                true => self
                    .endpoints_under(self.providers[i].provider.id)
                    .into_iter()
                    .map(|e| self.endpoints[e].name.clone())
                    .collect(),
                false => vec![self.providers[i].provider.id.to_string()],
            })
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
        // An endpoint entry chosen as the default provider brings the model
        // picked on its own screen, unless a model was already chosen here.
        let chosen_provider = providers.get(index).cloned().unwrap_or_default();
        let current_model = self
            .current_default_model()
            .filter(|m| m != Self::NO_DEFAULT_MODEL)
            .or_else(|| self.endpoint_default_model(&chosen_provider))
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
    /// Worded as the blueprint's decision, never as a "provider default":
    /// no provider default model is consulted anywhere at run time. A stage
    /// that names no model of its own falls back to a model built into
    /// Leviath, not to anything the provider chose.
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
        self.picker_field = field;
        // Opening on the current value rather than at the top: the list is
        // long, and "where am I now" is the first thing you look for.
        self.picker = Some(Picker::new(
            title,
            Self::precedence_explanation(field == Self::PROVIDER_FIELD)
                .into_iter()
                .map(str::to_string)
                .collect(),
            options,
            index,
        ));
    }

    /// What a provider id is, for the chooser's second column.
    fn provider_detail(&self, id: &str) -> String {
        // Entries before rows: an entry is named after its preset by default,
        // and the preset row is never itself a choice here.
        if let Some(entry) = self.endpoints.iter().find(|e| e.name == id) {
            let preset = self
                .providers
                .iter()
                .find(|r| r.provider.id == entry.preset)
                .map_or(entry.preset, |r| r.provider.display);
            return format!("{preset} at {}", entry.base_url);
        }
        if let Some(row) = self.providers.iter().find(|r| r.provider.id == id) {
            return row.provider.display.to_string();
        }
        // A provider that is configured but not in the catalog: it came
        // from the config file, so it is still a legitimate choice.
        "from your config".to_string()
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

    /// Take the chooser's answer (an index into its options), writing it
    /// back into the field it came from.
    pub(super) fn commit_picker(&mut self, chosen: usize) {
        // The chooser's options were built from this field's, one for one and
        // in order, so the option index *is* the field's index. Indexing rather
        // than looking up: the field is where it was when the chooser opened,
        // and nothing rebuilds the form while one is on screen.
        self.defaults[self.picker_field].value.set_index(chosen);
        self.dirty = true;
        // The concurrency default follows the provider, so an Ollama-first
        // setup does not inherit a number meant for hosted APIs.
        if self.picker_field == Self::PROVIDER_FIELD {
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
    pub(crate) fn apply_provider_concurrency_default(&mut self) {
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
    pub(crate) fn open_quit_confirm(&mut self) {
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

    /// Leaving the Providers screen with nothing selected: Leviath cannot run
    /// an agent without a provider, so this is almost always a slip.
    pub(crate) fn open_no_providers_confirm(&mut self) {
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
    pub(crate) fn commit_edit(&mut self) {
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
            EditTarget::Endpoint { entry, field } => {
                self.commit_endpoint_edit(entry, field, edit.line.value());
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
    pub(crate) fn build_config(&self) -> Config {
        let mut config = self.base.clone();

        for row in &self.providers {
            match row.provider.credential {
                // Written below, from the entries rather than the row.
                Credential::Endpoint => {}
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

        // The Claude Code transport has no row here. Its keys
        // (`claude_code_enabled`, `claude_code_effort`, `claude_code_binary`)
        // are set by hand or by `lev setup --claude-code`, and the clone of
        // `base` above carries whatever the file already says through
        // untouched.

        self.write_endpoints(&mut config);

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
    pub(crate) fn build_plan(&self) -> SetupPlan {
        SetupPlan {
            config: self.build_config(),
            agents: self
                .agents
                .iter()
                .filter(|r| r.selected)
                .map(|r| r.agent)
                .collect(),
            declined: self.declined(),
        }
    }

    /// What was offered as a change and left unchecked.
    ///
    /// Only rows that were a real offer count. An MCP server that collides with
    /// one already configured, and a blueprint that is up to date or locally
    /// edited, are unchecked because *we* made them so - reading that back as
    /// the user's refusal would mean an up-to-date blueprint stayed unchecked
    /// forever once its next version arrived.
    fn declined(&self) -> crate::ui_state::SetupUi {
        crate::ui_state::SetupUi {
            declined_mcp: self
                .mcp
                .iter()
                .filter(|row| !row.selected && !row.collides)
                .map(|row| {
                    crate::ui_state::mcp_decline_key(&row.source, &row.candidate.config.name)
                })
                .collect(),
            declined_agents: self
                .agents
                .iter()
                .filter(|row| !row.selected && row.action.preselect())
                .map(|row| (row.agent.name.to_string(), row.agent.version.to_string()))
                .collect(),
        }
    }

    /// Lines for the review screen.
    pub(crate) fn review_lines(&self) -> Vec<String> {
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
    pub(crate) fn selected_inline_secrets(&self) -> Vec<String> {
        self.mcp
            .iter()
            .filter(|r| r.selected && !r.candidate.inline_secrets.is_empty())
            .map(|r| format!("{}: {}", r.name, r.candidate.inline_secrets.join(", ")))
            .collect()
    }
}

/// Merge every scan into the flat `(source, candidate)` list the wizard takes,
/// alongside the human-readable errors.
pub(crate) fn candidates_from_scans(
    scans: Vec<import::Scan>,
) -> (Vec<(String, Candidate)>, Vec<String>) {
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

/// Build an [`MCPServerConfig`] list from selected rows.
#[cfg(test)]
pub(crate) fn selected_servers(rows: &[McpRow]) -> Vec<MCPServerConfig> {
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
            Default::default(),
        )
    }

    // ─── remembering what was turned down ─────────────────────────────────

    /// A wizard offered one importable server, with `remembered` as whatever
    /// the last run recorded.
    fn wizard_offering_mcp(
        agents_dir: &std::path::Path,
        remembered: crate::ui_state::SetupUi,
    ) -> Wizard {
        Wizard::new(
            Config::default(),
            &|_| None,
            vec![("cursor".to_string(), candidate("linear"))],
            Vec::new(),
            agents_dir,
            std::sync::Arc::new(|_| true),
            remembered,
        )
    }

    /// The whole point: a server turned down last time is still listed, and
    /// still unchecked.
    #[test]
    fn a_declined_mcp_server_is_offered_again_but_not_preselected() {
        let dir = tempfile::tempdir().unwrap();

        let fresh = wizard_offering_mcp(dir.path(), Default::default());
        assert_eq!(fresh.mcp.len(), 1);
        assert!(fresh.mcp[0].selected, "a first-time offer is pre-checked");

        let mut remembered = crate::ui_state::SetupUi::default();
        remembered
            .declined_mcp
            .insert(crate::ui_state::mcp_decline_key("cursor", "linear"));
        let second = wizard_offering_mcp(dir.path(), remembered);
        assert_eq!(
            second.mcp.len(),
            1,
            "still shown, so it can be reconsidered"
        );
        assert!(!second.mcp[0].selected, "but not proposed again");
    }

    /// A decline recorded against a *different* source or name is not this
    /// server's decline.
    #[test]
    fn a_decline_is_scoped_to_the_source_it_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let mut remembered = crate::ui_state::SetupUi::default();
        remembered
            .declined_mcp
            .insert(crate::ui_state::mcp_decline_key("claude-code", "linear"));
        let w = wizard_offering_mcp(dir.path(), remembered);
        assert!(
            w.mcp[0].selected,
            "the same name from another harness is a different offer"
        );
    }

    /// Leaving an offered row unchecked is what gets recorded; a row that was
    /// never a real offer is not.
    #[test]
    fn the_plan_records_only_rows_that_were_genuinely_offered() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = wizard_offering_mcp(dir.path(), Default::default());
        w.mcp[0].selected = false;
        assert!(
            w.build_plan()
                .declined
                .declined_mcp
                .contains(&crate::ui_state::mcp_decline_key("cursor", "linear")),
            "unchecked and importable: a refusal"
        );

        // Unchecked because it collides with one already configured is our
        // doing, not the user's, and must not read as a refusal.
        w.mcp[0].collides = true;
        assert!(w.build_plan().declined.declined_mcp.is_empty());
    }

    /// A blueprint turned down stays unchecked - until a newer version makes
    /// it a different offer.
    #[test]
    fn a_declined_blueprint_is_re_offered_when_its_version_moves() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path();
        let offered = &BUNDLED_AGENTS[0];

        let mut remembered = crate::ui_state::SetupUi::default();
        remembered
            .declined_agents
            .insert(offered.name.to_string(), offered.version.to_string());
        let w = Wizard::new(
            Config::default(),
            &|_| None,
            Vec::new(),
            Vec::new(),
            agents_dir,
            std::sync::Arc::new(|_| true),
            remembered,
        );
        let row = w
            .agents
            .iter()
            .find(|r| r.agent.name == offered.name)
            .expect("the bundled agent is offered");
        assert!(
            row.action.preselect(),
            "nothing installed, so it is an offer"
        );
        assert!(!row.selected, "and it was turned down at this version");

        // A different version is a different offer.
        let mut moved_on = crate::ui_state::SetupUi::default();
        moved_on
            .declined_agents
            .insert(offered.name.to_string(), "0.0.0-ancient".to_string());
        let w = Wizard::new(
            Config::default(),
            &|_| None,
            Vec::new(),
            Vec::new(),
            agents_dir,
            std::sync::Arc::new(|_| true),
            moved_on,
        );
        let row = w
            .agents
            .iter()
            .find(|r| r.agent.name == offered.name)
            .expect("still offered");
        assert!(row.selected, "a newer version is asked about again");
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
            Default::default(),
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
    fn an_enabled_claude_code_transport_survives_the_wizard_untouched() {
        // The transport has no row, so the wizard must neither switch it off
        // nor rewrite its effort: the config keys pass straight through.
        let dir = tempfile::tempdir().unwrap();
        let base = Config {
            providers: crate::config::ProviderConfig {
                claude_code_enabled: true,
                claude_code_effort: Some("max".to_string()),
                claude_code_binary: Some("/opt/claude".into()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let mut wizard = Wizard::new(
            base.clone(),
            &|_| None,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        assert!(
            wizard
                .providers
                .iter()
                .all(|r| r.provider.id != "claude-code")
        );

        // Touching other providers does not disturb it either.
        wizard.providers[0].selected = true;
        wizard.providers[0].value = "sk-ant-x".to_string();
        let config = wizard.build_config();

        assert!(config.providers.claude_code_enabled);
        assert_eq!(
            config.providers.claude_code_effort,
            base.providers.claude_code_effort
        );
        assert_eq!(
            config.providers.claude_code_binary,
            base.providers.claude_code_binary
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
        wizard.limits[3].value = FieldValue::Bool(false);
        wizard.limits[4].value = FieldValue::Bool(false);
        wizard.limits[5].value = FieldValue::Number(Some(11));
        wizard.limits[6].value = FieldValue::Number(Some(22));
        wizard.limits[7].value = FieldValue::Number(Some(33));
        wizard.limits[8].value = FieldValue::Number(Some(44));

        let config = wizard.build_config();

        assert_eq!(config.limits.max_concurrent_inferences, Some(2));
        assert_eq!(
            config.limits.max_concurrent_tools,
            Config::default().limits.max_concurrent_tools
        );
        assert!(config.limits.default_max_iterations.is_none());
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
        wizard.limits[5].value = FieldValue::Number(Some(0));
        wizard.limits[6].value = FieldValue::Number(Some(0));
        wizard.limits[7].value = FieldValue::Number(Some(0));
        wizard.limits[8].value = FieldValue::Number(Some(300));

        let config = wizard.build_config();

        assert_eq!(config.limits.stall_timeout_secs, 0);
        assert_eq!(config.limits.dead_cycles_before_relief, 0);
        assert_eq!(config.limits.finished_retention_secs, 0);
        assert_eq!(config.limits.wedge_timeout_secs, 300);

        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Limits);
        wizard.limits[5].value = FieldValue::Number(None);
        wizard.limits[6].value = FieldValue::Number(None);
        wizard.limits[7].value = FieldValue::Number(None);
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

    /// `lev setup` writes no interaction timeout unless the user types one.
    /// The field is offered blank; blank stays unset (the run waits for a
    /// person), and the written config carries no line for it, so a fresh
    /// install is not handed a deadline it never asked for.
    #[test]
    fn setup_leaves_the_interaction_timeout_unset_unless_the_user_sets_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Limits);
        assert_eq!(
            wizard.limits[9].value,
            FieldValue::Number(None),
            "the field is offered blank"
        );

        let config = wizard.build_config();
        assert_eq!(config.limits.interaction_timeout_secs, None);
        let written = toml::to_string_pretty(&config).unwrap();
        assert!(
            !written.contains("interaction_timeout_secs"),
            "nothing written for an unset timeout: {written}"
        );

        wizard.limits[9].value = FieldValue::Number(Some(900));
        assert_eq!(
            wizard.build_config().limits.interaction_timeout_secs,
            Some(900),
            "a number the user typed is stored"
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
                    display: "Harness A",
                    path: std::path::PathBuf::from("/a"),
                    layout: import::Layout::ClaudeCode,
                    allows_comments: false,
                },
                result: Ok(vec![candidate("fs")]),
            },
            import::Scan {
                source: import::Source {
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

    // ─── browser sign-ins ───────────────────────────────────────────────────

    /// The row reads the grant store, so the card can say who is signed in
    /// rather than only that the provider is selected.
    #[test]
    fn a_sign_in_row_reports_the_account_from_the_grant_store() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || {
            let path =
                leviath_providers::codex::ProviderAuthStore::default_path().expect("a home is set");
            std::fs::create_dir_all(path.parent().expect("a parent")).unwrap();
            // Nobody signed in, nowhere to look, and a file that will not parse
            // are all the same answer: nothing to report.
            assert_eq!(signed_in_as(Some(&path), "codex"), None);
            assert_eq!(signed_in_as(None, "codex"), None);
            std::fs::write(&path, "{ not json").unwrap();
            assert_eq!(signed_in_as(Some(&path), "codex"), None);
            let mut store = leviath_providers::codex::ProviderAuthStore::default();
            store.set(
                "codex",
                leviath_providers::ProviderGrant {
                    access_token: "at".to_string(),
                    refresh_token: "rt".to_string(),
                    email: Some("someone@example.com".to_string()),
                    plan_type: Some("plus".to_string()),
                    ..Default::default()
                },
            );
            store.save(&path).unwrap();
            assert_eq!(
                signed_in_as(Some(&path), "codex").as_deref(),
                Some("someone@example.com (plus plan)")
            );

            // A grant with an account but no plan says the account alone.
            let mut store = leviath_providers::codex::ProviderAuthStore::default();
            store.set(
                "codex",
                leviath_providers::ProviderGrant {
                    access_token: "at".to_string(),
                    refresh_token: "rt".to_string(),
                    email: Some("someone@example.com".to_string()),
                    ..Default::default()
                },
            );
            store.save(&path).unwrap();
            assert_eq!(
                signed_in_as(Some(&path), "codex").as_deref(),
                Some("someone@example.com")
            );

            // A grant with no account at all still counts as signed in. The
            // grant is what lets the provider answer, and reporting "not
            // signed in" over a missing claim would offer to replace a
            // working sign-in.
            let mut store = leviath_providers::codex::ProviderAuthStore::default();
            store.set(
                "codex",
                leviath_providers::ProviderGrant {
                    access_token: "at".to_string(),
                    refresh_token: "rt".to_string(),
                    ..Default::default()
                },
            );
            store.save(&path).unwrap();
            assert_eq!(
                signed_in_as(Some(&path), "codex").as_deref(),
                Some("signed in")
            );

            // And a provider nobody has signed in to.
            assert_eq!(signed_in_as(Some(&path), "someone-else"), None);
        });
    }

    /// A sign-in row is checkable only once a grant exists: there is nothing
    /// typed into it to verify.
    #[test]
    fn a_sign_in_row_is_checkable_only_when_it_is_signed_in() {
        let dir = tempfile::tempdir().unwrap();
        let wizard = test_wizard(dir.path());
        let index = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "codex")
            .expect("the codex row is offered");

        let mut row = wizard.providers[index].clone();
        // Set explicitly rather than assumed: `Wizard::new` reads the grant
        // store under a process-wide `$LEVIATH_HOME`, so what this row arrives
        // holding depends on what else is running.
        row.signed_in = None;
        assert!(!row.has_credential());
        row.signed_in = Some("someone@example.com (plus plan)".to_string());
        assert!(row.has_credential());
    }

    /// With nothing selected the credential screen has no provider to show,
    /// so it offers no buttons rather than the first row's.
    #[test]
    fn a_credential_screen_with_no_provider_offers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        for row in &mut wizard.providers {
            row.selected = false;
        }
        wizard.step = Step::ProviderDetail;

        assert!(wizard.detail_actions().is_empty());
        assert_eq!(wizard.row_count(), 0);
    }

    /// The codex row at its index, on a wizard whose only selection it is and
    /// which is signed in to nothing.
    ///
    /// `signed_in` is cleared rather than trusted. `Wizard::new` reads the
    /// grant store under `$LEVIATH_HOME`, `temp_env` sets that for the whole
    /// process, and another test writing a grant into its own temp home is
    /// visible here while it runs. Depending on that made this fail on
    /// whichever platform lost the race - Windows, as it happened.
    fn wizard_showing_codex(agents_dir: &std::path::Path) -> (Wizard, usize) {
        let mut wizard = test_wizard(agents_dir);
        let index = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "codex")
            .expect("the codex row is offered");
        for row in &mut wizard.providers {
            row.selected = false;
            row.signed_in = None;
        }
        wizard.providers[index].selected = true;
        wizard.step = Step::ProviderDetail;
        (wizard, index)
    }

    /// The buttons on offer follow the sign-in, because two of the three do
    /// nothing without one.
    #[test]
    fn a_sign_in_row_offers_more_once_it_is_signed_in() {
        let dir = tempfile::tempdir().unwrap();
        let (mut wizard, index) = wizard_showing_codex(dir.path());

        assert_eq!(
            wizard.detail_actions(),
            vec![DetailAction::SignIn, DetailAction::OpenSignup]
        );
        // No credential row, so the buttons are the whole screen, and the
        // first is the one this screen exists for.
        assert!(!wizard.detail_has_credential_row(index));
        assert_eq!(wizard.row_count(), 2);
        assert_eq!(
            wizard.detail_action_at(index, 0),
            Some(DetailAction::SignIn)
        );

        wizard.providers[index].signed_in = Some("a@b.c".to_string());
        assert_eq!(
            wizard.detail_actions(),
            vec![
                DetailAction::Verify,
                DetailAction::SignIn,
                DetailAction::SignOut,
                DetailAction::OpenSignup,
            ],
            "once signed in, checking it is what somebody came here to do"
        );
        assert_eq!(wizard.row_count(), 4);
    }

    /// A typed provider still has its credential above the buttons, so the
    /// offset is not a blanket change.
    #[test]
    fn a_typed_row_still_starts_with_its_credential() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let index = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "anthropic")
            .expect("the anthropic row is offered");
        for row in &mut wizard.providers {
            row.selected = false;
        }
        wizard.providers[index].selected = true;
        wizard.step = Step::ProviderDetail;

        assert!(wizard.detail_has_credential_row(index));
        assert_eq!(wizard.detail_action_at(index, 0), None, "row 0 is the key");
        assert_eq!(
            wizard.detail_action_at(index, 1),
            Some(DetailAction::OpenSignup)
        );
        assert_eq!(wizard.row_count(), 3);
    }

    /// Asking to sign in puts the row into its waiting state at the key press
    /// rather than a tick later, selects the provider, and sends the request.
    #[test]
    fn requesting_a_sign_in_shows_it_waiting_and_sends_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut wizard, index) = wizard_showing_codex(dir.path());
        wizard.providers[index].selected = false;
        wizard.providers[index].outcome = Outcome::Reachable {
            models: vec!["gpt-5.6-sol".to_string()],
        };
        let (mut requests, _events) = wizard.take_signin_ends().expect("first take");

        wizard.request_signin(index, SigninAction::In);

        assert!(wizard.providers[index].signing_in);
        assert!(
            wizard.providers[index].selected,
            "signing in says what a checkbox says, only louder"
        );
        assert!(wizard.dirty);
        assert_eq!(
            wizard.providers[index].outcome,
            Outcome::Skipped,
            "the old check described the account being replaced"
        );
        let request = requests.try_recv().expect("the lane was asked");
        assert_eq!(request.provider_id, "codex");
        assert_eq!(request.action, SigninAction::In);
    }

    /// A sign-out asks without claiming the browser is open.
    #[test]
    fn requesting_a_sign_out_does_not_show_a_browser_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let (mut wizard, index) = wizard_showing_codex(dir.path());
        let (mut requests, _events) = wizard.take_signin_ends().expect("first take");

        wizard.request_signin(index, SigninAction::Out);

        assert!(!wizard.providers[index].signing_in);
        assert_eq!(
            requests.try_recv().expect("the lane was asked").action,
            SigninAction::Out
        );
    }

    /// An index no row has is a no-op rather than a panic.
    #[test]
    fn an_out_of_range_sign_in_request_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let (mut wizard, _) = wizard_showing_codex(dir.path());
        let (mut requests, _events) = wizard.take_signin_ends().expect("first take");
        wizard.request_signin(999, SigninAction::In);
        assert!(requests.try_recv().is_err());
    }

    /// The lane's ends can only be taken once, like the verifier's.
    #[test]
    fn the_sign_in_channel_ends_can_only_be_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        let (mut wizard, _) = wizard_showing_codex(dir.path());
        assert!(wizard.take_signin_ends().is_some());
        assert!(wizard.take_signin_ends().is_none());
    }

    /// What the lane reports lands on the row: the URL while it waits, then
    /// the identity, and the waiting state clears either way.
    #[test]
    fn a_finished_sign_in_settles_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let (mut wizard, index) = wizard_showing_codex(dir.path());
        let (_requests, events) = wizard.take_signin_ends().expect("first take");
        wizard.providers[index].signing_in = true;

        events
            .send(SigninEvent::Opened {
                provider_id: "codex".to_string(),
                url: "https://auth.example/go".to_string(),
            })
            .unwrap();
        wizard.drain_signins();
        assert_eq!(
            wizard.providers[index].authorize_url.as_deref(),
            Some("https://auth.example/go")
        );
        assert!(wizard.providers[index].signing_in, "still waiting");

        events
            .send(SigninEvent::SignedIn {
                provider_id: "codex".to_string(),
                who: "a@b.c (plus plan)".to_string(),
            })
            .unwrap();
        wizard.drain_signins();
        assert!(!wizard.providers[index].signing_in);
        assert_eq!(wizard.providers[index].authorize_url, None);
        assert_eq!(
            wizard.providers[index].signed_in.as_deref(),
            Some("a@b.c (plus plan)")
        );
    }

    /// A sign-out clears the identity, and a failure lands where a failed
    /// check would so the card has one place to look.
    #[test]
    fn a_sign_out_and_a_failure_both_settle_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let (mut wizard, index) = wizard_showing_codex(dir.path());
        let (_requests, events) = wizard.take_signin_ends().expect("first take");
        wizard.providers[index].signed_in = Some("a@b.c".to_string());
        wizard.providers[index].signing_in = true;

        events
            .send(SigninEvent::SignedOut {
                provider_id: "codex".to_string(),
            })
            .unwrap();
        wizard.drain_signins();
        assert_eq!(wizard.providers[index].signed_in, None);
        assert!(!wizard.providers[index].signing_in);

        wizard.providers[index].signing_in = true;
        events
            .send(SigninEvent::Failed {
                provider_id: "codex".to_string(),
                message: "could not listen on port 1455".to_string(),
            })
            .unwrap();
        wizard.drain_signins();
        assert!(!wizard.providers[index].signing_in);
        assert_eq!(
            wizard.providers[index].outcome,
            Outcome::Failed {
                message: "could not listen on port 1455".to_string()
            }
        );
    }

    /// An event for a row that is not on this wizard is dropped rather than
    /// landing on whichever row happens to be first.
    #[test]
    fn an_event_for_an_unknown_provider_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (mut wizard, _) = wizard_showing_codex(dir.path());
        let (_requests, events) = wizard.take_signin_ends().expect("first take");
        events
            .send(SigninEvent::SignedIn {
                provider_id: "not-a-provider".to_string(),
                who: "nobody".to_string(),
            })
            .unwrap();
        let before: Vec<Option<String>> = wizard
            .providers
            .iter()
            .map(|r| r.signed_in.clone())
            .collect();

        wizard.drain_signins();

        let after: Vec<Option<String>> = wizard
            .providers
            .iter()
            .map(|r| r.signed_in.clone())
            .collect();
        assert_eq!(before, after, "an event landed on a row it was not for");
    }

    /// A sign-in provider is checked through the grant on disk, so the
    /// request has to carry where that grant is - the registry refuses to
    /// guess, and a check with no path would report the provider missing.
    #[test]
    fn checking_a_sign_in_provider_says_where_its_grant_lives() {
        let dir = tempfile::tempdir().unwrap();
        let (mut wizard, index) = wizard_showing_codex(dir.path());
        wizard.providers[index].signed_in = Some("a@b.c".to_string());
        let (mut requests, _replies) = wizard.take_verify_ends().expect("first take");

        wizard.request_verification(index);

        let request = requests.try_recv().expect("the verifier was asked");
        assert_eq!(request.creds.name, "codex");
        assert!(
            request.creds.api_key.is_none(),
            "a sign-in provider has no key to send"
        );
        // Bound rather than formatted inline: a call that only runs when the
        // assertion fails is a region no passing test reaches.
        let named: Vec<&String> = request.creds.options.keys().collect();
        assert!(
            request.creds.options.contains_key("auth_store_path"),
            "{named:?}"
        );
    }
}
