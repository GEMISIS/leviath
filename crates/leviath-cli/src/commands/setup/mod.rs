//! `lev setup` - the guided path from "just installed Leviath" to "ready to run
//! an agent".
//!
//! The previous version was nine `print!`/`read_line` prompts in a fixed order:
//! it asked every user for four API keys whether they had them or not, echoed
//! them in plaintext, touched about eight of `Config`'s twenty-odd fields, and
//! knew nothing about MCP servers or agent blueprints. It also ended by
//! claiming "All API keys look valid" on the strength of a `starts_with`
//! check. A fresh install came out the other side with a config file and no
//! agents.
//!
//! This is a ratatui wizard instead: pick the providers you actually use,
//! configure and verify each, set defaults and limits, install the bundled
//! blueprints, and import MCP servers already configured in other harnesses.
//!
//! ## Shape
//!
//! * `state` - what step we're on and what's been chosen. Pure data.
//! * `input` - key handling.
//! * `render` - drawing.
//! * `plan` - the decisions as plain data, and the only code that writes.
//! * `catalog` - which providers exist and how each is configured.
//! * [`import`] - MCP servers found in other tools.
//! * [`verify`] - proving a credential works.
//! * [`signin`] - taking a browser sign-in for a provider that has no key.
//!
//! The terminal is a *front-end*, not the feature: everything it collects lands
//! in a `plan::SetupPlan`, and `--non-interactive` builds the same struct
//! from flags. A future mobile or web host would be a third builder with
//! nothing downstream changing - which is why none of the platform-shaped parts
//! (scanning a home directory, taking over a TTY) are prescribed anywhere but
//! here.

pub(crate) mod catalog;
pub mod import;
pub(crate) mod input;
pub(crate) mod plan;
pub(crate) mod render;
pub mod signin;
pub(crate) mod state;
pub mod verify;

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::tui::{EventSource, TerminalSetup};
use crossterm::event::{Event, KeyEventKind};
use state::{VerifyReply, VerifyRequest, Wizard};
use verify::ProviderVerifier;

/// Arguments for `lev setup`.
#[derive(Args)]
pub struct SetupArgs {
    /// Run non-interactively using only flag values (useful for scripting)
    #[arg(long)]
    pub non_interactive: bool,

    /// Skip checking credentials against the provider APIs
    #[arg(long)]
    pub no_verify: bool,

    /// Anthropic API key
    #[arg(long)]
    pub anthropic_key: Option<String>,

    /// OpenAI API key
    #[arg(long)]
    pub openai_key: Option<String>,

    /// Google AI (Gemini) API key
    #[arg(long)]
    pub google_key: Option<String>,

    /// OpenRouter API key
    #[arg(long)]
    pub openrouter_key: Option<String>,

    /// Ollama base URL (default: http://localhost:11434)
    #[arg(long)]
    pub ollama_url: Option<String>,

    /// Default model override (e.g. claude-sonnet-4-6)
    #[arg(long)]
    pub default_model: Option<String>,

    /// Enable the Claude Code CLI transport (runs on your Claude subscription
    /// instead of an API key). Off unless set: the CLI adds its own context to
    /// every call, including your account email address.
    #[arg(long)]
    pub claude_code: Option<bool>,

    /// Reasoning effort for the Claude Code transport
    /// (low, medium, high, xhigh, max)
    #[arg(long)]
    pub claude_code_effort: Option<String>,

    /// Enable the Codex transport (runs on your ChatGPT subscription instead
    /// of an API key). This flag only flips the switch: interactive `lev setup`
    /// signs in from its own screen, and a non-interactive run has nobody
    /// watching a browser, so on this path sign in with `lev auth login codex`.
    #[arg(long)]
    pub codex: Option<bool>,

    /// Install the bundled agent blueprints without asking
    #[arg(long)]
    pub install_agents: bool,
}

/// Reads an environment variable. Injected so a test can hand the wizard a
/// fixed environment instead of the developer's real one.
pub(crate) type EnvLookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Everything the wizard needs from the outside world, injected so tests point
/// it at tempdirs and a fake environment instead of the developer's real home.
pub struct SetupEnv {
    /// Where the config is read from and written back to.
    pub config_path: PathBuf,
    /// Where bundled blueprints are installed.
    pub agents_dir: PathBuf,
    /// Roots for the harness scan.
    pub roots: import::Roots,
    /// Reads an environment variable.
    pub env_lookup: EnvLookup,
    /// Opens a URL in a browser.
    pub opener: leviath_mcp::BrowserOpener,
    /// Where the offers this user has already turned down are remembered (see
    /// [`crate::ui_state`]). `None` reads and writes nothing, which is what
    /// every test gets, so no test can reach the real file.
    pub ui_state_path: Option<PathBuf>,
}

// The real `SetupEnv` - the user's actual home, a real `std::env` lookup, and
// a real browser - is built in the binary, where those leaves belong. Nothing
// in the library reaches the real environment, so no test can either.

/// The non-interactive arm: apply flags to the config on disk and save.
///
/// Kept working byte-for-byte because it is the documented headless path and an
/// integration test spawns the real binary through it.
pub fn run_non_interactive(args: &SetupArgs, env: &SetupEnv) -> anyhow::Result<()> {
    let mut config = Config::load_from_path_public(&env.config_path).unwrap_or_default();
    apply_flags(&mut config, args);

    let agents = if args.install_agents {
        crate::bundled::plan_agent_actions(&env.agents_dir)
            .into_iter()
            // `preselect`, not `is_change`: the headless path must not overwrite
            // a blueprint the user edited, any more than the wizard does.
            .filter(|(_, action)| action.preselect())
            .map(|(agent, _)| agent)
            .collect()
    } else {
        Vec::new()
    };

    let applied = plan::apply(
        // Nothing was offered here, so nothing was declined: the headless arm
        // takes its answer from flags and must not rewrite what the wizard
        // remembered about a person's choices.
        &plan::SetupPlan {
            config,
            agents,
            declined: Default::default(),
        },
        &env.config_path,
        &env.agents_dir,
        None,
    )?;
    report(&applied);
    Ok(())
}

/// Print what happened. Shared by both arms so the closing summary reads the
/// same however setup was driven.
fn report(applied: &plan::Applied) {
    println!("Config saved to {}", applied.config_path.display());
    if !applied.agents_installed.is_empty() {
        println!(
            "Installed {} agent(s): {}",
            applied.agents_installed.len(),
            applied.agents_installed.join(", ")
        );
    }
    for warning in &applied.warnings {
        println!("  Warning: {warning}");
    }
}

/// Copy the flag values onto a config.
fn apply_flags(config: &mut Config, args: &SetupArgs) {
    if let Some(ref k) = args.anthropic_key {
        config.providers.anthropic_api_key = Some(k.clone());
    }
    if let Some(ref k) = args.openai_key {
        config.providers.openai_api_key = Some(k.clone());
    }
    if let Some(ref k) = args.google_key {
        config.providers.google_api_key = Some(k.clone());
    }
    if let Some(ref k) = args.openrouter_key {
        config.openrouter_api_key = Some(k.clone());
    }
    if let Some(ref u) = args.ollama_url {
        config.ollama_base_url = Some(u.clone());
    }
    if let Some(ref m) = args.default_model {
        config.default_model = Some(m.clone());
    }
    if let Some(enabled) = args.claude_code {
        config.providers.claude_code_enabled = enabled;
    }
    if let Some(enabled) = args.codex {
        config.providers.codex_enabled = enabled;
    }
    if let Some(ref e) = args.claude_code_effort {
        config.providers.claude_code_effort = Some(e.clone());
    }
    retarget_default_provider(config);
}

/// The providers this config holds a credential for, best first.
///
/// Ollama sits last on purpose. It needs no key, so a config that merely
/// mentions it is not a statement of preference, and putting it first would
/// make it the default on a machine that never installed it. An
/// OpenAI-compatible endpoint was written deliberately, so it sits with the
/// keyed providers, after them and in name order.
pub(crate) fn configured_providers(config: &Config) -> Vec<String> {
    let mut endpoints: Vec<&str> = config
        .model_providers
        .iter()
        .filter(|(_, e)| e.is_endpoint())
        .map(|(name, _)| name.as_str())
        .collect();
    endpoints.sort_unstable();
    let kind = |id: &str| {
        catalog::providers()
            .into_iter()
            .find(|p| p.id == id)
            .map(|p| p.credential)
    };
    let chosen: Vec<&'static str> = catalog::configured(config)
        .into_iter()
        // An endpoint *preset* is not a provider name: the entries under it
        // are, and they are listed by name below.
        .filter(|id| kind(id) != Some(catalog::Credential::Endpoint))
        // Enabled *and* signed in. Enabled alone would let a headless
        // `--codex true` make an unauthenticated provider the host default,
        // and the very next run would fail on a credential nobody was asked
        // for.
        .filter(|id| *id != leviath_providers::codex::PROVIDER_NAME || codex_grant_exists())
        .collect();
    // A provider that needed no credential sorts last, and the first name in
    // this list is what `--default-provider` picks when nothing else says.
    // A local server the user happens to have is the weakest thing to choose
    // on somebody's behalf: it should lose to every key they went and got,
    // and to every endpoint they wrote down.
    let (unkeyed, keyed): (Vec<&str>, Vec<&str>) = chosen
        .into_iter()
        .partition(|id| kind(id) == Some(catalog::Credential::BaseUrl));
    keyed
        .into_iter()
        // The Claude Code transport has no catalog row: the wizard omits it
        // by a pinned test, because the CLI adds its own context to every
        // call. It is still a provider a run can use, so it is named here.
        .chain(
            config
                .providers
                .claude_code_enabled
                .then_some("claude-code"),
        )
        .chain(endpoints)
        .chain(unkeyed)
        .map(str::to_string)
        .collect()
}

/// Whether a Codex sign-in has actually been taken.
fn codex_grant_exists() -> bool {
    leviath_providers::codex::ProviderAuthStore::default_path()
        .and_then(|path| leviath_providers::codex::ProviderAuthStore::load(&path).ok())
        .is_some_and(|store| store.get(leviath_providers::codex::PROVIDER_NAME).is_some())
}

/// Point `default_provider` at a provider this config can actually reach.
///
/// It defaults to `anthropic` and nothing in non-interactive mode ever moved
/// it, so `lev setup --non-interactive --openrouter-key ...` produced a config
/// whose very next `lev doctor` said it "resolved to 'anthropic', which is not
/// configured". The install was fine; the default was pointing at a provider
/// the user had not asked for.
///
/// Only ever moves a default that is unreachable, so a deliberate choice
/// already in the file survives.
fn retarget_default_provider(config: &mut Config) {
    let configured = configured_providers(config);
    if configured.contains(&config.default_provider) {
        return;
    }
    if let Some(first) = configured.first() {
        config.default_provider = first.clone();
    }
}

/// Build a wizard against `env`.
///
/// The base config comes from reading the *file*, deliberately not from
/// `Config::load()`: `load` folds `$ANTHROPIC_API_KEY` and friends in, and the
/// old wizard re-serialized the whole struct - quietly writing into
/// `~/.leviath/config.toml` a key the user had chosen to keep in their
/// environment. Those are tracked separately and shown as such.
pub fn build_wizard(env: &SetupEnv) -> Wizard {
    let base = Config::load_from_path_public(&env.config_path).unwrap_or_default();
    let (candidates, errors) = state::candidates_from_scans(import::scan(&env.roots));
    let remembered = env
        .ui_state_path
        .as_deref()
        .map(|p| crate::ui_state::load(p).setup)
        .unwrap_or_default();
    Wizard::new(
        base,
        &env.env_lookup,
        candidates,
        errors,
        &env.agents_dir,
        env.opener.clone(),
        remembered,
    )
}

/// Answer verification requests until the wizard drops its sender.
///
/// Sequential rather than fanned out: the answers land on separate provider
/// cards a user reads one at a time, and firing six requests at once buys
/// nothing but a chance to trip a rate limiter with what is supposed to be a
/// harmless check.
pub async fn verification_loop<V: ProviderVerifier>(
    verifier: V,
    mut requests: mpsc::UnboundedReceiver<VerifyRequest>,
    replies: mpsc::UnboundedSender<VerifyReply>,
) {
    while let Some(request) = requests.recv().await {
        let outcome = verifier.verify(&request.creds).await;
        // A closed receiver means the wizard exited; nothing left to report to.
        if replies
            .send(VerifyReply {
                provider_id: request.provider_id,
                outcome,
            })
            .is_err()
        {
            return;
        }
    }
}

/// The wizard's draw/input loop.
///
/// Generic over the backend and event source so it runs against a
/// `TestBackend` and canned keys; the real crossterm bindings live in the
/// binary. Returns the plan to apply, or `None` if the user quit.
pub(crate) async fn run_wizard_loop<B: ratatui::backend::Backend>(
    wizard: &mut Wizard,
    terminal: &mut Terminal<B>,
    events: &mut impl EventSource,
    tick_rate: Duration,
) -> anyhow::Result<Option<plan::SetupPlan>> {
    loop {
        wizard.ticks += 1;
        wizard.drain_verifications();
        wizard.drain_signins();
        // The area is taken from the frame that was actually drawn, so a click
        // resolves against the layout the user was looking at rather than
        // against a size asked for separately afterwards.
        let mut area = ratatui::layout::Rect::default();
        terminal
            .draw(|frame| {
                area = frame.area();
                render::draw(frame, wizard);
            })
            // ratatui 0.30 made the backend error an associated type with no
            // Send/Sync guarantee, so convert by message rather than by `?`.
            .map_err(|e| anyhow::anyhow!("terminal draw failed: {e}"))?;

        match events.poll_event(tick_rate)? {
            Some(Event::Key(key))
                if key.kind == KeyEventKind::Press
                    && wizard.handle_key(key) == input::Action::Save =>
            {
                wizard.finished = true;
            }
            Some(Event::Mouse(mouse))
                if wizard.handle_mouse(mouse, area) == input::Action::Save =>
            {
                wizard.finished = true;
            }
            _ => {}
        }

        if wizard.finished {
            return Ok(Some(wizard.build_plan()));
        }
        if wizard.should_quit {
            return Ok(None);
        }
    }
}

/// Set up the terminal, run the loop, tear the terminal down, then apply.
///
/// The teardown happens before anything is printed: writing a summary while the
/// alternate screen is still up puts it somewhere the user will never see.
pub async fn execute_core<S: TerminalSetup, E: EventSource>(
    wizard: &mut Wizard,
    env: &SetupEnv,
    setup: &mut S,
    events: &mut E,
) -> anyhow::Result<()> {
    setup.enable()?;
    let mut terminal = setup.create_terminal()?;
    let result = run_wizard_loop(wizard, &mut terminal, events, Duration::from_millis(120)).await;
    setup.disable();

    match result? {
        Some(plan) => {
            let applied = plan::apply(
                &plan,
                &env.config_path,
                &env.agents_dir,
                env.ui_state_path.as_deref(),
            )?;
            report(&applied);
            print_next_steps(&applied);
        }
        None => println!("Setup cancelled. Nothing was written."),
    }
    Ok(())
}

/// What to do now that setup is done.
fn print_next_steps(applied: &plan::Applied) {
    println!();
    match applied.agents_installed.first() {
        Some(agent) => println!("Try it:  lev run {agent} --task \"...\""),
        None => println!("Install an agent with `lev setup`, then `lev run <agent>`."),
    }
}

/// `lev setup`: the flags path, or the wizard.
///
/// `is_terminal` is injected because the answer is a property of the real
/// process's stdout, and a wizard that starts on a pipe would take over a
/// terminal that isn't there.
pub async fn execute_with<S: TerminalSetup, E: EventSource>(
    args: &SetupArgs,
    env: &SetupEnv,
    setup: &mut S,
    events: &mut E,
    is_terminal: bool,
) -> anyhow::Result<()> {
    if args.non_interactive {
        return run_non_interactive(args, env);
    }
    if !is_terminal {
        anyhow::bail!(
            "lev setup needs a terminal. For scripted use:\n  \
             lev setup --non-interactive --anthropic-key sk-ant-... --install-agents"
        );
    }
    let mut wizard = build_wizard(env);
    execute_core(&mut wizard, env, setup, events).await
}

/// Resolve `~/.leviath/agents` for the real environment.
pub fn real_agents_dir(home: Option<&Path>) -> PathBuf {
    home.unwrap_or(Path::new(""))
        .join(".leviath")
        .join("agents")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundled::BUNDLED_AGENTS;
    use crate::tui::{TestEventSource, TestSetup, key, key_with, test_terminal};
    use crossterm::event::{KeyCode, KeyModifiers};

    /// Args with everything off, so each test names only what it exercises.
    fn args() -> SetupArgs {
        SetupArgs {
            non_interactive: false,
            no_verify: false,
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
            claude_code: None,
            claude_code_effort: None,
            codex: None,
            install_agents: false,
        }
    }

    /// A `SetupEnv` rooted entirely in a tempdir, with a browser opener that
    /// records instead of launching and an environment that is simply empty.
    fn env_in(dir: &Path) -> SetupEnv {
        SetupEnv {
            config_path: dir.join("config.toml"),
            agents_dir: dir.join("agents"),
            roots: import::Roots {
                home: dir.join("home"),
                os_config: dir.join("os-config"),
                xdg_config: dir.join("home").join(".config"),
                cwd: dir.join("cwd"),
            },
            env_lookup: Box::new(|_| None),
            opener: std::sync::Arc::new(|_| true),
            // Inside the tempdir like everything else, so a test that applies
            // a plan writes its declines here and never to the real file.
            ui_state_path: Some(dir.join("ui-state.json")),
        }
    }

    // ─── default_provider retargeting ───────────────────────────────────────

    #[test]
    fn a_single_non_anthropic_key_becomes_the_default_provider() {
        // The bug this exists for: setup succeeded, then `lev doctor` said the
        // install resolved to a provider the user had never configured.
        let mut config = Config::default();
        assert_eq!(config.default_provider, "anthropic");
        apply_flags(
            &mut config,
            &SetupArgs {
                openrouter_key: Some("sk-or-test".to_string()),
                ..args()
            },
        );
        assert_eq!(config.default_provider, "openrouter");
    }

    #[test]
    fn a_reachable_default_provider_is_left_alone() {
        let mut config = Config::default();
        apply_flags(
            &mut config,
            &SetupArgs {
                anthropic_key: Some("sk-ant-test".to_string()),
                openrouter_key: Some("sk-or-test".to_string()),
                ..args()
            },
        );
        assert_eq!(config.default_provider, "anthropic");
    }

    #[test]
    fn a_deliberate_default_provider_survives() {
        let mut config = Config {
            default_provider: "google".to_string(),
            ..Config::default()
        };
        apply_flags(
            &mut config,
            &SetupArgs {
                google_key: Some("AIza-test".to_string()),
                openrouter_key: Some("sk-or-test".to_string()),
                ..args()
            },
        );
        assert_eq!(config.default_provider, "google");
    }

    /// An endpoint written by hand is a deliberate provider, so it can be
    /// the default, after any keyed provider and before Ollama.
    #[test]
    fn an_endpoint_entry_can_become_the_default_provider() {
        use crate::config::{ModelProviderConfig, ModelProviderKind};
        let mut config = Config {
            ollama_base_url: Some("http://localhost:11434".to_string()),
            ..Config::default()
        };
        for name in ["zeta", "alpha"] {
            config.model_providers.insert(
                name.to_string(),
                ModelProviderConfig {
                    kind: Some(ModelProviderKind::OpenaiCompatible),
                    base_url: Some("http://h/v1".to_string()),
                    ..Default::default()
                },
            );
        }
        // A script entry is not a provider this can point at.
        config.model_providers.insert(
            "groq".to_string(),
            ModelProviderConfig {
                script: Some("groq.rhai".to_string()),
                ..Default::default()
            },
        );
        // Ollama last: a local server nobody had to sign up for should not
        // outrank an endpoint the user wrote down when something has to pick
        // a default.
        assert_eq!(configured_providers(&config), ["alpha", "zeta", "ollama"]);
        apply_flags(&mut config, &args());
        assert_eq!(config.default_provider, "alpha");

        // A key still comes first.
        apply_flags(
            &mut config,
            &SetupArgs {
                openai_key: Some("sk-test".to_string()),
                ..args()
            },
        );
        assert_eq!(configured_providers(&config)[0], "openai");
        assert_eq!(
            config.default_provider, "alpha",
            "already reachable, so kept"
        );
    }

    #[test]
    fn configuring_nothing_leaves_the_default_provider_untouched() {
        // Nothing to retarget to, so moving it would only make it wrong
        // differently.
        let mut config = Config::default();
        apply_flags(&mut config, &args());
        assert_eq!(config.default_provider, "anthropic");
    }

    #[test]
    fn ollama_is_the_last_provider_considered() {
        // It needs no key, so it is the one most likely to be present by
        // accident. Anything the user actually holds a credential for wins.
        let mut config = Config::default();
        apply_flags(
            &mut config,
            &SetupArgs {
                ollama_url: Some("http://localhost:11434".to_string()),
                google_key: Some("AIza-test".to_string()),
                ..args()
            },
        );
        assert_eq!(config.default_provider, "google");

        let mut ollama_only = Config::default();
        apply_flags(
            &mut ollama_only,
            &SetupArgs {
                ollama_url: Some("http://localhost:11434".to_string()),
                ..args()
            },
        );
        assert_eq!(ollama_only.default_provider, "ollama");
    }

    #[test]
    fn the_codex_transport_counts_only_once_it_is_signed_in() {
        // Enabled alone would let a headless run retarget `default_provider`
        // onto a provider with no credential, and the next run would fail on
        // one nobody had been asked for.
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || {
            let mut config = Config::default();
            config.providers.codex_enabled = true;
            assert!(!codex_grant_exists());
            assert!(!configured_providers(&config).contains(&"codex".to_string()));

            let path =
                leviath_providers::codex::ProviderAuthStore::default_path().expect("a home is set");
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

            assert!(codex_grant_exists());
            assert!(configured_providers(&config).contains(&"codex".to_string()));

            // And a grant with the provider turned off still does not count.
            config.providers.codex_enabled = false;
            assert!(!configured_providers(&config).contains(&"codex".to_string()));
        });
    }

    /// Choosing a provider in the wizard reaches the runtime: it is offered
    /// as a default, and it is built into the credentials a run resolves
    /// against.
    ///
    /// Over `catalog::providers()`, so a provider added to that table is
    /// covered the day it is added rather than the day somebody remembers.
    ///
    /// Adding a provider means touching about eight places - the catalog row,
    /// three match arms in `catalog`, the credential arm in `build_config`,
    /// the tuple in `configured_providers`, the creds in
    /// `provider_creds_from_config` and the registry arm in
    /// `provider_creds` - and nothing held them together. Codex reached the
    /// last two and stopped at the fifth, so it signed in, congratulated the
    /// user, and vanished. This is the test that says so.
    #[test]
    fn every_offered_provider_reaches_the_runtime_when_it_is_chosen() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || {
            // Codex counts as configured only once it is signed in, so the
            // grant has to exist before the question is asked.
            let grants =
                leviath_providers::codex::ProviderAuthStore::default_path().expect("a home is set");
            let mut store = leviath_providers::codex::ProviderAuthStore::default();
            store.set(
                "codex",
                leviath_providers::ProviderGrant {
                    access_token: "at".to_string(),
                    refresh_token: "rt".to_string(),
                    ..Default::default()
                },
            );
            store.save(&grants).unwrap();

            for provider in crate::commands::setup::catalog::providers() {
                // An endpoint preset writes `[model_providers]` entries under
                // a name the user picks, not a credential under its own id.
                // Those have their own tests.
                if provider.credential == crate::commands::setup::catalog::Credential::Endpoint {
                    continue;
                }
                let mut wizard = crate::commands::setup::state::tests::test_wizard(dir.path());
                let index = wizard
                    .providers
                    .iter()
                    .position(|r| r.provider.id == provider.id)
                    .expect("the row this came from");
                for row in &mut wizard.providers {
                    row.selected = false;
                    row.value = String::new();
                }
                wizard.providers[index].selected = true;
                wizard.providers[index].value = match provider.credential {
                    crate::commands::setup::catalog::Credential::ApiKey => {
                        "a-credential".to_string()
                    }
                    // Not the default: that deliberately writes nothing, and
                    // `state::tests` asserts it separately.
                    crate::commands::setup::catalog::Credential::BaseUrl => {
                        "http://elsewhere:11434".to_string()
                    }
                    _ => String::new(),
                };

                let config = wizard.build_config();
                assert!(
                    configured_providers(&config).contains(&provider.id.to_string()),
                    "'{}' cannot be chosen as the default provider after being configured",
                    provider.id
                );
                let creds = crate::commands::run::session::provider_creds_from_config(&config);
                // Named up front rather than formatted into the assertion: a
                // call that only runs when it fails is a region no passing
                // test reaches.
                let built: Vec<&String> = creds.iter().map(|c| &c.name).collect();
                assert!(
                    built.iter().any(|name| *name == provider.id),
                    "'{}' is configured but a run would not build it: {built:?}",
                    provider.id
                );
            }
        });
    }

    /// The reported bug, end to end: choose Codex in the wizard, apply, and
    /// find it in the file and on the default-provider list.
    ///
    /// The unit above proves `configured_providers` reads the two flags
    /// correctly; it did not prove anything ever wrote the first one. It did
    /// not: a browser sign-in types nothing, `build_config` read that as "no
    /// credential given", and the sign-in was switched off at Apply. The
    /// symptom was a wizard that connected, congratulated you, and left a
    /// config with `codex_enabled` unset - no `codex/...` models, and Codex
    /// missing from the default-provider choices on the next run through.
    #[test]
    fn choosing_codex_in_the_wizard_reaches_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || {
            let grants =
                leviath_providers::codex::ProviderAuthStore::default_path().expect("a home is set");
            let mut store = leviath_providers::codex::ProviderAuthStore::default();
            store.set(
                "codex",
                leviath_providers::ProviderGrant {
                    access_token: "at".to_string(),
                    refresh_token: "rt".to_string(),
                    ..Default::default()
                },
            );
            store.save(&grants).unwrap();

            let mut wizard = crate::commands::setup::state::tests::test_wizard(dir.path());
            let index = wizard
                .providers
                .iter()
                .position(|r| r.provider.id == "codex")
                .expect("the codex row is offered");
            for row in &mut wizard.providers {
                row.selected = false;
            }
            wizard.providers[index].selected = true;

            let config_path = dir.path().join("config.toml");
            let plan = wizard.build_plan();
            plan::apply(&plan, &config_path, &dir.path().join("agents"), None)
                .expect("the plan applies");

            let saved = Config::load_from_path_public(&config_path).expect("it reads back");
            assert!(
                saved.providers.codex_enabled,
                "the sign-in was switched off on the way to disk"
            );
            assert!(
                configured_providers(&saved).contains(&"codex".to_string()),
                "codex is not offered as a default provider"
            );

            // And the text really is in the file, not only in the struct.
            let toml = std::fs::read_to_string(&config_path).expect("the file");
            assert!(toml.contains("codex_enabled"), "{toml}");
        });
    }

    /// The flag sets the switch and nothing else: it never opens a browser,
    /// because nothing is watching one in a non-interactive run.
    #[test]
    fn the_codex_flag_only_flips_the_switch() {
        let mut config = Config::default();
        let mut args = args();
        args.codex = Some(true);
        apply_flags(&mut config, &args);
        assert!(config.providers.codex_enabled);

        args.codex = Some(false);
        apply_flags(&mut config, &args);
        assert!(!config.providers.codex_enabled);
    }

    #[test]
    fn the_claude_code_transport_counts_as_a_configured_provider() {
        let mut config = Config::default();
        apply_flags(
            &mut config,
            &SetupArgs {
                claude_code: Some(true),
                ..args()
            },
        );
        assert_eq!(config.default_provider, "claude-code");
    }

    // ─── the non-interactive path ───────────────────────────────────────────

    #[test]
    fn flags_are_written_to_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: Some("sk-ant-x".to_string()),
            openai_key: Some("sk-oai".to_string()),
            google_key: Some("goog".to_string()),
            openrouter_key: Some("sk-or".to_string()),
            ollama_url: Some("http://box:11434".to_string()),
            default_model: Some("m".to_string()),
            claude_code: Some(true),
            claude_code_effort: Some("xhigh".to_string()),
            ..args()
        };

        run_non_interactive(&args, &env).unwrap();

        let written = Config::load_from_path_public(&env.config_path).unwrap();
        assert_eq!(
            written.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-x")
        );
        assert_eq!(written.providers.openai_api_key.as_deref(), Some("sk-oai"));
        assert_eq!(written.providers.google_api_key.as_deref(), Some("goog"));
        assert_eq!(written.openrouter_api_key.as_deref(), Some("sk-or"));
        assert_eq!(written.ollama_base_url.as_deref(), Some("http://box:11434"));
        assert_eq!(written.default_model.as_deref(), Some("m"));
        assert!(written.providers.claude_code_enabled);
        assert_eq!(
            written.providers.claude_code_effort.as_deref(),
            Some("xhigh")
        );
    }

    #[test]
    fn the_non_interactive_path_installs_agents_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());

        run_non_interactive(&args(), &env).unwrap();
        assert!(!env.agents_dir.exists(), "nothing was asked for");

        run_non_interactive(
            &SetupArgs {
                install_agents: true,
                ..args()
            },
            &env,
        )
        .unwrap();
        assert!(
            env.agents_dir.join(BUNDLED_AGENTS[0].name).exists(),
            "every bundled blueprint should land"
        );

        // Second time round there is nothing left to do.
        run_non_interactive(
            &SetupArgs {
                install_agents: true,
                ..args()
            },
            &env,
        )
        .unwrap();
        assert!(env.agents_dir.join(BUNDLED_AGENTS[0].name).exists());
    }

    #[test]
    fn the_non_interactive_path_keeps_settings_it_was_not_given() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        run_non_interactive(
            &SetupArgs {
                anthropic_key: Some("sk-ant-first".to_string()),
                ..args()
            },
            &env,
        )
        .unwrap();

        run_non_interactive(
            &SetupArgs {
                openai_key: Some("sk-oai".to_string()),
                ..args()
            },
            &env,
        )
        .unwrap();

        let written = Config::load_from_path_public(&env.config_path).unwrap();
        assert_eq!(
            written.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-first")
        );
        assert_eq!(written.providers.openai_api_key.as_deref(), Some("sk-oai"));
    }

    #[test]
    fn a_config_that_cannot_be_written_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("not-a-dir");
        std::fs::write(&blocked, "").unwrap();
        let mut env = env_in(dir.path());
        env.config_path = blocked.join("config.toml");

        assert!(run_non_interactive(&args(), &env).is_err());
    }

    // ─── building the wizard from the environment ───────────────────────────

    #[test]
    fn the_wizard_reads_the_config_file_and_scans_for_harnesses() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        std::fs::create_dir_all(&env.roots.home).unwrap();
        std::fs::write(
            env.roots.home.join(".claude.json"),
            r#"{"mcpServers":{"fs":{"command":"npx"}}}"#,
        )
        .unwrap();
        run_non_interactive(
            &SetupArgs {
                anthropic_key: Some("sk-ant-stored".to_string()),
                ..args()
            },
            &env,
        )
        .unwrap();

        let wizard = build_wizard(&env);

        assert_eq!(
            wizard.base.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-stored")
        );
        assert_eq!(wizard.mcp.len(), 1);
        assert_eq!(wizard.mcp[0].candidate.config.name, "fs");
    }

    #[test]
    fn a_missing_config_file_starts_from_defaults() {
        let dir = tempfile::tempdir().unwrap();

        let wizard = build_wizard(&env_in(dir.path()));

        assert_eq!(
            wizard.base.default_provider,
            Config::default().default_provider
        );
    }

    // ─── the verification background loop ───────────────────────────────────

    #[tokio::test]
    async fn the_verification_loop_answers_every_request_then_stops() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = build_wizard(&env_in(dir.path()));
        let (requests, replies) = wizard.take_verify_ends().expect("first take");
        wizard.providers[0].selected = true;
        wizard.providers[0].value = "sk-ant".to_string();
        wizard.request_verification(0);

        let handle = tokio::spawn(verification_loop(verify::SkipVerifier, requests, replies));
        // Dropping the wizard's sender ends the loop.
        let sender = wizard.verify_tx.clone();
        drop(sender);

        // Give the loop a turn, then confirm the answer arrived.
        for _ in 0..50 {
            wizard.drain_verifications();
            if !wizard.providers[0].checking {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(!wizard.providers[0].checking);
        assert_eq!(wizard.providers[0].outcome, verify::Outcome::Skipped);

        drop(wizard);
        handle.await.expect("the loop exits cleanly");
    }

    #[tokio::test]
    async fn the_verification_loop_stops_when_nobody_is_listening() {
        // The wizard exited mid-check; there is nothing left to report to.
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = mpsc::unbounded_channel::<VerifyReply>();
        request_tx
            .send(VerifyRequest {
                provider_id: "anthropic".to_string(),
                creds: leviath_runtime::provider_creds::ProviderCreds {
                    name: "anthropic".to_string(),
                    api_key: Some("sk-ant".to_string()),
                    base_url: None,
                    model_capabilities: std::collections::HashMap::new(),
                    request_timeout_secs: Some(1),
                    rate_limit: None,
                    options: std::collections::HashMap::new(),
                },
            })
            .unwrap();
        drop(reply_rx);

        verification_loop(verify::SkipVerifier, request_rx, reply_tx).await;
    }

    // ─── the wizard loop ────────────────────────────────────────────────────

    #[tokio::test]
    async fn quitting_returns_no_plan() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = build_wizard(&env_in(dir.path()));
        let mut terminal = test_terminal();
        let mut events = TestEventSource::new(vec![key(KeyCode::Char('q'))]);

        let plan = run_wizard_loop(
            &mut wizard,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await
        .unwrap();

        assert!(plan.is_none());
    }

    #[tokio::test]
    async fn saving_returns_the_plan_the_wizard_describes() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = build_wizard(&env_in(dir.path()));
        let mut terminal = test_terminal();
        // A tick with no input, then save - covering the poll-timeout path.
        let mut events = TestEventSource::new_with_nones(vec![
            None,
            Some(key_with(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        ]);

        let plan = run_wizard_loop(
            &mut wizard,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await
        .unwrap()
        .expect("a plan was produced");

        assert_eq!(plan.agents.len(), BUNDLED_AGENTS.len());
    }

    #[tokio::test]
    async fn non_press_and_non_key_events_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = build_wizard(&env_in(dir.path()));
        let mut terminal = test_terminal();
        let release = crossterm::event::Event::Key(crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::empty(),
            KeyEventKind::Release,
        ));
        let mut events = TestEventSource::new(vec![
            release,
            crossterm::event::Event::FocusGained,
            crossterm::event::Event::Resize(80, 24),
            key(KeyCode::Char('q')),
        ]);

        let plan = run_wizard_loop(
            &mut wizard,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await
        .unwrap();

        assert!(plan.is_none(), "only the real press quit");
    }

    /// A click reaches the wizard through the loop, against the size the
    /// terminal reports, and can finish the run the same way a key can.
    #[tokio::test]
    async fn a_click_is_routed_with_the_window_it_was_made_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = build_wizard(&env_in(dir.path()));
        wizard.enter(state::Step::Providers);
        let mut terminal = test_terminal();
        let size = terminal.size().expect("the test backend has a size");
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        // The row the click has to land on is asked for, not assumed, so the
        // test does not encode a layout.
        let row = (0..area.height)
            .find(|y| render::row_at(area, &wizard, 4, *y) == Some(1))
            .expect("the second provider is on screen");

        let mut events = TestEventSource::new(vec![
            crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 4,
                row,
                modifiers: KeyModifiers::empty(),
            }),
            key_with(KeyCode::Char('s'), KeyModifiers::CONTROL),
        ]);

        let plan = run_wizard_loop(
            &mut wizard,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await
        .unwrap()
        .expect("ctrl-s finished it");

        assert!(
            wizard.providers[1].selected,
            "the click selected what it landed on"
        );
        assert!(!plan.agents.is_empty());
    }

    /// The last button finishes the run, whether it is pressed or clicked.
    #[tokio::test]
    async fn clicking_apply_and_finish_ends_the_wizard() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = build_wizard(&env_in(dir.path()));
        wizard.enter(state::Step::Review);
        let mut terminal = test_terminal();
        let size = terminal.size().expect("the test backend has a size");
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        let button = wizard.nav_rows() - 1;
        let row = (0..area.height)
            .find(|y| render::row_at(area, &wizard, 4, *y) == Some(button))
            .expect("the button is on screen");

        let mut events = TestEventSource::new(vec![crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 4,
                row,
                modifiers: KeyModifiers::empty(),
            },
        )]);

        let plan = run_wizard_loop(
            &mut wizard,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await
        .unwrap();

        assert!(plan.is_some(), "the click applied the plan");
    }

    #[tokio::test]
    async fn a_draw_failure_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = build_wizard(&env_in(dir.path()));
        let mut terminal =
            ratatui::Terminal::new(crate::tui::TestBackendHarness::failing(80, 24)).unwrap();
        let mut events = TestEventSource::new(vec![]);

        let result = run_wizard_loop(
            &mut wizard,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn an_event_source_failure_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let mut wizard = build_wizard(&env_in(dir.path()));
        let mut terminal = test_terminal();
        let mut events = TestEventSource::failing();

        let result = run_wizard_loop(
            &mut wizard,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await;

        assert!(result.is_err());
    }

    // ─── the composed command ───────────────────────────────────────────────

    #[tokio::test]
    async fn saving_writes_the_config_and_installs_the_agents() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        let mut wizard = build_wizard(&env);
        let mut setup = TestSetup::new();
        let mut events =
            TestEventSource::new(vec![key_with(KeyCode::Char('s'), KeyModifiers::CONTROL)]);

        execute_core(&mut wizard, &env, &mut setup, &mut events)
            .await
            .unwrap();

        assert!(env.config_path.exists());
        assert!(env.agents_dir.join(BUNDLED_AGENTS[0].name).exists());
    }

    #[tokio::test]
    async fn quitting_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        let mut wizard = build_wizard(&env);
        let mut setup = TestSetup::new();
        let mut events = TestEventSource::new(vec![key(KeyCode::Char('q'))]);

        execute_core(&mut wizard, &env, &mut setup, &mut events)
            .await
            .unwrap();

        assert!(
            !env.config_path.exists(),
            "nothing should have been written"
        );
        assert!(!env.agents_dir.exists());
    }

    #[tokio::test]
    async fn a_terminal_that_will_not_start_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        let mut wizard = build_wizard(&env);
        let mut events = TestEventSource::new(vec![]);

        let mut enable_fails = TestSetup {
            enable_should_fail: true,
            create_should_fail: false,
            draw_should_fail: false,
            ..TestSetup::new()
        };
        assert!(
            execute_core(&mut wizard, &env, &mut enable_fails, &mut events)
                .await
                .is_err()
        );

        let mut create_fails = TestSetup {
            enable_should_fail: false,
            create_should_fail: true,
            draw_should_fail: false,
            ..TestSetup::new()
        };
        assert!(
            execute_core(&mut wizard, &env, &mut create_fails, &mut events)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_loop_failure_is_surfaced_after_the_terminal_is_restored() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        let mut wizard = build_wizard(&env);
        let mut setup = TestSetup::new();
        let mut events = TestEventSource::failing();

        let result = execute_core(&mut wizard, &env, &mut setup, &mut events).await;

        assert!(result.is_err());
        assert!(!env.config_path.exists());
    }

    #[tokio::test]
    async fn a_write_failure_after_the_wizard_is_surfaced() {
        // The terminal must already be restored, or the error would be printed
        // onto an alternate screen the user never sees again.
        let dir = tempfile::tempdir().unwrap();
        let mut env = env_in(dir.path());
        let blocked = dir.path().join("not-a-dir");
        std::fs::write(&blocked, "").unwrap();
        let mut wizard = build_wizard(&env);
        env.config_path = blocked.join("config.toml");
        let mut setup = TestSetup::new();
        let mut events =
            TestEventSource::new(vec![key_with(KeyCode::Char('s'), KeyModifiers::CONTROL)]);

        let result = execute_core(&mut wizard, &env, &mut setup, &mut events).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_with_routes_to_the_flags_path() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        let mut setup = TestSetup::new();
        let mut events = TestEventSource::new(vec![]);

        execute_with(
            &SetupArgs {
                non_interactive: true,
                anthropic_key: Some("sk-ant-x".to_string()),
                ..args()
            },
            &env,
            &mut setup,
            &mut events,
            false,
        )
        .await
        .unwrap();

        let written = Config::load_from_path_public(&env.config_path).unwrap();
        assert_eq!(
            written.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-x")
        );
    }

    #[tokio::test]
    async fn without_a_terminal_the_wizard_refuses_and_says_what_to_run_instead() {
        // Starting ratatui on a pipe would take over a terminal that isn't
        // there.
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        let mut setup = TestSetup::new();
        let mut events = TestEventSource::new(vec![]);

        let error = execute_with(&args(), &env, &mut setup, &mut events, false)
            .await
            .expect_err("a pipe is not a terminal");

        let message = error.to_string();
        assert!(message.contains("needs a terminal"), "{message}");
        assert!(message.contains("--non-interactive"), "{message}");
        assert!(!env.config_path.exists());
    }

    #[tokio::test]
    async fn with_a_terminal_execute_with_runs_the_wizard() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        let mut setup = TestSetup::new();
        let mut events =
            TestEventSource::new(vec![key_with(KeyCode::Char('s'), KeyModifiers::CONTROL)]);

        execute_with(&args(), &env, &mut setup, &mut events, true)
            .await
            .unwrap();

        assert!(env.config_path.exists());
    }

    // ─── reporting ──────────────────────────────────────────────────────────

    #[test]
    fn the_summary_covers_agents_warnings_and_the_empty_case() {
        report(&plan::Applied {
            config_path: PathBuf::from("/tmp/config.toml"),
            agents_installed: vec!["coder".to_string()],
            warnings: vec!["could not install x".to_string()],
        });
        report(&plan::Applied {
            config_path: PathBuf::from("/tmp/config.toml"),
            agents_installed: Vec::new(),
            warnings: Vec::new(),
        });
    }

    #[test]
    fn the_next_step_names_an_installed_agent_when_there_is_one() {
        print_next_steps(&plan::Applied {
            config_path: PathBuf::from("/tmp/config.toml"),
            agents_installed: vec!["coder".to_string()],
            warnings: Vec::new(),
        });
        print_next_steps(&plan::Applied {
            config_path: PathBuf::from("/tmp/config.toml"),
            agents_installed: Vec::new(),
            warnings: Vec::new(),
        });
    }

    #[test]
    fn the_real_agents_directory_sits_under_the_leviath_home() {
        assert_eq!(
            real_agents_dir(Some(Path::new("/home/u"))),
            PathBuf::from("/home/u/.leviath/agents")
        );
        // No home directory resolvable: a relative path, not a panic.
        assert_eq!(real_agents_dir(None), PathBuf::from(".leviath/agents"));
    }
}
