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
}

/// Run a `lev providers` subcommand against the injected environment.
pub async fn execute_with(args: ProvidersArgs, env: &ProvidersEnv) -> anyhow::Result<()> {
    match args.command {
        None | Some(ProvidersCommand::List(ListArgs { json: false })) => list(false, env),
        Some(ProvidersCommand::List(ListArgs { json: true })) => list(true, env),
        Some(ProvidersCommand::Order(order)) => set_order(order, env),
    }
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
        (dir, ProvidersEnv { config_path })
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
        let env = ProvidersEnv { config_path };
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
    }
}
