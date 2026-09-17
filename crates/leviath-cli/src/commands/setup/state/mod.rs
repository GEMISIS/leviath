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
mod modal;
mod models;
mod priority;
mod retention;
pub(crate) use endpoints::*;
pub(crate) use modal::ProviderModal;
pub(in crate::commands::setup) mod checks;
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
    /// A provider's setup modal, while one is open over the Providers screen.
    pub(crate) modal: Option<ProviderModal>,
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
    /// The AWS region the environment names (`AWS_REGION`, else
    /// `AWS_DEFAULT_REGION`), which is what Bedrock is called in when the
    /// config says nothing. Shown as the region field's starting value and,
    /// like a credential from the environment, never written back.
    pub region_from_env: Option<String>,
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
    /// The open chooser, if one is open: a Defaults value, or a level of the
    /// add-a-provider flow.
    pub(crate) picker: Option<Picker>,
    /// What the open chooser is choosing, so its answer can be routed.
    pub(crate) picker_purpose: PickerPurpose,
    /// The open reorder modal for the provider priority, if one is open.
    pub(crate) reorder: Option<crate::tui::widgets::reorder::Reorder>,
    /// Which Defaults field the open reorder modal is arranging.
    pub(crate) reorder_field: usize,
    /// Where provider checks are recorded for other surfaces to read (the
    /// capability cache). `None` reads and writes nothing, which is what
    /// every test gets. See [`checks`].
    pub(crate) check_store: Option<std::path::PathBuf>,
    /// The key checks are fingerprinted with, beside `check_store`.
    pub(crate) check_key: Option<crate::provider_checks::CheckKey>,
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
    let grant = leviath_providers::oauth::ProviderAuthStore::load(path?)
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
        let region_from_env = env_lookup("AWS_REGION")
            .or_else(|| env_lookup("AWS_DEFAULT_REGION"))
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());

        let providers = catalog::providers()
            .into_iter()
            .map(|provider| {
                let stored = catalog::stored_credential(&base, provider.id);
                let from_env = provider
                    .env_var
                    .filter(|v| stored.is_none() && env_only.contains_key(v));
                let signed_in = match provider.credential {
                    catalog::Credential::Signin => signed_in_as(
                        leviath_providers::oauth::ProviderAuthStore::default_path().as_deref(),
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
                    checked_at: None,
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
            modal: None,
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
            region_from_env,
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
            picker_purpose: PickerPurpose::Field(0),
            reorder: None,
            reorder_field: 0,
            check_store: None,
            check_key: None,
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

    /// The provider row whose setup modal is open, if one is.
    pub(crate) fn detail_row(&self) -> Option<usize> {
        self.modal_index()
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

    /// The setup modal's action rows, after the credential itself and before
    /// its buttons.
    ///
    /// They exist as rows rather than as shortcut keys alone because that is
    /// how they become discoverable: a row can be seen, moved onto, and
    /// clicked, and `o` still works for anyone who knows it. Checking the
    /// credential is the modal's own "Verify and use" button.
    pub(crate) fn detail_actions(&self) -> Vec<DetailAction> {
        let Some(index) = self.detail_row() else {
            return Vec::new();
        };
        // An endpoint preset's card is its entries' own rows, each with its
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
    /// Sign out is offered only when there is something to forget.
    fn signin_actions(row: &ProviderRow) -> Vec<DetailAction> {
        let mut actions = vec![DetailAction::SignIn];
        if row.signed_in.is_some() {
            actions.push(DetailAction::SignOut);
        }
        // Where to get a subscription, for somebody who does not have one yet.
        actions.extend(row.provider.signup_url.map(|_| DetailAction::OpenSignup));
        actions
    }

    /// Whether the setup modal's first card row is the credential itself.
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

    /// How many selectable rows the current step has, or, while a provider's
    /// setup modal is open, how many its card has above its buttons.
    pub(crate) fn row_count(&self) -> usize {
        if self.modal.is_some() {
            return self.modal_card_rows();
        }
        match self.step {
            Step::Welcome | Step::Review => 0,
            // The providers this install has, then the "Add a provider" row.
            Step::Providers => self.visible_providers().len() + 1,
            Step::Defaults => self.defaults.len(),
            Step::Limits => self.limits.len(),
            Step::Agents => self.agents.len(),
            Step::Mcp => self.mcp.len(),
        }
    }

    /// How many cursor positions there are: the step's rows plus the
    /// Continue/action button every step ends with, or the modal's card rows
    /// plus its three buttons.
    pub(crate) fn nav_rows(&self) -> usize {
        match self.modal.is_some() {
            true => self.row_count() + ModalButton::ALL.len(),
            false => self.row_count() + 1,
        }
    }

    /// Whether the cursor sits on the step's Continue/action button (the
    /// virtual row after the last real one). Never while a modal is open:
    /// its buttons are its own.
    pub(crate) fn on_continue(&self) -> bool {
        self.modal.is_none() && self.cursor == self.row_count()
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
                    "Continue (add a provider first)".to_string()
                } else {
                    format!("Continue: {} ({count} configured)", self.next_step_title())
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

    /// Advance to the next step, skipping ones with nothing to show.
    pub(crate) fn next_step(&mut self) {
        let mut index = self.step.index();
        while index + 1 < Step::ALL.len() {
            index += 1;
            let step = Step::ALL[index];
            if !self.is_empty_step(step) {
                self.enter(step);
                return;
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
    /// Only the discovery-driven screen can be empty: nobody should have to
    /// press Enter through "no MCP servers found" on a clean machine.
    fn is_empty_step(&self, step: Step) -> bool {
        match step {
            Step::Mcp => self.mcp.is_empty() && self.mcp_scan_errors.is_empty(),
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
            // The provider priority is populated by verification, which may
            // have finished since the last visit.
            self.rebuild_defaults();
        }
        if step == Step::Limits {
            // Same for the two model choices at the end of the tuning screen.
            self.rebuild_advanced_models();
        }
    }

    // ── Forms ───────────────────────────────────────────────────────────────

    /// Rebuild the Defaults screen. Called on entry, since both the provider
    /// list and the discovered models can change between visits.
    pub(crate) fn rebuild_defaults(&mut self) {
        let chosen = self.current_default_provider();
        let providers = self.configured_provider_names();
        // Fall back to whatever is configured when nothing is selected, so the
        // field is never empty.
        let providers = if providers.is_empty() {
            vec![self.base.default_provider.clone()]
        } else {
            providers
        };
        // The provider field is an ordered priority, its head the default
        // provider. Preserve whatever arrangement the form (or the config it
        // loaded) already holds for still-configured providers. A provider
        // configured since is NOT added on its own: being in the priority is
        // a choice made in the reorder modal, and a provider left out of it
        // still runs any stage that names it. The list is never left empty
        // while something is configured, because its head is the default
        // provider: on a first build it is the chosen default, else the first
        // configured provider.
        let prior = match self.current_provider_order().is_empty() {
            false => self.current_provider_order(),
            true => self.base.providers.provider_order.clone(),
        };
        let mut order: Vec<String> = prior
            .into_iter()
            .filter(|p| providers.contains(p))
            .collect();
        if order.is_empty() {
            let head = match providers.contains(&chosen) {
                true => chosen.clone(),
                false => providers[0].clone(),
            };
            order.push(head);
        }

        let timeout = self.current_request_timeout();
        let region = self.current_bedrock_region();
        // Read off the form before it is replaced, like the two above.
        let zero = self.current_zero_retention();
        let agreements = self.current_agreements();
        let uploads = self.current_file_uploads();
        self.defaults = vec![
            Field {
                label: "Provider priority",
                help: "The providers a bare model name may run on, best first. Its head is your \
                       default provider. Enter opens a modal to drag the order and to add or \
                       drop configured providers; one left out is never chosen for a bare \
                       model name, and still runs any stage that names it as provider/model.",
                value: FieldValue::Order(order),
            },
            Field {
                label: "Request timeout (seconds)",
                help: "How long to wait on one inference. Unset uses the provider default.",
                value: FieldValue::Number(timeout),
            },
            Field {
                label: "Show advanced tuning",
                help: "Adds a screen of concurrency, retry and context limits, and the two \
                       model settings that override or back up what a blueprint names. \
                       Every one of them already has a default that works.",
                value: FieldValue::Bool(self.show_advanced),
            },
        ];
        // Bedrock's hosts are regional, so a chosen Bedrock gets a region
        // field; nobody else has one, and the retention rows below follow it.
        if self.bedrock_selected() {
            let current =
                region.unwrap_or_else(|| leviath_providers::bedrock::DEFAULT_REGION.to_string());
            let mut options: Vec<String> = Self::BEDROCK_REGIONS
                .iter()
                .map(|r| r.to_string())
                .collect();
            if !options.contains(&current) {
                options.insert(0, current.clone());
            }
            let index = options.iter().position(|r| *r == current).unwrap_or(0);
            self.defaults.push(Field {
                label: "AWS Bedrock region",
                help: "The region whose Bedrock endpoints are called. Starts from AWS_REGION \
                       when that is exported; written to the config only when it differs.",
                value: FieldValue::Choice { options, index },
            });
        }
        // The zero data retention switch and the agreement rows, read off
        // the previous form above (their rows move when Bedrock is picked
        // or dropped, so they are found by label; see `retention`).
        self.push_retention_fields(zero, &agreements, uploads);
        // Re-pick the concurrency default now that the provider choice is
        // settled. Doing this only on an arrow press missed the commonest
        // Ollama case entirely: when it is the *only* provider selected it is
        // already at index 0, nobody ever presses an arrow, and the limit
        // stayed at the hosted-API default of 8.
        self.apply_provider_concurrency_default();
    }

    /// Where the provider choice sits on the Defaults screen.
    pub const PROVIDER_FIELD: usize = 0;

    /// Where the request timeout sits on the Defaults screen.
    pub const TIMEOUT_FIELD: usize = 1;

    /// Where the advanced-tuning toggle sits on the Defaults screen.
    pub const ADVANCED_FIELD: usize = 2;

    /// Where the Bedrock region sits on the Defaults screen, when Bedrock is
    /// selected; the form has no fourth row otherwise.
    pub const REGION_FIELD: usize = 3;

    /// The regions the region field cycles: the commercial regions Bedrock
    /// serves models in. One the environment or the config names that is not
    /// here is offered first rather than refused.
    pub const BEDROCK_REGIONS: &'static [&'static str] = &[
        "us-east-1",
        "us-east-2",
        "us-west-2",
        "ca-central-1",
        "sa-east-1",
        "eu-west-1",
        "eu-west-2",
        "eu-west-3",
        "eu-central-1",
        "eu-north-1",
        "ap-northeast-1",
        "ap-northeast-2",
        "ap-south-1",
        "ap-southeast-1",
        "ap-southeast-2",
    ];

    /// Where the override model sits on the advanced screen: after every
    /// tuning limit, so the limits keep the indices `apply_limits_fields`
    /// matches on.
    pub const OVERRIDE_FIELD: usize = LIMITS_FIXED;

    /// Where the fallback model sits on the advanced screen.
    pub const FALLBACK_FIELD: usize = LIMITS_FIXED + 1;

    /// The model field's "no default" option.
    ///
    /// Worded as the blueprint's decision, never as a "provider default":
    /// no provider default model is consulted anywhere at run time. A stage
    /// that names no model of its own falls back to a model built into
    /// Leviath, not to anything the provider chose.
    pub const NO_DEFAULT_MODEL: &'static str = "(each blueprint decides)";

    fn current_request_timeout(&self) -> Option<u64> {
        match self.defaults.get(Self::TIMEOUT_FIELD).map(|f| &f.value) {
            Some(FieldValue::Number(n)) => *n,
            _ => self.base.request_timeout_secs,
        }
    }

    /// Whether the Bedrock row is selected.
    fn bedrock_selected(&self) -> bool {
        self.providers
            .iter()
            .any(|row| row.selected && row.provider.id == leviath_providers::bedrock::PROVIDER_NAME)
    }

    /// The Bedrock region in force: the field's pick while the form holds
    /// one, else what the config file says, else what the environment says.
    /// `None` is the provider's own default.
    pub(crate) fn current_bedrock_region(&self) -> Option<String> {
        match self.defaults.get(Self::REGION_FIELD).map(|f| &f.value) {
            Some(FieldValue::Choice { options, index }) => options.get(*index).cloned(),
            _ => self
                .base
                .providers
                .bedrock_region
                .clone()
                .or_else(|| self.region_from_env.clone()),
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

    /// How many of the pending changes the quit dialog lists before it says
    /// "and N more": the dialog is half the window tall, and a list that ran
    /// off its bottom would hide the buttons.
    pub(in crate::commands::setup) const QUIT_CHANGES_SHOWN: usize = 8;

    /// `q`/Ctrl-C with unsaved choices: ask before discarding them, and say
    /// what they are, so what is about to be lost is on screen rather than
    /// a guess.
    pub(crate) fn open_quit_confirm(&mut self) {
        use ratatui::style::Style;
        use ratatui::text::{Line, Span};
        let changes = super::plan::changes(&self.base, &self.build_plan());
        if changes.is_empty() {
            self.confirm = Some(PendingConfirm {
                purpose: ConfirmPurpose::QuitDiscard,
                dialog: crate::tui::widgets::confirm::Confirm::new(
                    "Quit setup?",
                    vec![Line::from(
                        "Nothing has been written yet, and nothing would change.",
                    )],
                    "Quit",
                    "Stay",
                ),
            });
            return;
        }
        let mut body = vec![Line::from(
            "Nothing has been written yet. Quitting discards these choices:",
        )];
        for change in changes.iter().take(Self::QUIT_CHANGES_SHOWN) {
            body.push(Line::from(Span::styled(
                format!("  {change}"),
                Style::default().fg(crate::tui::theme::C_MUTED),
            )));
        }
        if changes.len() > Self::QUIT_CHANGES_SHOWN {
            body.push(Line::from(Span::styled(
                format!("  and {} more", changes.len() - Self::QUIT_CHANGES_SHOWN),
                Style::default().fg(crate::tui::theme::C_DIM),
            )));
        }
        self.confirm = Some(PendingConfirm {
            purpose: ConfirmPurpose::QuitDiscard,
            dialog: crate::tui::widgets::confirm::Confirm::new("Quit setup?", body, "Quit", "Stay"),
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
                        // Booleans, choices and ordered lists are never
                        // text-edited: they change by toggle, picker, or the
                        // reorder modal.
                        FieldValue::Bool(_) | FieldValue::Choice { .. } | FieldValue::Order(_) => {}
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
                // A key left blank is a credential the user did not give -
                // usually because it is in their environment on purpose, and
                // `Config::load` reads it back from there. This is the only
                // kind that rule holds for: written as "any empty value" it
                // would catch the kinds that never have one, and a finished
                // browser sign-in, which types nothing, would be switched back
                // off the moment the plan was applied.
                Credential::ApiKey if row.value.is_empty() => {
                    catalog::set_credential(&mut config, row.provider.id, None)
                }
                // Everything else: choosing the row is the setting, and
                // whatever was typed rides along. What that means is the row's
                // own business - Ollama drops an address equal to its default
                // so `$OLLAMA_HOST` still applies, and a browser sign-in has
                // nothing to store at all - and none of it is decided here.
                _ => catalog::set_credential(&mut config, row.provider.id, Some(row.value.clone())),
            }
        }

        // The Claude Code transport has no row here. Its keys
        // (`claude_code_enabled`, `claude_code_effort`, `claude_code_binary`)
        // are set by hand or by `lev setup --claude-code`, and the clone of
        // `base` above carries whatever the file already says through
        // untouched.

        self.write_endpoints(&mut config);

        // The head of the priority is the default provider, and the whole
        // order is `provider_order`. Written only when the user arranged more
        // than one: a single-provider order says nothing `default_provider`
        // does not, and leaving it empty keeps a config that never wanted an
        // order from growing a one-entry one.
        let order = self.current_provider_order();
        config.default_provider = order
            .first()
            .cloned()
            .unwrap_or_else(|| self.current_default_provider());
        config.providers.provider_order = if order.len() > 1 { order } else { Vec::new() };
        // The two model settings live on the advanced screen. Until it has
        // been visited the override keeps what the config holds, or what an
        // endpoint entry at the head of the priority picked for itself; a
        // visit that chose "(each blueprint decides)" is a real answer and is
        // not overridden by that pick.
        let head = config.default_provider.clone();
        config.override_model = self.chosen_model(Self::OVERRIDE_FIELD).unwrap_or_else(|| {
            self.base
                .override_model
                .clone()
                .or_else(|| self.endpoint_default_model(&head))
        });
        config.fallback_model = self
            .chosen_model(Self::FALLBACK_FIELD)
            .unwrap_or_else(|| self.base.fallback_model.clone());
        config.request_timeout_secs = self.current_request_timeout();
        // The region is written only when Bedrock is chosen and the pick
        // says something the environment and the provider's default do not:
        // `Config::load` folds `AWS_REGION` into `base`, and writing that
        // back would pin a machine's environment into its config file, the
        // same mistake the credential rows avoid.
        config.providers.bedrock_region = match self.bedrock_selected() {
            true => self
                .current_bedrock_region()
                .filter(|r| r != leviath_providers::bedrock::DEFAULT_REGION)
                .filter(|r| Some(r) != self.region_from_env.as_ref()),
            false => None,
        };
        config.providers.zero_retention = self.current_zero_retention();
        config.providers.zero_retention_agreements = self.current_agreements();
        config.providers.file_uploads = self.current_file_uploads();

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

    // ─── the Bedrock region ───────────────────────────────────────────────

    fn bedrock_row(wizard: &Wizard) -> usize {
        wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "bedrock")
            .expect("the catalog offers Bedrock")
    }

    #[test]
    fn the_region_field_appears_only_when_bedrock_is_chosen() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].selected = true;
        wizard.enter(Step::Defaults);
        // Three fixed rows, the retention switch, the first provider's
        // agreement row, and the upload switch.
        assert_eq!(wizard.defaults.len(), 6);
        assert_eq!(wizard.current_bedrock_region(), None);

        let bedrock = bedrock_row(&wizard);
        wizard.providers[bedrock].selected = true;
        wizard.enter(Step::Defaults);
        assert_eq!(wizard.defaults.len(), 7);
        let field = &wizard.defaults[Wizard::REGION_FIELD];
        assert_eq!(field.label, "AWS Bedrock region");
        assert_eq!(field.value.display(), "us-east-1");
        assert_eq!(
            wizard.current_bedrock_region().as_deref(),
            Some("us-east-1")
        );
        // The default region is not written; a pick is; deselecting clears.
        assert_eq!(wizard.build_config().providers.bedrock_region, None);
        let eu = Wizard::BEDROCK_REGIONS
            .iter()
            .position(|r| *r == "eu-west-1")
            .unwrap();
        wizard.defaults[Wizard::REGION_FIELD].value.set_index(eu);
        assert_eq!(
            wizard.build_config().providers.bedrock_region.as_deref(),
            Some("eu-west-1")
        );
        // Rebuilding the form keeps the pick.
        wizard.rebuild_defaults();
        assert_eq!(
            wizard.defaults[Wizard::REGION_FIELD].value.display(),
            "eu-west-1"
        );
        wizard.providers[bedrock].selected = false;
        wizard.rebuild_defaults();
        assert_eq!(wizard.defaults.len(), 6);
        assert_eq!(wizard.build_config().providers.bedrock_region, None);
    }

    /// The retention switch follows the region row, keeps its value across a
    /// rebuild that moves it, and writes `zero_retention`; an agreement row
    /// appears for each chosen contract provider and writes the list, while
    /// a name the wizard does not offer stays as the config wrote it.
    #[test]
    fn the_retention_rows_follow_the_region_and_write_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.base.providers.zero_retention_agreements =
            vec!["groq".to_string(), "openai".to_string()];
        let anthropic = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "anthropic")
            .expect("the catalog offers Anthropic");
        wizard.providers[anthropic].selected = true;
        wizard.enter(Step::Defaults);

        // No region row, so the switch is fourth.
        let zero = Wizard::REGION_FIELD;
        assert_eq!(wizard.defaults[zero].label, Wizard::ZERO_RETENTION_LABEL);
        assert_eq!(wizard.defaults[zero].value.display(), "no");
        assert!(
            wizard.defaults[zero]
                .help
                .contains("zero data retention, ZDR"),
            "the help spells the term out"
        );
        assert_eq!(
            wizard.defaults[zero + 1].label,
            "ZDR agreement with Anthropic"
        );
        assert_eq!(
            wizard.defaults[zero + 2].label,
            Wizard::FILE_UPLOADS_LABEL,
            "OpenAI is not chosen, so no row before the upload switch"
        );
        assert_eq!(wizard.defaults.len(), zero + 3);
        assert!(wizard.current_file_uploads(), "uploads are on by default");
        assert!(
            wizard.defaults[zero + 2]
                .help
                .contains("Zero data retention turns uploads off")
        );
        assert!(!wizard.current_zero_retention());
        assert_eq!(
            wizard.current_agreements(),
            vec!["groq".to_string(), "openai".to_string()],
            "the config's names stand until a row says otherwise"
        );

        wizard.defaults[zero].value = FieldValue::Bool(true);
        wizard.defaults[zero + 1].value = FieldValue::Bool(true);
        wizard.defaults[zero + 2].value = FieldValue::Bool(false);
        let config = wizard.build_config();
        assert!(!config.providers.file_uploads);
        assert!(config.providers.zero_retention);
        assert_eq!(
            config.providers.zero_retention_agreements,
            vec![
                "groq".to_string(),
                "anthropic".to_string(),
                "openai".to_string()
            ]
        );

        // Bedrock moves the switch down one; the values ride along.
        let bedrock = bedrock_row(&wizard);
        wizard.providers[bedrock].selected = true;
        wizard.rebuild_defaults();
        assert_eq!(wizard.defaults[4].label, Wizard::ZERO_RETENTION_LABEL);
        assert_eq!(wizard.defaults[4].value.display(), "yes");
        assert_eq!(wizard.defaults[5].value.display(), "yes");
        assert!(wizard.build_config().providers.zero_retention);

        // An OpenAI row appears once OpenAI is chosen, holding the config's
        // answer; turning it off drops the name.
        let openai = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "openai")
            .expect("the catalog offers OpenAI");
        wizard.providers[openai].selected = true;
        wizard.rebuild_defaults();
        assert_eq!(wizard.defaults[6].label, "ZDR agreement with OpenAI");
        assert_eq!(wizard.defaults[6].value.display(), "yes");
        wizard.defaults[6].value = FieldValue::Bool(false);
        assert_eq!(
            wizard.build_config().providers.zero_retention_agreements,
            vec!["groq".to_string(), "anthropic".to_string()]
        );
    }

    #[test]
    fn the_region_starts_from_the_file_then_the_environment_and_never_writes_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let lookup = |name: &str| match name {
            "AWS_REGION" => Some(" ap-south-1 ".to_string()),
            "AWS_BEARER_TOKEN_BEDROCK" => Some("ABSK-env".to_string()),
            _ => None,
        };
        let mut wizard = Wizard::new(
            Config::default(),
            &lookup,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        assert_eq!(wizard.region_from_env.as_deref(), Some("ap-south-1"));
        let bedrock = bedrock_row(&wizard);
        assert!(
            wizard.providers[bedrock].selected,
            "a key in the environment selects the row"
        );
        wizard.enter(Step::Defaults);
        assert_eq!(
            wizard.defaults[Wizard::REGION_FIELD].value.display(),
            "ap-south-1"
        );
        // The environment's region is left to the environment.
        assert_eq!(wizard.build_config().providers.bedrock_region, None);

        // A region the list does not carry is offered first, not refused.
        let mut base = Config::default();
        base.providers.bedrock_api_key = Some("ABSK".to_string());
        base.providers.bedrock_region = Some("mx-central-1".to_string());
        let mut wizard = Wizard::new(
            base,
            &lookup,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        assert_eq!(
            wizard.current_bedrock_region().as_deref(),
            Some("mx-central-1")
        );
        wizard.enter(Step::Defaults);
        let field = &wizard.defaults[Wizard::REGION_FIELD];
        assert_eq!(field.value.display(), "mx-central-1");
        assert_eq!(
            field.value.options().first().map(String::as_str),
            Some("mx-central-1")
        );
        assert_eq!(
            wizard.build_config().providers.bedrock_region.as_deref(),
            Some("mx-central-1")
        );
        // The default-region variable is the second choice.
        let fallback = |name: &str| match name {
            "AWS_DEFAULT_REGION" => Some("eu-north-1".to_string()),
            _ => None,
        };
        let wizard = Wizard::new(
            Config::default(),
            &fallback,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        assert_eq!(wizard.region_from_env.as_deref(), Some("eu-north-1"));
    }

    #[test]
    fn a_bedrock_check_carries_the_region_a_run_would_use() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (mut requests, _replies) = wizard.take_verify_ends().expect("first take");
        let bedrock = bedrock_row(&wizard);
        wizard.providers[bedrock].selected = true;
        wizard.providers[bedrock].value = "ABSK-typed".to_string();
        wizard.base.providers.bedrock_region = Some("eu-west-2".to_string());
        wizard.request_verification(bedrock);
        let request = requests.try_recv().expect("a check was requested");
        assert_eq!(request.provider_id, "bedrock");
        assert_eq!(request.creds.api_key.as_deref(), Some("ABSK-typed"));
        assert_eq!(
            request.creds.options.get("region").map(String::as_str),
            Some("eu-west-2")
        );
        // No region anywhere: none is carried, and the provider's default
        // applies to the check as it would to a run.
        wizard.base.providers.bedrock_region = None;
        wizard.request_verification(bedrock);
        let request = requests.try_recv().expect("a second check");
        assert!(!request.creds.options.contains_key("region"));
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

    /// The Providers screen's rows are the configured providers, then the
    /// add row, then the button; the cursor never leaves them.
    #[test]
    fn the_cursor_stays_inside_the_current_step() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].selected = true;
        wizard.enter(Step::Providers);
        assert_eq!(wizard.visible_providers(), vec![0]);
        assert_eq!(wizard.cursor_provider(), Some(0));
        assert!(!wizard.on_add_provider());

        wizard.move_cursor(-5);
        assert_eq!(wizard.cursor, 0);
        wizard.move_cursor(100);
        assert_eq!(
            wizard.cursor, 2,
            "clamped to the Continue button after the provider and the add row"
        );
        assert!(wizard.on_continue());
        assert_eq!(wizard.cursor_provider(), None);
        wizard.move_cursor(-1);
        assert!(wizard.on_add_provider());
        assert_eq!(wizard.cursor_provider(), None);
        assert!(!wizard.on_continue());

        // Only the add row and the button with nothing configured.
        wizard.providers[0].selected = false;
        wizard.enter(Step::Providers);
        assert_eq!(wizard.row_count(), 1);
        assert!(wizard.on_add_provider());
        wizard.move_cursor(1);
        assert!(wizard.on_continue());
        // The add row is the Providers screen's alone.
        wizard.enter(Step::Agents);
        assert!(!wizard.on_add_provider());
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
        assert_eq!(wizard.continue_label(), "Continue (add a provider first)");
        wizard.providers[0].selected = true;
        wizard.providers[1].selected = true;
        assert_eq!(wizard.continue_label(), "Continue: Defaults (2 configured)");

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
        // clean machine, or through the tuning screen they did not ask for.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        assert!(wizard.mcp.is_empty());

        wizard.enter(Step::Providers);
        wizard.next_step();
        assert_eq!(
            wizard.step,
            Step::Defaults,
            "the tuning screen is off by default"
        );

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

    // ─── the setup modal ────────────────────────────────────────────────────

    /// The modal takes over the cursor while it is open: its card's rows,
    /// then its three buttons, and the step's Continue button is never on
    /// offer underneath it.
    #[test]
    fn opening_the_modal_hands_the_cursor_to_its_card_and_buttons() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Providers);
        assert_eq!(wizard.modal_index(), None);
        assert_eq!(wizard.detail_row(), None);
        assert!(wizard.detail_actions().is_empty());
        assert_eq!(wizard.modal_card_rows(), 0);
        assert_eq!(wizard.modal_button_at(0), None);

        wizard.cursor = 1;
        wizard.scroll = 3;
        wizard.message = Some("stale".to_string());
        wizard.open_provider_modal(0);
        assert_eq!(wizard.modal_index(), Some(0));
        assert_eq!(wizard.detail_row(), Some(0));
        assert_eq!(wizard.cursor, 0, "the card starts on its first row");
        assert_eq!(wizard.scroll, 0);
        assert_eq!(wizard.message, None);
        // The key row and the key-page button, then the three buttons.
        assert_eq!(wizard.modal_card_rows(), 2);
        assert_eq!(wizard.row_count(), 2);
        assert_eq!(wizard.nav_rows(), 5);
        assert_eq!(wizard.modal_button_at(1), None);
        assert_eq!(wizard.modal_button_at(2), Some(ModalButton::VerifyUse));
        assert_eq!(wizard.modal_button_at(3), Some(ModalButton::SkipUse));
        assert_eq!(wizard.modal_button_at(4), Some(ModalButton::Cancel));
        assert_eq!(wizard.modal_button_at(5), None);
        wizard.move_cursor(100);
        assert_eq!(wizard.cursor, 4, "clamped to the last button");
        assert!(!wizard.on_continue(), "the modal's buttons are its own");
        assert_eq!(
            ModalButton::ALL.map(ModalButton::label),
            ["Verify and use", "Skip verification and use", "Cancel"]
        );

        // A row that does not exist opens nothing.
        let mut wizard = test_wizard(dir.path());
        wizard.open_provider_modal(999);
        assert_eq!(wizard.modal_index(), None);
    }

    /// "Skip verification and use" keeps the provider as it stands, once it
    /// has enough to keep: a key where one is typed, an entry where entries
    /// are, and nothing more for the kinds that carry nothing.
    #[test]
    fn accepting_the_modal_keeps_the_provider_and_refuses_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Providers);
        wizard.accept_modal();
        assert!(!wizard.dirty, "nothing open, nothing to accept");

        wizard.open_provider_modal(0);
        wizard.accept_modal();
        assert_eq!(
            wizard.message.as_deref(),
            Some("Enter an API key first, or cancel.")
        );
        assert_eq!(wizard.modal_index(), Some(0), "still open");
        assert!(!wizard.providers[0].selected);
        assert!(!wizard.dirty);

        wizard.providers[0].value = "sk-ant-x".to_string();
        wizard.accept_modal();
        assert_eq!(wizard.modal_index(), None);
        assert!(wizard.providers[0].selected);
        assert!(wizard.dirty);
        assert_eq!(wizard.message.as_deref(), Some("Anthropic is set up."));
        assert_eq!(wizard.step, Step::Providers);
        assert_eq!(
            wizard.cursor, 0,
            "the cursor lands on the provider just set up"
        );

        // A key from the environment is a key.
        let openai = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "openai")
            .expect("the catalog offers OpenAI");
        wizard.providers[openai].from_env = Some("OPENAI_API_KEY");
        wizard.open_provider_modal(openai);
        wizard.accept_modal();
        assert!(wizard.providers[openai].selected);
        assert_eq!(wizard.cursor, 1, "second in the list now");

        // Choosing Ollama is the whole configuration.
        let ollama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");
        wizard.open_provider_modal(ollama);
        wizard.accept_modal();
        assert!(wizard.providers[ollama].selected);
        assert_eq!(wizard.message.as_deref(), Some("Ollama is set up."));

        // An endpoint preset needs an entry under it.
        let llama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "llama-cpp")
            .expect("the catalog offers llama.cpp");
        wizard.open_provider_modal(llama);
        assert_eq!(
            wizard.endpoints_under("llama-cpp").len(),
            1,
            "opened with a fresh entry"
        );
        wizard.remove_endpoint(0);
        wizard.accept_modal();
        assert_eq!(
            wizard.message.as_deref(),
            Some("Add an endpoint first, or cancel.")
        );
        assert_eq!(wizard.modal_index(), Some(llama));
        wizard.add_endpoint(llama);
        wizard.accept_modal();
        assert_eq!(wizard.modal_index(), None);
        assert!(wizard.providers[llama].selected);
        assert_eq!(wizard.message.as_deref(), Some("llama.cpp is set up."));
    }

    /// Cancel puts the row, its entries and the Providers screen's cursor
    /// back exactly as the modal found them.
    #[test]
    fn cancelling_the_modal_restores_the_row_its_entries_and_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.cancel_modal();
        assert_eq!(wizard.message, None, "nothing open, nothing to cancel");

        wizard.providers[0].selected = true;
        wizard.providers[0].value = "sk-ant-old".to_string();
        wizard.enter(Step::Providers);
        wizard.cursor = 1;
        wizard.scroll = 2;
        wizard.open_provider_modal(0);
        wizard.providers[0].value = "sk-ant-new".to_string();
        wizard.providers[0].outcome = Outcome::Reachable {
            models: vec!["m".to_string()],
        };
        wizard.edit = Some(Edit {
            target: EditTarget::Credential(0),
            line: crate::tui::widgets::line_edit::LineEdit::new("typing".to_string(), true),
        });
        wizard.cancel_modal();
        assert_eq!(wizard.modal_index(), None);
        assert_eq!(wizard.providers[0].value, "sk-ant-old");
        assert_eq!(wizard.providers[0].outcome, Outcome::Skipped);
        assert!(wizard.providers[0].selected, "still configured, as it was");
        assert!(wizard.edit.is_none());
        assert_eq!(wizard.cursor, 1);
        assert_eq!(wizard.scroll, 2);
        assert_eq!(
            wizard.message.as_deref(),
            Some("Cancelled; nothing changed.")
        );
        assert!(!wizard.dirty);

        // An endpoint preset opened with no entry gets one to edit, and
        // Cancel takes it away again.
        let llama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "llama-cpp")
            .expect("the catalog offers llama.cpp");
        wizard.open_provider_modal(llama);
        assert_eq!(wizard.endpoints_under("llama-cpp").len(), 1);
        assert!(wizard.providers[llama].selected);
        wizard.cancel_modal();
        assert!(wizard.endpoints.is_empty());
        assert!(!wizard.providers[llama].selected);

        // One that had an entry keeps it, as it was, and loses the one
        // added meanwhile.
        wizard.add_endpoint(llama);
        wizard.open_provider_modal(llama);
        assert_eq!(
            wizard.endpoints.len(),
            1,
            "no second entry for one that has one"
        );
        wizard.endpoints[0].base_url = "http://changed:1/v1".to_string();
        wizard.add_endpoint(llama);
        wizard.cancel_modal();
        assert_eq!(wizard.endpoints.len(), 1);
        assert_eq!(wizard.endpoints[0].base_url, catalog::LLAMA_CPP_URL);
        assert!(wizard.providers[llama].selected);
    }

    /// The two buttons that need no verifier: one keeps, one cancels.
    #[test]
    fn the_skip_and_cancel_buttons_accept_and_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.activate_modal_button(ModalButton::SkipUse);
        assert!(!wizard.dirty, "nothing open, nothing pressed");

        wizard.providers[0].value = "sk-ant-x".to_string();
        wizard.enter(Step::Providers);
        wizard.open_provider_modal(0);
        wizard.activate_modal_button(ModalButton::SkipUse);
        assert_eq!(wizard.modal_index(), None);
        assert!(wizard.providers[0].selected);

        wizard.open_provider_modal(1);
        wizard.providers[1].value = "sk-typed-then-abandoned".to_string();
        wizard.activate_modal_button(ModalButton::Cancel);
        assert_eq!(wizard.modal_index(), None);
        assert!(!wizard.providers[1].selected);
        assert!(wizard.providers[1].value.is_empty());
        assert_eq!(
            wizard.message.as_deref(),
            Some("Cancelled; nothing changed.")
        );
    }

    /// `d` on the Providers screen: the provider is deselected, its
    /// credential and check forgotten, a preset's entries taken with it; one
    /// the environment supplies is refused, since the variable would put it
    /// straight back.
    #[test]
    fn removing_a_provider_clears_it_and_refuses_one_from_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].selected = true;
        wizard.providers[0].value = "sk-ant-x".to_string();
        wizard.providers[0].outcome = Outcome::Reachable {
            models: vec!["m".to_string()],
        };
        wizard.providers[1].selected = true;
        wizard.providers[1].value = "sk-oai".to_string();
        wizard.enter(Step::Providers);
        wizard.cursor = 2; // the add row, after two providers

        wizard.remove_provider(1);
        assert!(!wizard.providers[1].selected);
        assert!(wizard.providers[1].value.is_empty());
        assert_eq!(wizard.providers[1].outcome, Outcome::Skipped);
        assert!(wizard.dirty);
        assert_eq!(
            wizard.message.as_deref(),
            Some("OpenAI removed; its credential is cleared when you finish.")
        );
        assert_eq!(wizard.cursor, 1, "one row fewer: the add row moved up");
        assert!(wizard.on_add_provider());
        assert!(!wizard.on_continue());
        assert!(!catalog::is_configured(&wizard.build_config(), "openai"));

        // The environment's key stays: the row is refused with a message.
        wizard.providers[0].from_env = Some("ANTHROPIC_API_KEY");
        wizard.dirty = false;
        wizard.remove_provider(0);
        assert!(wizard.providers[0].selected);
        assert_eq!(wizard.providers[0].value, "sk-ant-x");
        assert!(!wizard.dirty);
        assert_eq!(
            wizard.message.as_deref(),
            Some(
                "Anthropic is supplied by $ANTHROPIC_API_KEY; unset the variable to stop using it."
            )
        );

        // A preset goes with its entries.
        let llama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "llama-cpp")
            .expect("the catalog offers llama.cpp");
        wizard.add_endpoint(llama);
        wizard.add_endpoint(llama);
        wizard.remove_provider(llama);
        assert!(wizard.endpoints.is_empty());
        assert!(!wizard.providers[llama].selected);

        // An index no row has is a no-op.
        wizard.message = None;
        wizard.remove_provider(999);
        assert_eq!(wizard.message, None);
    }

    /// "Verify and use" asks the verifier and waits: a pass keeps the
    /// provider and closes the modal, a failure leaves it open with the
    /// answer on screen.
    #[tokio::test]
    async fn verify_and_use_accepts_on_a_pass_and_stays_open_on_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (mut requests, replies) = wizard.take_verify_ends().expect("first take");
        wizard.activate_modal_button(ModalButton::VerifyUse);
        assert!(requests.try_recv().is_err(), "nothing open, nothing asked");

        wizard.enter(Step::Providers);
        wizard.open_provider_modal(0);
        wizard.providers[0].value = "sk-ant-x".to_string();
        wizard.activate_modal_button(ModalButton::VerifyUse);
        assert!(wizard.providers[0].checking);
        assert!(wizard.modal.as_ref().is_some_and(|m| m.awaiting_verify));
        assert_eq!(wizard.message.as_deref(), Some("Checking…"));
        assert_eq!(requests.try_recv().expect("asked").provider_id, "anthropic");

        replies
            .send(VerifyReply {
                provider_id: "anthropic".to_string(),
                outcome: Outcome::Failed {
                    message: "rejected - check the key".to_string(),
                },
            })
            .unwrap();
        wizard.drain_verifications();
        assert_eq!(
            wizard.modal_index(),
            Some(0),
            "a failure keeps the modal open"
        );
        assert!(!wizard.modal.as_ref().is_some_and(|m| m.awaiting_verify));
        assert!(!wizard.providers[0].selected);
        assert_eq!(wizard.message.as_deref(), Some("rejected - check the key"));

        wizard.activate_modal_button(ModalButton::VerifyUse);
        replies
            .send(VerifyReply {
                provider_id: "anthropic".to_string(),
                outcome: Outcome::Reachable {
                    models: vec!["claude-opus-5".to_string()],
                },
            })
            .unwrap();
        wizard.drain_verifications();
        assert_eq!(wizard.modal_index(), None);
        assert!(wizard.providers[0].selected);
        assert!(wizard.dirty);
        assert_eq!(wizard.message.as_deref(), Some("Anthropic is set up."));
    }

    /// With nothing to check the button says so rather than waiting for an
    /// answer that will never come; a reply that lands while nobody is
    /// waiting is the row's business, not the modal's; and a wait with
    /// nothing in flight and no answer is told to use it unverified.
    #[tokio::test]
    async fn verify_and_use_with_nothing_to_check_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (mut requests, replies) = wizard.take_verify_ends().expect("first take");
        wizard.enter(Step::Providers);
        wizard.open_provider_modal(0);
        wizard.activate_modal_button(ModalButton::VerifyUse);
        assert!(requests.try_recv().is_err());
        assert!(!wizard.modal.as_ref().is_some_and(|m| m.awaiting_verify));
        assert_eq!(
            wizard.message.as_deref(),
            Some("Nothing to check yet: enter a credential, or use it unverified.")
        );

        replies
            .send(VerifyReply {
                provider_id: "anthropic".to_string(),
                outcome: Outcome::Reachable {
                    models: vec!["m".to_string()],
                },
            })
            .unwrap();
        wizard.drain_verifications();
        assert_eq!(
            wizard.modal_index(),
            Some(0),
            "not waiting, so not accepted"
        );
        assert!(!wizard.providers[0].selected);

        wizard.providers[0].outcome = Outcome::Skipped;
        wizard.modal.as_mut().expect("open").awaiting_verify = true;
        wizard.settle_modal_verification();
        assert!(!wizard.modal.as_ref().is_some_and(|m| m.awaiting_verify));
        assert_eq!(
            wizard.message.as_deref(),
            Some("Not checked: nothing to verify yet. Use it unverified, or cancel.")
        );

        // Still in flight: nothing is decided yet.
        wizard.providers[0].checking = true;
        wizard.modal.as_mut().expect("open").awaiting_verify = true;
        wizard.settle_modal_verification();
        assert!(wizard.modal.as_ref().is_some_and(|m| m.awaiting_verify));
        assert_eq!(wizard.modal_index(), Some(0));

        // Nothing open: nothing to settle.
        wizard.cancel_modal();
        wizard.settle_modal_verification();
        assert_eq!(wizard.modal_index(), None);
    }

    /// An endpoint preset's "Verify and use" checks every entry under it
    /// and waits for all of them: one failure names the entry, and the
    /// modal accepts only once every entry has answered yes.
    #[tokio::test]
    async fn verify_and_use_on_a_preset_waits_for_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (mut requests, replies) = wizard.take_verify_ends().expect("first take");
        let llama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "llama-cpp")
            .expect("the catalog offers llama.cpp");
        wizard.enter(Step::Providers);
        wizard.open_provider_modal(llama);
        wizard.add_endpoint(llama);
        let names: Vec<&str> = wizard.endpoints.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["llama-cpp", "llama-cpp-2"]);

        wizard.activate_modal_button(ModalButton::VerifyUse);
        assert!(wizard.endpoints.iter().all(|e| e.checking));
        assert!(wizard.modal.as_ref().is_some_and(|m| m.awaiting_verify));
        let mut asked = Vec::new();
        while let Ok(request) = requests.try_recv() {
            asked.push(request.provider_id);
        }
        assert_eq!(asked, ["llama-cpp", "llama-cpp-2"]);

        // One answer in: still waiting on the other.
        replies
            .send(VerifyReply {
                provider_id: "llama-cpp".to_string(),
                outcome: Outcome::Reachable {
                    models: vec!["a".to_string()],
                },
            })
            .unwrap();
        wizard.drain_verifications();
        assert!(wizard.modal.as_ref().is_some_and(|m| m.awaiting_verify));

        replies
            .send(VerifyReply {
                provider_id: "llama-cpp-2".to_string(),
                outcome: Outcome::Failed {
                    message: "unreachable".to_string(),
                },
            })
            .unwrap();
        wizard.drain_verifications();
        assert_eq!(wizard.modal_index(), Some(llama));
        assert!(!wizard.modal.as_ref().is_some_and(|m| m.awaiting_verify));
        assert_eq!(
            wizard.message.as_deref(),
            Some("Check failed for llama-cpp-2.")
        );

        // Both pass the second time, and the preset is kept.
        wizard.activate_modal_button(ModalButton::VerifyUse);
        for name in ["llama-cpp", "llama-cpp-2"] {
            replies
                .send(VerifyReply {
                    provider_id: name.to_string(),
                    outcome: Outcome::Reachable {
                        models: vec!["a".to_string()],
                    },
                })
                .unwrap();
        }
        wizard.drain_verifications();
        assert_eq!(wizard.modal_index(), None);
        assert!(wizard.providers[llama].selected);
        assert_eq!(wizard.message.as_deref(), Some("llama.cpp is set up."));

        // An entry with no address is refused on the spot, so a preset
        // whose only entry has none has nothing in flight.
        let custom = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "openai-compatible")
            .expect("the catalog offers a custom endpoint");
        wizard.open_provider_modal(custom);
        wizard.activate_modal_button(ModalButton::VerifyUse);
        assert!(!wizard.modal.as_ref().is_some_and(|m| m.awaiting_verify));
        assert_eq!(
            wizard.message.as_deref(),
            Some("Nothing to check yet: enter a credential, or use it unverified.")
        );
    }

    // ─── adding a provider ──────────────────────────────────────────────────

    /// The chooser walks the catalog a level at a time: how the provider is
    /// reached, what it makes, then which one. A choice at the last level
    /// opens the modal, and a dismissal steps back one level.
    #[test]
    fn the_add_flow_walks_category_kind_and_provider_and_steps_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Providers);
        assert_eq!(
            wizard.provider_categories(),
            ["API key", "Subscription logins", "Local and custom"]
        );

        wizard.open_add_provider();
        assert_eq!(wizard.picker_purpose, PickerPurpose::Category);
        let picker = wizard.picker.as_ref().expect("open");
        assert_eq!(picker.title, "Add a provider");
        let values: Vec<&str> = picker
            .matches()
            .into_iter()
            .map(|i| picker.options[i].value.as_str())
            .collect();
        assert_eq!(
            values,
            ["API key", "Subscription logins", "Local and custom"]
        );
        assert_eq!(
            picker.options.len(),
            3 + wizard.providers.len(),
            "every provider can be searched for"
        );
        assert!(picker.options[0].detail.contains("paste a key"));
        assert!(picker.options[1].detail.contains("browser"));
        assert!(picker.options[2].detail.contains("server you run"));

        // A category: what its providers make, each naming them.
        wizard.settle_picker_choice(0);
        assert_eq!(
            wizard.picker_purpose,
            PickerPurpose::Kind {
                category: "API key"
            }
        );
        let picker = wizard.picker.as_ref().expect("open");
        assert_eq!(picker.title, "Add a provider: API key");
        let values: Vec<&str> = picker.options.iter().map(|o| o.value.as_str()).collect();
        assert_eq!(
            values,
            [
                "Text and images",
                "Video",
                "Speech and audio",
                "3D models and textures"
            ]
        );
        let detail = picker.options[0].detail.as_str();
        assert!(detail.starts_with("Anthropic, OpenAI"), "{detail}");
        assert_eq!(picker.options[1].detail, "OpenAI, Google, xAI");
        assert_eq!(picker.options[2].detail, "OpenAI, Google, xAI, Meta");
        assert_eq!(picker.options[3].detail, "Meshy");

        // A kind: the providers themselves, one row of `providers` each.
        wizard.settle_picker_choice(3);
        let meshy = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "meshy")
            .expect("the catalog offers Meshy");
        assert_eq!(
            wizard.picker_purpose,
            PickerPurpose::Provider {
                category: "API key",
                kind: "3D models and textures",
                rows: vec![meshy],
            }
        );
        let picker = wizard.picker.as_ref().expect("open");
        assert_eq!(picker.title, "Add a provider: 3D models and textures");
        let values: Vec<&str> = picker.options.iter().map(|o| o.value.as_str()).collect();
        assert_eq!(values, ["Meshy"]);
        assert_eq!(
            picker.options[0].detail,
            wizard.providers[meshy].provider.blurb
        );

        // Back a level, and back again; the top level stays put, open.
        wizard.picker_back();
        assert_eq!(
            wizard.picker_purpose,
            PickerPurpose::Kind {
                category: "API key"
            }
        );
        wizard.picker_back();
        assert_eq!(wizard.picker_purpose, PickerPurpose::Category);
        wizard.picker_back();
        assert_eq!(wizard.picker_purpose, PickerPurpose::Category);
        assert!(wizard.picker.is_some());

        // Down to the provider: choosing it opens its modal and closes the
        // chooser.
        wizard.settle_picker_choice(0);
        wizard.settle_picker_choice(3);
        wizard.settle_picker_choice(0);
        assert_eq!(wizard.modal_index(), Some(meshy));
        assert!(wizard.picker.is_none());

        // One already set up says so in the chooser.
        wizard.providers[meshy].value = "msy_x".to_string();
        wizard.accept_modal();
        wizard.open_add_provider();
        wizard.settle_picker_choice(0);
        wizard.settle_picker_choice(3);
        assert!(
            wizard.picker.as_ref().expect("open").options[0]
                .detail
                .ends_with("(already set up)")
        );

        // A choice past the options at any level changes nothing.
        wizard.picker_back();
        wizard.picker_back();
        wizard.settle_picker_choice(99);
        assert_eq!(wizard.picker_purpose, PickerPurpose::Category);
        wizard.settle_picker_choice(0);
        wizard.settle_picker_choice(99);
        assert_eq!(
            wizard.picker_purpose,
            PickerPurpose::Kind {
                category: "API key"
            }
        );
        wizard.settle_picker_choice(0);
        wizard.settle_picker_choice(99);
        let text_rows: Vec<usize> = wizard
            .providers
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.provider.auth_kind() == "API key"
                    && r.provider.kinds().contains(&"Text and images")
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            wizard.picker_purpose,
            PickerPurpose::Provider {
                category: "API key",
                kind: "Text and images",
                rows: text_rows,
            }
        );
        assert_eq!(wizard.modal_index(), None);

        // A field's chooser is untouched by the back step.
        wizard.picker_purpose = PickerPurpose::Field(0);
        wizard.picker_back();
        assert_eq!(wizard.picker_purpose, PickerPurpose::Field(0));
    }

    // ─── the provider priority ──────────────────────────────────────────────

    /// Configuring a provider does not put it in the priority: the order
    /// keeps what it had, filtered to what is still configured, and is only
    /// ever seeded with one entry when it would otherwise be empty.
    #[test]
    fn rebuilding_the_defaults_never_adds_a_newly_configured_provider() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].selected = true;
        wizard.enter(Step::Defaults);
        assert_eq!(wizard.current_provider_order(), ["anthropic".to_string()]);

        wizard.providers[1].selected = true;
        wizard.rebuild_defaults();
        assert_eq!(
            wizard.current_provider_order(),
            ["anthropic".to_string()],
            "OpenAI is configured, not chosen"
        );
        assert_eq!(
            wizard.configured_provider_names(),
            ["anthropic".to_string(), "openai".to_string()]
        );

        // Dropping the head leaves the order empty, so it is re-seeded from
        // what is left.
        wizard.providers[0].selected = false;
        wizard.rebuild_defaults();
        assert_eq!(wizard.current_provider_order(), ["openai".to_string()]);
        let config = wizard.build_config();
        assert_eq!(config.default_provider, "openai");
        assert!(
            config.providers.provider_order.is_empty(),
            "a one-entry order is not written"
        );

        // The config's own order is the starting point, filtered to what is
        // configured.
        let mut base = Config::default();
        base.providers.provider_order = vec![
            "openai".to_string(),
            "ollama".to_string(),
            "anthropic".to_string(),
        ];
        catalog::set_credential(&mut base, "openai", Some("sk-oai".to_string()));
        catalog::set_credential(&mut base, "anthropic", Some("sk-ant".to_string()));
        let mut wizard = Wizard::new(
            base,
            &|_| None,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        wizard.enter(Step::Defaults);
        assert_eq!(
            wizard.current_provider_order(),
            ["openai".to_string(), "anthropic".to_string()]
        );
        assert_eq!(
            wizard.build_config().providers.provider_order,
            vec!["openai".to_string(), "anthropic".to_string()]
        );
    }

    /// The reorder modal is seeded with the order, then every other
    /// configured provider left out so Space can bring one in; Enter keeps
    /// the included rows only, and what it keeps is written back.
    #[test]
    fn open_reorder_seeds_the_order_and_the_left_out_providers() {
        use crate::tui::widgets::reorder::ReorderOutcome;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].selected = true;
        wizard.providers[1].selected = true;
        let llama = wizard
            .providers
            .iter()
            .position(|r| r.provider.id == "llama-cpp")
            .expect("the catalog offers llama.cpp");
        wizard.add_endpoint(llama);
        wizard.enter(Step::Defaults);
        wizard.cursor = Wizard::PROVIDER_FIELD;
        wizard.open_reorder();
        assert_eq!(wizard.reorder_field, Wizard::PROVIDER_FIELD);
        let mut reorder = wizard.reorder.take().expect("open");
        assert_eq!(
            reorder.rows_for_test(),
            vec![
                ("anthropic".to_string(), "Anthropic".to_string()),
                ("openai".to_string(), "OpenAI".to_string()),
                (
                    "llama-cpp".to_string(),
                    "llama.cpp at http://localhost:8080/v1".to_string()
                ),
            ]
        );

        // Enter as seeded keeps the order alone: the others are shown, not
        // in it.
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(
            reorder.handle_key(&key(KeyCode::Enter)),
            ReorderOutcome::Confirmed(vec!["anthropic".to_string()])
        );
        // Space brings one in.
        reorder.handle_key(&key(KeyCode::Down));
        reorder.handle_key(&key(KeyCode::Char(' ')));
        assert_eq!(
            reorder.handle_key(&key(KeyCode::Enter)),
            ReorderOutcome::Confirmed(vec!["anthropic".to_string(), "openai".to_string()])
        );

        // What the modal confirms is written back, head first.
        wizard.dirty = false;
        wizard.commit_reorder(vec!["openai".to_string(), "anthropic".to_string()]);
        assert!(wizard.dirty);
        assert_eq!(
            wizard.current_provider_order(),
            ["openai".to_string(), "anthropic".to_string()]
        );
        let config = wizard.build_config();
        assert_eq!(config.default_provider, "openai");
        assert_eq!(
            config.providers.provider_order,
            vec!["openai".to_string(), "anthropic".to_string()]
        );

        // Reopened, the order comes first and what is left out follows; a
        // rebuild keeps that choice rather than adding the rest back.
        wizard.commit_reorder(vec!["openai".to_string()]);
        wizard.open_reorder();
        let values: Vec<String> = wizard
            .reorder
            .as_ref()
            .expect("open")
            .rows_for_test()
            .into_iter()
            .map(|(value, _)| value)
            .collect();
        assert_eq!(values, ["openai", "anthropic", "llama-cpp"]);
        wizard.rebuild_defaults();
        assert_eq!(wizard.current_provider_order(), ["openai".to_string()]);
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
        wizard.show_advanced = true;
        wizard.enter(Step::Limits);
        assert_eq!(
            wizard.limits[Wizard::OVERRIDE_FIELD].value.options(),
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
            wizard.limits[Wizard::OVERRIDE_FIELD]
                .value
                .options()
                .contains(&"claude-opus-5".to_string()),
            "the picker should have refilled"
        );
        assert!(
            wizard.limits[Wizard::FALLBACK_FIELD]
                .value
                .options()
                .contains(&"claude-opus-5".to_string()),
            "both choosers share the list"
        );
    }

    #[tokio::test]
    async fn a_late_reply_rebuilds_the_defaults_screen_too() {
        // The Defaults screen's provider priority is built from the same
        // replies, so a reply landing while it is open rebuilds it as well.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let (_requests, replies) = wizard.take_verify_ends().expect("first take");
        wizard.providers[0].selected = true;
        wizard.enter(Step::Defaults);
        let before = wizard.defaults.len();
        replies
            .send(VerifyReply {
                provider_id: "anthropic".to_string(),
                outcome: Outcome::Reachable {
                    models: vec!["claude-opus-5".to_string()],
                },
            })
            .unwrap();
        wizard.drain_verifications();
        assert_eq!(wizard.defaults.len(), before, "the form keeps its shape");
        assert_eq!(wizard.step, Step::Defaults);
    }

    #[test]
    fn commit_picker_off_a_form_step_changes_no_field() {
        // The chooser only opens on a form step, but the commit is written
        // against the step the wizard is on when it lands: a screen with no
        // fields has nothing to write into and nothing to panic over.
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Providers);
        wizard.picker_purpose = PickerPurpose::Field(0);
        wizard.commit_picker(0);
        assert!(wizard.dirty);
        assert!(wizard.fields().is_empty());

        // A chooser that is a level of the add flow has no field to write.
        wizard.dirty = false;
        wizard.picker_purpose = PickerPurpose::Category;
        wizard.commit_picker(0);
        assert!(!wizard.dirty);
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

        assert_eq!(
            wizard.defaults[0]
                .value
                .order()
                .expect("an ordered priority"),
            ["ollama".to_string()]
        );
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
            override_model: Some("hand-typed".to_string()),
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

        wizard.show_advanced = true;
        wizard.enter(Step::Limits);

        let options = wizard.limits[Wizard::OVERRIDE_FIELD].value.options();
        assert!(options.contains(&Wizard::NO_DEFAULT_MODEL.to_string()));
        assert!(options.contains(&"claude-opus-5".to_string()));
        assert_eq!(
            wizard.limits[Wizard::OVERRIDE_FIELD].value.display(),
            "hand-typed",
            "a model already in the config must survive"
        );
        assert_eq!(
            wizard.limits[Wizard::FALLBACK_FIELD].value.display(),
            Wizard::NO_DEFAULT_MODEL,
            "nothing configured reads as the blueprints deciding"
        );
    }

    /// A choice made on the advanced screen is what gets written, survives
    /// leaving and re-entering the screen, and an explicit "(each blueprint
    /// decides)" there beats the model an endpoint entry picked for itself.
    #[test]
    fn the_advanced_model_choices_are_written_back_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.providers[0].selected = true;
        wizard.providers[0].outcome = Outcome::Reachable {
            models: vec!["m-1".to_string(), "m-2".to_string()],
        };
        wizard.show_advanced = true;
        wizard.enter(Step::Defaults);
        assert_eq!(wizard.build_config().override_model, None);
        assert_eq!(wizard.build_config().fallback_model, None);

        wizard.enter(Step::Limits);
        wizard.cursor = Wizard::OVERRIDE_FIELD;
        wizard.open_picker(
            "Override model",
            wizard.limits[Wizard::OVERRIDE_FIELD]
                .value
                .options()
                .to_vec(),
            0,
        );
        let pick = wizard.limits[Wizard::OVERRIDE_FIELD]
            .value
            .options()
            .iter()
            .position(|m| m == "m-2")
            .expect("reported");
        wizard.commit_picker(pick);
        wizard.cursor = Wizard::FALLBACK_FIELD;
        wizard.open_picker(
            "Fallback model",
            wizard.limits[Wizard::FALLBACK_FIELD]
                .value
                .options()
                .to_vec(),
            0,
        );
        let pick = wizard.limits[Wizard::FALLBACK_FIELD]
            .value
            .options()
            .iter()
            .position(|m| m == "m-1")
            .expect("reported");
        wizard.commit_picker(pick);
        assert_eq!(wizard.build_config().override_model.as_deref(), Some("m-2"));
        assert_eq!(wizard.build_config().fallback_model.as_deref(), Some("m-1"));

        // Out and back in: the choices are still there.
        wizard.enter(Step::Defaults);
        wizard.enter(Step::Limits);
        assert_eq!(wizard.limits[Wizard::OVERRIDE_FIELD].value.display(), "m-2");
        assert_eq!(wizard.limits[Wizard::FALLBACK_FIELD].value.display(), "m-1");

        // Choosing "(each blueprint decides)" is an answer, not an absence.
        wizard.cursor = Wizard::OVERRIDE_FIELD;
        wizard.open_picker(
            "Override model",
            wizard.limits[Wizard::OVERRIDE_FIELD]
                .value
                .options()
                .to_vec(),
            0,
        );
        wizard.commit_picker(0);
        assert_eq!(wizard.build_config().override_model, None);
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
        assert!(FieldValue::Order(vec!["a".into()]).options().is_empty());
    }

    /// `order` answers only for an ordered list, and its display is the values
    /// joined best-first (or "(none)" when empty).
    #[test]
    fn only_an_order_field_has_an_order() {
        let order = FieldValue::Order(vec!["codex".into(), "openai".into()]);
        assert_eq!(
            order.order(),
            Some(["codex".to_string(), "openai".to_string()].as_slice())
        );
        assert_eq!(order.display(), "codex > openai");
        assert_eq!(FieldValue::Order(Vec::new()).display(), "(none)");
        assert!(FieldValue::Number(Some(1)).order().is_none());
        assert!(FieldValue::Bool(true).order().is_none());
    }

    /// If the provider field somehow is not an ordered list, the helpers fall
    /// back to the base config rather than reading an order that is not there.
    #[test]
    fn the_provider_helpers_fall_back_when_the_field_is_not_an_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Defaults);
        wizard.defaults[0].value = FieldValue::Number(Some(1));
        assert_eq!(
            wizard.current_default_provider(),
            wizard.base.default_provider
        );
        assert!(wizard.current_provider_order().is_empty());
    }

    /// Opening the reorder modal on a field that is not an ordered list does
    /// nothing, the guard that lets `activate` route only order fields to it.
    #[test]
    fn open_reorder_on_a_non_order_field_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Defaults);
        wizard.cursor = Wizard::ADVANCED_FIELD; // the "Show advanced tuning" bool
        wizard.open_reorder();
        assert!(wizard.reorder.is_none());
    }

    /// `commit_reorder` for a field index that is not there writes nothing, and
    /// a field that is not the provider one skips the concurrency adjustment -
    /// the alternate arms of the field write and the head-follows-provider tweak.
    #[test]
    fn commit_reorder_out_of_range_and_off_the_provider_field_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        wizard.enter(Step::Defaults);
        wizard.reorder_field = 99; // no such field, and not the provider one
        wizard.commit_reorder(vec!["x".to_string()]);
        assert!(wizard.dirty, "the edit is still marked dirty");
        assert!(
            wizard
                .defaults
                .iter()
                .all(|f| f.value != FieldValue::Order(vec!["x".to_string()])),
            "nothing was written to a field that does not exist"
        );
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
            target: EditTarget::Field(1),
            line: crate::tui::widgets::line_edit::LineEdit::new("45".to_string(), false),
        });
        wizard.commit_edit();

        assert_eq!(wizard.defaults[1].value, FieldValue::Number(Some(45)));
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

        // Nothing chosen and nothing configured: both settings stay unset.
        assert!(wizard.build_config().override_model.is_none());
        assert!(wizard.build_config().fallback_model.is_none());
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
        // The two model choosers past the tuning fields are read by
        // `build_config` itself, not by `apply_limits_fields`.
        assert_eq!(wizard.limits.len(), LIMITS_FIXED + 2);
        wizard.limits.truncate(LIMITS_FIXED);
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
                leviath_providers::oauth::ProviderAuthStore::default_path().expect("a home is set");
            std::fs::create_dir_all(path.parent().expect("a parent")).unwrap();
            // Nobody signed in, nowhere to look, and a file that will not parse
            // are all the same answer: nothing to report.
            assert_eq!(signed_in_as(Some(&path), "codex"), None);
            assert_eq!(signed_in_as(None, "codex"), None);
            std::fs::write(&path, "{ not json").unwrap();
            assert_eq!(signed_in_as(Some(&path), "codex"), None);
            let mut store = leviath_providers::oauth::ProviderAuthStore::default();
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
            let mut store = leviath_providers::oauth::ProviderAuthStore::default();
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
            let mut store = leviath_providers::oauth::ProviderAuthStore::default();
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

    /// With no modal open there is no provider card, so there are no action
    /// rows rather than the first row's, and the Providers screen with
    /// nothing configured is only its add row.
    #[test]
    fn with_no_modal_open_there_are_no_detail_actions() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        for row in &mut wizard.providers {
            row.selected = false;
        }
        wizard.enter(Step::Providers);

        assert!(wizard.detail_actions().is_empty());
        assert_eq!(wizard.detail_row(), None);
        assert_eq!(wizard.row_count(), 1, "the add row");
        assert_eq!(wizard.modal_card_rows(), 0);
    }

    /// Every provider the wizard offers survives being chosen.
    ///
    /// Over `catalog::providers()` rather than a written-out list, so a
    /// provider added to that table is covered the day it is added. This is
    /// the invariant the whole screen exists to produce, and nothing else
    /// asserted it end to end: Codex shipped selected, signed in, and switched
    /// back off by `build_config`, because a browser sign-in types nothing and
    /// the empty-value arm read that as "no credential given".
    #[test]
    fn every_offered_provider_is_configured_by_choosing_it() {
        let dir = tempfile::tempdir().unwrap();
        for provider in catalog::providers() {
            // Endpoint presets are their entries, not a row credential, and
            // have their own tests; everything else answers here.
            if provider.credential == Credential::Endpoint {
                continue;
            }
            let mut wizard = test_wizard(dir.path());
            let index = wizard
                .providers
                .iter()
                .position(|r| r.provider.id == provider.id)
                .expect("the row this came from");
            for row in &mut wizard.providers {
                row.selected = false;
                row.value = String::new();
            }

            // Unselected: not configured, whatever the file said before.
            let off = wizard.build_config();
            assert!(
                !catalog::is_configured(&off, provider.id),
                "'{}' is configured without being chosen",
                provider.id
            );

            // Chosen, and given whatever its kind actually needs. A sign-in
            // needs nothing typed, which is the case that broke.
            wizard.providers[index].selected = true;
            wizard.providers[index].value = match provider.credential {
                Credential::ApiKey => "a-credential".to_string(),
                // Not the default one: see the separate case below.
                Credential::BaseUrl => "http://elsewhere:11434".to_string(),
                Credential::Signin | Credential::Endpoint => String::new(),
            };
            let on = wizard.build_config();
            assert!(
                catalog::is_configured(&on, provider.id),
                "choosing '{}' did not configure it",
                provider.id
            );
        }
    }

    /// A base-URL provider left on its default is configured, and its
    /// address is still not written down.
    ///
    /// The choice and the address are two fields saying two different
    /// things. Were the URL the only record of the choice, keeping the
    /// default would record nothing: the provider would be missing from
    /// `configured_providers`, unpickable as the default and forgotten by
    /// the wizard's next run. Storing the default address instead would pin
    /// it and stop `$OLLAMA_HOST` applying.
    #[test]
    fn a_base_url_provider_left_on_its_default_is_still_chosen() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = test_wizard(dir.path());
        let index = wizard
            .providers
            .iter()
            .position(|r| r.provider.credential == Credential::BaseUrl)
            .expect("the catalog offers one");
        for row in &mut wizard.providers {
            row.selected = false;
            row.value = String::new();
        }
        wizard.providers[index].selected = true;
        wizard.providers[index].value = catalog::DEFAULT_OLLAMA_URL.to_string();

        let config = wizard.build_config();
        let id = wizard.providers[index].provider.id;
        assert!(
            catalog::is_configured(&config, id),
            "'{id}' was chosen and is not configured"
        );
        assert_eq!(
            config.ollama_base_url, None,
            "the default address was pinned, so $OLLAMA_HOST stops applying"
        );

        // And a *different* address is written down, because that one is not
        // something any default would supply.
        wizard.providers[index].value = "http://elsewhere:11434".to_string();
        let config = wizard.build_config();
        assert_eq!(
            config.ollama_base_url.as_deref(),
            Some("http://elsewhere:11434")
        );
    }

    /// The codex row at its index, on a wizard whose only selection it is,
    /// which is signed in to nothing, with its setup modal open.
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
        wizard.enter(Step::Providers);
        wizard.open_provider_modal(index);
        (wizard, index)
    }

    /// The buttons on offer follow the sign-in, because one of them does
    /// nothing without one; the check is the modal's own "Verify and use".
    #[test]
    fn a_sign_in_row_offers_more_once_it_is_signed_in() {
        let dir = tempfile::tempdir().unwrap();
        let (mut wizard, index) = wizard_showing_codex(dir.path());

        assert_eq!(
            wizard.detail_actions(),
            vec![DetailAction::SignIn, DetailAction::OpenSignup]
        );
        // No credential row, so the buttons are the whole card, and the
        // first is the one this card exists for.
        assert!(!wizard.detail_has_credential_row(index));
        assert_eq!(wizard.row_count(), 2);
        assert_eq!(wizard.nav_rows(), 5);
        assert_eq!(
            wizard.detail_action_at(index, 0),
            Some(DetailAction::SignIn)
        );
        assert_eq!(wizard.modal_button_at(2), Some(ModalButton::VerifyUse));

        wizard.providers[index].signed_in = Some("a@b.c".to_string());
        assert_eq!(
            wizard.detail_actions(),
            vec![
                DetailAction::SignIn,
                DetailAction::SignOut,
                DetailAction::OpenSignup,
            ],
            "once signed in, there is a sign-in to forget"
        );
        assert_eq!(wizard.row_count(), 3);
        assert_eq!(wizard.modal_button_at(2), None);
        assert_eq!(wizard.modal_button_at(3), Some(ModalButton::VerifyUse));
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
        wizard.enter(Step::Providers);
        wizard.open_provider_modal(index);

        assert!(wizard.detail_has_credential_row(index));
        assert_eq!(wizard.detail_action_at(index, 0), None, "row 0 is the key");
        assert_eq!(
            wizard.detail_action_at(index, 1),
            Some(DetailAction::OpenSignup)
        );
        assert_eq!(wizard.row_count(), 2);
        assert_eq!(wizard.modal_button_at(2), Some(ModalButton::VerifyUse));
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
