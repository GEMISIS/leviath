//! `lev providers` - see the configured providers and set their priority.
//!
//! The priority is `[providers] provider_order`: the order a bare model name -
//! one a blueprint lists with no provider - prefers when more than one
//! configured provider serves it. Setting it here writes the same key `lev
//! setup` and `PUT /api/config` write, so the three agree on one file.
//!
//! Naming a provider in the order is also how a subscription transport (Codex,
//! Claude Code) becomes eligible for a bare name: it is otherwise reachable
//! only by an explicit `provider/model`, so that turning it on never silently
//! moves billing. Listing it here is the deliberate choice that opts it in.

pub(crate) mod quota;

use clap::{Args, Subcommand};

use crate::commands::setup::catalog;
use crate::config::Config;

/// Arguments for `lev providers`.
#[derive(Args)]
pub struct ProvidersArgs {
    /// Which subcommand to run. Omitted, it lists.
    #[command(subcommand)]
    command: Option<ProvidersCommand>,
}

impl ProvidersArgs {
    /// A bare (list) invocation, for routing tests in `dispatch`.
    #[cfg(test)]
    pub(crate) fn list_for_test() -> Self {
        Self { command: None }
    }
}

#[derive(Subcommand)]
enum ProvidersCommand {
    /// Show configured providers and the current priority order
    List(ListArgs),
    /// Set the priority order for a bare model name, best first
    Order(OrderArgs),
    /// What each provider keeps of a request, and ask for zero retention
    Retention(RetentionArgs),
    /// How much of each signed-in subscription (Codex, Grok) is left
    Quota(ListArgs),
}

#[derive(Args)]
struct RetentionArgs {
    /// Omitted, it shows what every provider keeps and how that is controlled
    #[command(subcommand)]
    command: Option<RetentionCommand>,
    /// Emit JSON instead of a table
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum RetentionCommand {
    /// `zero` asks every provider for zero retention and refuses a model that
    /// cannot give it; `off` stops asking
    Set(RetentionSetArgs),
    /// Set the Bedrock account's data retention mode directly (`none`,
    /// `default`, `aws_review`, `inherit`)
    Bedrock(BedrockModeArgs),
}

#[derive(Args)]
struct RetentionSetArgs {
    /// `zero` or `off`
    want: String,
}

#[derive(Args)]
struct BedrockModeArgs {
    /// The mode: `none` is zero retention, `aws_review` is what Claude Fable 5
    /// and Mythos 5 need
    mode: String,
}

#[derive(Args)]
struct ListArgs {
    /// Emit JSON instead of a table
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct OrderArgs {
    /// Provider names in priority order, best first (e.g. `codex openrouter
    /// openai`). A provider left out keeps its old priority; a subscription
    /// left out stays excluded from a bare model name.
    names: Vec<String>,
    /// Clear the order, so `default_provider` alone decides again.
    #[arg(long, conflicts_with = "names")]
    clear: bool,
}

/// Seams the real I/O of `lev providers` depends on, injected so the command
/// logic is unit-testable without touching the real config file.
pub struct ProvidersEnv {
    /// Path to the config file to read and rewrite.
    pub config_path: std::path::PathBuf,
    /// Where Bedrock's control plane answers, when a test stands one up in
    /// place of AWS's. `None` derives it from the configured region.
    pub bedrock_control_url: Option<String>,
    /// Where Bedrock's `bedrock-mantle` host answers (the per-model
    /// retention listing), when a test stands one up. `None` derives it
    /// from the configured region.
    pub bedrock_mantle_url: Option<String>,
}

/// Run a `lev providers` subcommand against the injected environment.
pub async fn execute_with(args: ProvidersArgs, env: &ProvidersEnv) -> anyhow::Result<()> {
    match args.command {
        None | Some(ProvidersCommand::List(ListArgs { json: false })) => list(false, env),
        Some(ProvidersCommand::List(ListArgs { json: true })) => list(true, env),
        Some(ProvidersCommand::Order(order)) => set_order(order, env),
        Some(ProvidersCommand::Quota(ListArgs { json })) => {
            quota::show(json, &env.config_path).await
        }
        Some(ProvidersCommand::Retention(RetentionArgs {
            command: None,
            json,
        })) => show_retention(json, env, &leviath_providers::provider::build_http_client).await,
        Some(ProvidersCommand::Retention(RetentionArgs {
            command: Some(RetentionCommand::Set(set)),
            ..
        })) => {
            set_retention(
                &set.want,
                env,
                &leviath_providers::provider::build_http_client,
            )
            .await
        }
        Some(ProvidersCommand::Retention(RetentionArgs {
            command: Some(RetentionCommand::Bedrock(args)),
            ..
        })) => {
            set_bedrock_mode(
                &args.mode,
                env,
                &leviath_providers::provider::build_http_client,
            )
            .await
        }
    }
}

/// The Bedrock provider the config describes, for reading and writing the
/// account's data retention mode; `None` when no Bedrock key is configured.
fn bedrock_from(
    config: &Config,
    env: &ProvidersEnv,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> anyhow::Result<Option<leviath_providers::BedrockProvider>> {
    let Some(key) = config
        .providers
        .bedrock_api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    else {
        return Ok(None);
    };
    let client = build_client(None)?;
    let mut provider = leviath_providers::BedrockProvider::new(client, key.to_string())
        .with_region(config.providers.bedrock_region.clone())
        .with_base_url(config.providers.bedrock_base_url.clone());
    if let Some(url) = &env.bedrock_control_url {
        provider = provider.with_control_url(Some(url.clone()));
    }
    if let Some(url) = &env.bedrock_mantle_url {
        provider = provider.with_mantle_url(Some(url.clone()));
    }
    Ok(Some(provider))
}

/// Every provider worth a row: the built-ins that are configured, and every
/// `[model_providers]` entry.
fn retention_rows(config: &Config) -> Vec<String> {
    let configured = catalog::configured(config);
    let mut names: Vec<String> = catalog::providers()
        .iter()
        .filter(|p| configured.contains(&p.id))
        .map(|p| p.id.to_string())
        .collect();
    let mut custom: Vec<String> = config.model_providers.keys().cloned().collect();
    custom.sort();
    names.extend(custom);
    names
}

/// `lev providers retention`: what each provider keeps, with the account's
/// mode read live from Bedrock when a key is configured.
async fn show_retention(
    json: bool,
    env: &ProvidersEnv,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> anyhow::Result<()> {
    let config = Config::load_from_path_public(&env.config_path)?;
    let settings = crate::commands::run::session::retention_settings(&config);
    let bedrock = bedrock_from(&config, env, build_client)?;
    let bedrock_mode = match &bedrock {
        Some(provider) => match provider.account_retention().await {
            Ok(Some(read)) => Some(Ok(read.mode)),
            Ok(None) => None,
            Err(e) => Some(Err(e.to_string())),
        },
        None => None,
    };
    // What Bedrock's listing says per model, so a model it never serves
    // under mode none, or one this account cannot call as things stand, is
    // named here rather than at spawn. Best effort: a listing that cannot
    // be read leaves the account's mode to speak alone.
    let bedrock_models = match &bedrock {
        Some(provider) => {
            if let Err(e) = provider.read_model_retention().await {
                tracing::debug!(error = %e, "Bedrock's per-model data retention could not be read");
            }
            provider.model_retentions()
        }
        None => Vec::new(),
    };
    let never_none: Vec<&str> = bedrock_models
        .iter()
        .filter(|(_, r)| !r.allows("none"))
        .map(|(id, _)| id.as_str())
        .collect();
    let unavailable: Vec<String> = bedrock_models
        .iter()
        .filter(|(_, r)| r.unavailable())
        .map(|(id, r)| {
            format!(
                "{id} ({})",
                r.status_reason.as_deref().unwrap_or("no reason given")
            )
        })
        .collect();
    let rows: Vec<(String, leviath_providers::retention::RetentionPolicy)> =
        retention_rows(&config)
            .into_iter()
            .map(|name| {
                let base = leviath_providers::retention::builtin(&name, "");
                let policy = leviath_providers::retention::resolve(base, &name, "", &settings);
                (name, policy)
            })
            .collect();

    if json {
        let out = serde_json::json!({
            "zero_retention": config.providers.zero_retention,
            "zero_retention_agreements": config.providers.zero_retention_agreements,
            "bedrock_account_mode": match &bedrock_mode {
                Some(Ok(mode)) => serde_json::json!(mode),
                Some(Err(e)) => serde_json::json!({ "error": e }),
                None => serde_json::Value::Null,
            },
            "bedrock_models": bedrock_models.iter().map(|(id, r)| serde_json::json!({
                "id": id,
                "allowed_modes": r.allowed_modes,
                "mode": r.mode,
                "status": r.status,
                "status_reason": r.status_reason,
            })).collect::<Vec<_>>(),
            "providers": rows.iter().map(|(name, policy)| serde_json::json!({
                "id": name,
                "retention": policy.retention.as_word(),
                "control": policy.control,
                "source": policy.source,
                "note": policy.note,
            })).collect::<Vec<_>>(),
        });
        println!("{out:#}");
        return Ok(());
    }

    println!("What each provider keeps of a request once the reply is back:");
    for (name, policy) in &rows {
        println!("  {:<12} {}", name, policy.summary());
        println!("               {}", policy.note);
        if name == "bedrock" {
            match &bedrock_mode {
                Some(Ok(mode)) => {
                    println!("               account data retention mode: {mode} (read just now)")
                }
                Some(Err(e)) => {
                    println!("               account data retention mode: could not be read ({e})")
                }
                None => {}
            }
            if !never_none.is_empty() {
                println!(
                    "               never served under mode none, so never with zero retention: {}",
                    never_none.join(", ")
                );
            }
            for row in &unavailable {
                println!("               unavailable to this account as things stand: {row}");
            }
        }
    }
    println!();
    match config.providers.zero_retention {
        true => println!(
            "[providers] zero_retention = true: a stage whose model keeps anything is \
             refused at spawn; `lev providers retention set off` stops asking."
        ),
        false => println!(
            "[providers] zero_retention = false: runs use every configured model; \
             `lev providers retention set zero` asks for zero retention everywhere."
        ),
    }
    if !config.providers.zero_retention_agreements.is_empty() {
        println!(
            "Declared zero data retention agreements: {}",
            config.providers.zero_retention_agreements.join(", ")
        );
    }
    println!("Per model: `lev models show <model>` prints the model's own answer.");
    Ok(())
}

/// `lev providers retention set zero|off`: the config switch, and on
/// Bedrock the account mode that goes with it.
async fn set_retention(
    want: &str,
    env: &ProvidersEnv,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> anyhow::Result<()> {
    let zero = match want.trim().to_ascii_lowercase().as_str() {
        "zero" | "on" | "none" => true,
        "off" | "default" => false,
        other => anyhow::bail!(
            "'{other}' is not a setting: `zero` asks every provider for zero retention, `off` stops asking"
        ),
    };
    let mut config = Config::load_from_path_public(&env.config_path)?;
    config.providers.zero_retention = zero;
    config.save_to_path_public(&env.config_path)?;
    match zero {
        true => println!(
            "Zero retention is on: OpenAI is sent store=false, OpenRouter routes only to \
             zero-retention endpoints, and a stage whose model keeps anything is refused \
             at spawn."
        ),
        false => println!("Zero retention is off: runs use every configured model."),
    }
    match bedrock_from(&config, env, build_client)? {
        // The mode printed is the one asked for: Bedrock echoes it back on
        // success, so the two are equal, and the request is the fact worth
        // saying rather than a value read back off the account.
        Some(provider) if zero => match provider.set_account_retention("none").await {
            Ok(_) => println!(
                "Bedrock account data retention mode set to none (Claude Fable 5 and Mythos 5 \
                 need aws_review and are unavailable under it)."
            ),
            Err(e) => println!(
                "Bedrock's account data retention mode could not be set to none: {e}. Set it \
                 with `lev providers retention bedrock none` once the key can."
            ),
        },
        Some(_) => println!(
            "Bedrock's account data retention mode is left as it is; `lev providers \
             retention bedrock <mode>` changes it."
        ),
        None => {}
    }
    Ok(())
}

/// `lev providers retention bedrock <mode>`: the account setting, directly.
async fn set_bedrock_mode(
    mode: &str,
    env: &ProvidersEnv,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> anyhow::Result<()> {
    let config = Config::load_from_path_public(&env.config_path)?;
    let Some(provider) = bedrock_from(&config, env, build_client)? else {
        anyhow::bail!("no Bedrock key is configured; `lev setup --bedrock-key ...` first");
    };
    let mode = mode.trim();
    provider.set_account_retention(mode).await?;
    println!("Bedrock account data retention mode: {mode}");
    Ok(())
}

/// Whether `name` is a provider this machine could route to: a built-in the
/// wizard knows, or a `[model_providers.<name>]` entry in the config.
///
/// The order tolerates a name nothing serves - it simply never wins - but the
/// setter refuses one so a typo does not persist as a silent no-op. `lev
/// doctor` reports the same class after the fact; this catches it at the edit.
fn is_known(config: &Config, name: &str) -> bool {
    catalog::providers().iter().any(|p| p.id == name) || config.model_providers.contains_key(name)
}

/// Every provider name this config could route to, for an error that lists the
/// alternatives rather than only rejecting the typo.
fn known_names(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = catalog::providers()
        .iter()
        .map(|p| p.id.to_string())
        .collect();
    names.extend(config.model_providers.keys().cloned());
    names.sort_unstable();
    names.dedup();
    names
}

fn set_order(order: OrderArgs, env: &ProvidersEnv) -> anyhow::Result<()> {
    let mut config = Config::load_from_path_public(&env.config_path)?;

    let new_order = if order.clear {
        Vec::new()
    } else {
        if order.names.is_empty() {
            anyhow::bail!(
                "name at least one provider (best first), or pass --clear to remove the order.\n\
                 Known providers: {}",
                known_names(&config).join(", ")
            );
        }
        for name in &order.names {
            if !is_known(&config, name) {
                anyhow::bail!(
                    "'{name}' is not a configured provider, so it would never win a route.\n\
                     Known providers: {}",
                    known_names(&config).join(", ")
                );
            }
        }
        // A name given twice is a mistake, not a stronger preference: the first
        // position is the one that counts, so keep it and drop the rest.
        let mut seen = std::collections::HashSet::new();
        order
            .names
            .iter()
            .filter(|n| seen.insert((*n).clone()))
            .cloned()
            .collect()
    };

    config.providers.provider_order = new_order.clone();
    config.save_to_path_public(&env.config_path)?;

    if new_order.is_empty() {
        println!(
            "Cleared the provider priority; default_provider ({}) decides a bare model name now.",
            config.default_provider
        );
    } else {
        println!("Provider priority set: {}", new_order.join(" > "));
    }
    Ok(())
}

fn list(json: bool, env: &ProvidersEnv) -> anyhow::Result<()> {
    let config = Config::load_from_path_public(&env.config_path)?;
    let order = &config.providers.provider_order;
    let configured = catalog::configured(&config);

    if json {
        let rows: Vec<serde_json::Value> = catalog::providers()
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "display": p.display,
                    "configured": configured.contains(&p.id),
                    "priority": order.iter().position(|n| n == p.id),
                })
            })
            .collect();
        let out = serde_json::json!({
            "provider_order": order,
            "default_provider": config.default_provider,
            "providers": rows,
        });
        // `{:#}` is `serde_json::Value`'s own pretty Display - infallible,
        // unlike `to_string_pretty`, whose error arm nothing could reach.
        println!("{out:#}");
        return Ok(());
    }

    println!("Provider priority for a bare model name (best first):");
    if order.is_empty() {
        println!(
            "  (none set - default_provider '{}' decides; `lev providers order <name>...` to set one)",
            config.default_provider
        );
    } else {
        for (i, name) in order.iter().enumerate() {
            let mark = if configured.contains(&name.as_str()) {
                ""
            } else {
                "  (not configured - never wins)"
            };
            println!("  {}. {name}{mark}", i + 1);
        }
    }

    println!("\nConfigured providers:");
    let mut any = false;
    for p in catalog::providers() {
        if configured.contains(&p.id) {
            any = true;
            println!("  {:<12} {}", p.id, p.display);
        }
    }
    if !any {
        println!("  (none - run `lev setup`)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(config: Config) -> (tempfile::TempDir, ProvidersEnv) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        config.save_to_path_public(&config_path).expect("save");
        (
            dir,
            ProvidersEnv {
                config_path,
                bedrock_control_url: None,
                bedrock_mantle_url: None,
            },
        )
    }

    fn retention_args(command: Option<RetentionCommand>, json: bool) -> ProvidersArgs {
        ProvidersArgs {
            command: Some(ProvidersCommand::Retention(RetentionArgs { command, json })),
        }
    }

    /// The listing describes every configured built-in and every custom
    /// entry, the switch is written and read back, and a word that is not a
    /// setting is refused.
    #[tokio::test]
    async fn retention_is_shown_and_the_switch_is_written() {
        let mut config = Config::default();
        config.providers.openai_api_key = Some("sk-test".to_string());
        config.providers.zero_retention_agreements = vec!["openai".to_string()];
        config.model_providers.insert(
            "cerebras".to_string(),
            toml::from_str(
                r#"api_key = "k"
retention = "zero""#,
            )
            .unwrap(),
        );
        let (_dir, env) = env_with(config);

        execute_with(retention_args(None, false), &env)
            .await
            .unwrap();
        execute_with(retention_args(None, true), &env)
            .await
            .unwrap();

        execute_with(
            retention_args(
                Some(RetentionCommand::Set(RetentionSetArgs {
                    want: "zero".into(),
                })),
                false,
            ),
            &env,
        )
        .await
        .unwrap();
        assert!(load(&env).providers.zero_retention);
        execute_with(retention_args(None, false), &env)
            .await
            .unwrap();
        execute_with(
            retention_args(
                Some(RetentionCommand::Set(RetentionSetArgs {
                    want: "off".into(),
                })),
                false,
            ),
            &env,
        )
        .await
        .unwrap();
        assert!(!load(&env).providers.zero_retention);

        let err = execute_with(
            retention_args(
                Some(RetentionCommand::Set(RetentionSetArgs {
                    want: "maybe".into(),
                })),
                false,
            ),
            &env,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not a setting"), "{err}");

        // No Bedrock key: the direct mode command says so.
        let err = execute_with(
            retention_args(
                Some(RetentionCommand::Bedrock(BedrockModeArgs {
                    mode: "none".into(),
                })),
                false,
            ),
            &env,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no Bedrock key"), "{err}");
    }

    /// With a Bedrock key the listing reads the account's mode, `set zero`
    /// writes mode none to the account, and the direct command writes what
    /// it is told; a plane that refuses is reported, not fatal to `set`.
    #[tokio::test]
    async fn retention_reads_and_writes_the_bedrock_account_mode() {
        let mut config = Config::default();
        config.providers.bedrock_api_key = Some("ABSK-test".to_string());
        let (_dir, mut env) = env_with(config);

        // The listing names a model never served under mode none and one
        // unavailable as things stand; both are printed, as JSON and as text.
        let listing = || {
            br#"{"data":[
                {"id":"openai.gpt-5.4","status":"available","data_retention":{"allowed_modes":["default","aws_review"],"mode":"default"}},
                {"id":"anthropic.claude-fable-5","status":"unavailable","status_reason":"This model is not available under data retention mode 'default'.","data_retention":{"allowed_modes":["aws_review"],"mode":"default"}},
                {"id":"anthropic.claude-sonnet-5","status":"available","data_retention":{"allowed_modes":["none","default"],"mode":"default"}}
            ]}"#.to_vec()
        };
        let url =
            leviath_testkit::spawn_mock_server(200, "OK", br#"{"mode":"default"}"#.to_vec()).await;
        env.bedrock_control_url = Some(url);
        env.bedrock_mantle_url =
            Some(leviath_testkit::spawn_mock_server(200, "OK", listing()).await);
        execute_with(retention_args(None, true), &env)
            .await
            .unwrap();
        let url =
            leviath_testkit::spawn_mock_server(200, "OK", br#"{"mode":"default"}"#.to_vec()).await;
        env.bedrock_control_url = Some(url);
        env.bedrock_mantle_url =
            Some(leviath_testkit::spawn_mock_server(200, "OK", listing()).await);
        execute_with(retention_args(None, false), &env)
            .await
            .unwrap();
        // A listing that cannot be read leaves the account's mode to speak alone.
        env.bedrock_mantle_url = Some("http://127.0.0.1:1".to_string());

        let url =
            leviath_testkit::spawn_mock_server(200, "OK", br#"{"mode":"none"}"#.to_vec()).await;
        env.bedrock_control_url = Some(url);
        execute_with(
            retention_args(
                Some(RetentionCommand::Set(RetentionSetArgs {
                    want: "zero".into(),
                })),
                false,
            ),
            &env,
        )
        .await
        .unwrap();
        assert!(load(&env).providers.zero_retention);

        let url =
            leviath_testkit::spawn_mock_server(200, "OK", br#"{"mode":"aws_review"}"#.to_vec())
                .await;
        env.bedrock_control_url = Some(url);
        execute_with(
            retention_args(
                Some(RetentionCommand::Bedrock(BedrockModeArgs {
                    mode: "aws_review".into(),
                })),
                false,
            ),
            &env,
        )
        .await
        .unwrap();

        // `set off` leaves the account alone; a refusal on `set zero` is printed.
        execute_with(
            retention_args(
                Some(RetentionCommand::Set(RetentionSetArgs {
                    want: "off".into(),
                })),
                false,
            ),
            &env,
        )
        .await
        .unwrap();
        let url = leviath_testkit::spawn_mock_server(403, "Forbidden", b"{}".to_vec()).await;
        env.bedrock_control_url = Some(url.clone());
        execute_with(
            retention_args(
                Some(RetentionCommand::Set(RetentionSetArgs {
                    want: "zero".into(),
                })),
                false,
            ),
            &env,
        )
        .await
        .unwrap();
        // The listing reports a plane that will not answer. Each mock answers
        // one request, so every call gets its own.
        let url = leviath_testkit::spawn_mock_server(403, "Forbidden", b"{}".to_vec()).await;
        env.bedrock_control_url = Some(url);
        execute_with(retention_args(None, false), &env)
            .await
            .unwrap();
        let url = leviath_testkit::spawn_mock_server(403, "Forbidden", b"{}".to_vec()).await;
        env.bedrock_control_url = Some(url);
        let err = execute_with(
            retention_args(
                Some(RetentionCommand::Bedrock(BedrockModeArgs {
                    mode: "none".into(),
                })),
                false,
            ),
            &env,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");

        // The text listing prints a mode it read, and the JSON one a refusal.
        let url =
            leviath_testkit::spawn_mock_server(200, "OK", br#"{"mode":"none"}"#.to_vec()).await;
        env.bedrock_control_url = Some(url);
        execute_with(retention_args(None, false), &env)
            .await
            .unwrap();
        let url = leviath_testkit::spawn_mock_server(403, "Forbidden", b"{}".to_vec()).await;
        env.bedrock_control_url = Some(url);
        execute_with(retention_args(None, true), &env)
            .await
            .unwrap();
    }

    /// A gateway in front of Bedrock's control plane cannot be asked, so the
    /// listing has no mode to show and `set zero` cannot write one; a client
    /// that cannot be built, or a config that cannot be read, stops each
    /// command with its reason.
    #[tokio::test]
    async fn retention_commands_report_what_stops_them() {
        let mut config = Config::default();
        config.providers.bedrock_api_key = Some("ABSK-test".to_string());
        config.providers.bedrock_base_url = Some("http://gw.local".to_string());
        let (_dir, env) = env_with(config);
        execute_with(retention_args(None, false), &env)
            .await
            .unwrap();
        execute_with(retention_args(None, true), &env)
            .await
            .unwrap();
        execute_with(
            retention_args(
                Some(RetentionCommand::Set(RetentionSetArgs {
                    want: "zero".into(),
                })),
                false,
            ),
            &env,
        )
        .await
        .unwrap();
        // Without an override the control plane follows the region.
        let config = load(&env);
        let provider = bedrock_from(
            &config,
            &env,
            &leviath_providers::provider::build_http_client,
        )
        .unwrap()
        .expect("a key is configured");
        assert_eq!(provider.region(), "us-east-1");

        let failing: leviath_providers::provider::HttpClientFactory<'_> =
            &|_t| Err(leviath_providers::provider::malformed_url_error());
        assert!(bedrock_from(&config, &env, failing).is_err());
        assert!(show_retention(false, &env, failing).await.is_err());
        assert!(set_retention("zero", &env, failing).await.is_err());
        assert!(set_bedrock_mode("none", &env, failing).await.is_err());

        // A config that cannot be read stops every command the same way.
        let dir = tempfile::tempdir().expect("tempdir");
        let broken = dir.path().join("config.toml");
        std::fs::write(&broken, "this = = is not toml").expect("write config");
        let env = ProvidersEnv {
            config_path: broken,
            bedrock_control_url: None,
            bedrock_mantle_url: None,
        };
        let real = &leviath_providers::provider::build_http_client;
        assert!(show_retention(false, &env, real).await.is_err());
        assert!(set_retention("zero", &env, real).await.is_err());
        assert!(set_bedrock_mode("none", &env, real).await.is_err());

        // And one that loads (as the default) but cannot be written back.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a directory").expect("write blocker");
        let env = ProvidersEnv {
            config_path: blocker.join("config.toml"),
            bedrock_control_url: None,
            bedrock_mantle_url: None,
        };
        let err = set_retention("zero", &env, real).await.unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    fn order_args(names: &[&str], clear: bool) -> ProvidersArgs {
        ProvidersArgs {
            command: Some(ProvidersCommand::Order(OrderArgs {
                names: names.iter().map(|s| s.to_string()).collect(),
                clear,
            })),
        }
    }

    fn load(env: &ProvidersEnv) -> Config {
        Config::load_from_path_public(&env.config_path).expect("load")
    }

    #[tokio::test]
    async fn order_sets_the_priority_and_persists_it() {
        let (_d, env) = env_with(Config::default());
        execute_with(order_args(&["codex", "openrouter", "openai"], false), &env)
            .await
            .expect("ok");
        assert_eq!(
            load(&env).providers.provider_order,
            ["codex", "openrouter", "openai"]
        );
    }

    #[tokio::test]
    async fn order_clear_empties_it() {
        let mut config = Config::default();
        config.providers.provider_order = vec!["codex".to_string()];
        let (_d, env) = env_with(config);
        execute_with(order_args(&[], true), &env).await.expect("ok");
        assert!(load(&env).providers.provider_order.is_empty());
    }

    #[tokio::test]
    async fn order_refuses_an_unknown_provider() {
        let (_d, env) = env_with(Config::default());
        let err = execute_with(order_args(&["codex", "opennrouter"], false), &env)
            .await
            .expect_err("unknown name");
        assert!(err.to_string().contains("opennrouter"), "{err}");
        // Nothing was written.
        assert!(load(&env).providers.provider_order.is_empty());
    }

    #[tokio::test]
    async fn order_accepts_a_configured_script_provider() {
        let mut config = Config::default();
        config.model_providers.insert(
            "my-gateway".to_string(),
            crate::config::ModelProviderConfig::default(),
        );
        let (_d, env) = env_with(config);
        execute_with(order_args(&["my-gateway", "anthropic"], false), &env)
            .await
            .expect("a configured gateway is known");
        assert_eq!(
            load(&env).providers.provider_order,
            ["my-gateway", "anthropic"]
        );
    }

    #[tokio::test]
    async fn order_deduplicates_keeping_first_position() {
        let (_d, env) = env_with(Config::default());
        execute_with(order_args(&["openai", "codex", "openai"], false), &env)
            .await
            .expect("ok");
        assert_eq!(load(&env).providers.provider_order, ["openai", "codex"]);
    }

    #[tokio::test]
    async fn order_with_neither_names_nor_clear_errors_and_writes_nothing() {
        let (_d, env) = env_with(Config::default());
        let err = execute_with(order_args(&[], false), &env)
            .await
            .expect_err("nothing to do");
        assert!(err.to_string().contains("name at least one"), "{err}");
        assert!(load(&env).providers.provider_order.is_empty());
    }

    #[tokio::test]
    async fn list_runs_in_both_shapes() {
        let mut config = Config::default();
        config.providers.provider_order = vec!["codex".to_string()];
        let (_d, env) = env_with(config);
        // Both the default (None) and explicit list, table and json, just have
        // to run without erroring - they print, and the assertions above cover
        // the state they read.
        execute_with(ProvidersArgs::list_for_test(), &env)
            .await
            .expect("bare list");
        execute_with(
            ProvidersArgs {
                command: Some(ProvidersCommand::List(ListArgs { json: true })),
            },
            &env,
        )
        .await
        .expect("json list");
    }

    /// A config file that will not parse fails both the setter and the lister
    /// at load, rather than one of them writing over a file it could not read.
    #[tokio::test]
    async fn a_broken_config_errors_at_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "this is : not : toml\n").expect("write garbage");
        let env = ProvidersEnv {
            config_path,
            bedrock_control_url: None,
            bedrock_mantle_url: None,
        };
        assert!(
            execute_with(order_args(&["anthropic"], false), &env)
                .await
                .is_err()
        );
        assert!(
            execute_with(ProvidersArgs::list_for_test(), &env)
                .await
                .is_err()
        );
    }

    /// A save that cannot write is surfaced, not swallowed: with the config
    /// directory blocked by a regular file, the load returns defaults (the
    /// file does not exist under it) and the save fails.
    #[tokio::test]
    async fn a_save_that_cannot_write_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a directory").expect("write blocker");
        let env = ProvidersEnv {
            config_path: blocker.join("config.toml"),
            bedrock_control_url: None,
            bedrock_mantle_url: None,
        };
        let err = execute_with(order_args(&["anthropic"], false), &env)
            .await
            .expect_err("the save cannot create its directory");
        // A real save error, not a validation or load one.
        assert!(
            !err.to_string().contains("not a configured provider"),
            "{err}"
        );
    }

    /// The table's configured-provider paths: an order naming a provider that
    /// is configured (no "not configured" note) and the configured-providers
    /// section listing it. Distinct from the empty-config test, which drives
    /// the "none" arms.
    #[tokio::test]
    async fn list_shows_a_configured_provider_in_the_order_and_the_roster() {
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("sk-test".to_string());
        config.providers.provider_order = vec!["anthropic".to_string()];
        let (_d, env) = env_with(config);
        execute_with(ProvidersArgs::list_for_test(), &env)
            .await
            .expect("table list");
    }

    #[tokio::test]
    async fn list_says_when_nothing_is_configured_and_no_order_is_set() {
        let (_d, env) = env_with(Config::default());
        // Loading a config overlays every provider key from the process
        // environment, and a neighbouring test setting one through `temp_env`
        // sets it for the whole process while this one runs. Without clearing
        // them here, whether this config reads as having no provider at all
        // depends on which test happens to be running beside it.
        let vars = crate::config::config_isolation_vars(env.config_path.parent().expect("a dir"));
        temp_env::async_with_vars(vars, async {
            execute_with(ProvidersArgs::list_for_test(), &env)
                .await
                .expect("bare list on an empty config");
            execute_with(
                ProvidersArgs {
                    command: Some(ProvidersCommand::List(ListArgs { json: true })),
                },
                &env,
            )
            .await
            .expect("json on an empty config");
        })
        .await;
    }

    #[tokio::test]
    async fn the_quota_subcommand_reads_the_config() {
        let (_dir, env) = env_with(Config::default());
        execute_with(
            ProvidersArgs {
                command: Some(ProvidersCommand::Quota(ListArgs { json: true })),
            },
            &env,
        )
        .await
        .expect("nothing signed in is not an error");
    }
}
