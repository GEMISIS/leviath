//! `lev mcp` - manage MCP tool servers and their authentication, or serve
//! Leviath itself as one.
//!
//! Adding a server that requires OAuth starts the browser login automatically,
//! the way Claude Code and other clients do, so setup is a single command.
//!
//! `lev mcp serve` turns the direction around: it is the stdio MCP server a
//! host agent (Claude Code, Grok Build, Codex, Gemini, Hermes) launches to hand
//! work to Leviath. It lives in [`serve`] and is routed in the binary through
//! [`McpArgs::route`], because it takes over real stdin and stdout and speaks
//! to the daemon rather than reading the config the other subcommands rewrite.

pub mod serve;
mod serve_tools;

use clap::{Args, Subcommand};

use crate::config::Config;
use leviath_mcp::{AuthStore, LoginOutcome, MCPClient, MCPServerConfig, OAuthClient};

/// Arguments for `lev mcp`.
#[derive(Args)]
pub struct McpArgs {
    #[command(subcommand)]
    command: McpCommand,
}

impl McpArgs {
    /// A `list` invocation, for routing tests in `dispatch`.
    #[cfg(test)]
    pub(crate) fn list_for_test() -> Self {
        Self {
            command: McpCommand::List(ListArgs { json: false }),
        }
    }

    /// A `serve` invocation, for routing tests in `dispatch`.
    #[cfg(test)]
    pub(crate) fn serve_for_test() -> Self {
        Self {
            command: McpCommand::Serve(serve::McpServeArgs::default()),
        }
    }

    /// Which of the two very different things `lev mcp` was asked for.
    ///
    /// `serve` takes over stdio and talks to the daemon; everything else
    /// rewrites the config and may open a browser. The binary composes a
    /// different environment for each, so it asks this first. An enum rather
    /// than `Result<McpServeArgs, Self>`: `McpArgs` is large enough that clippy
    /// objects to it as an `Err` type.
    pub fn route(self) -> McpRoute {
        match self.command {
            McpCommand::Serve(serve) => McpRoute::Serve(serve),
            other => McpRoute::Manage(McpArgs { command: other }),
        }
    }
}

/// The two halves of `lev mcp`; see [`McpArgs::route`].
pub enum McpRoute {
    /// `lev mcp serve`: serve Leviath as an MCP server over stdio.
    Serve(serve::McpServeArgs),
    /// Everything else: manage the MCP servers Leviath itself calls.
    Manage(McpArgs),
}

#[derive(Subcommand)]
enum McpCommand {
    /// Add an MCP server (auto-starts login if it requires auth)
    Add(AddArgs),
    /// List configured MCP servers and their auth status
    List(ListArgs),
    /// Remove a configured MCP server
    Remove(RemoveArgs),
    /// Authenticate (or re-authenticate) with a configured server
    Login(ServerArg),
    /// Forget a server's stored credentials
    Logout(ServerArg),
    /// Connect to a server and list its tools
    Test(ServerArg),
    /// Serve Leviath itself as an MCP server over stdio, for a host agent
    Serve(serve::McpServeArgs),
}

#[derive(Args)]
struct AddArgs {
    /// Server name (an identifier used in config and for auth)
    name: String,
    /// Endpoint URL for an HTTP transport server
    #[arg(long)]
    url: Option<String>,
    /// Command to launch a stdio transport server
    #[arg(long)]
    command: Option<String>,
    /// Argument to pass to the command (repeatable)
    ///
    /// `allow_hyphen_values` because the arguments being passed through belong
    /// to the *server's* command line, not to `lev`. Nearly every published MCP
    /// server is launched as `npx -y <package>`, and without this the `-y` is
    /// read as an unknown flag of ours and the whole command is rejected.
    #[arg(long = "arg", allow_hyphen_values = true)]
    args: Vec<String>,
    /// Environment variable for the command, as KEY=VALUE (repeatable)
    #[arg(long = "env")]
    env: Vec<String>,
    /// HTTP header as KEY=VALUE (repeatable)
    #[arg(long = "header")]
    headers: Vec<String>,
    /// Add the server without attempting a login, even if it needs auth
    #[arg(long)]
    no_login: bool,
}

#[derive(Args)]
struct ListArgs {
    /// Emit JSON instead of a table
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct RemoveArgs {
    /// Server name
    name: String,
}

#[derive(Args)]
struct ServerArg {
    /// Server name
    name: String,
}

/// Seams the real I/O of `lev mcp` depends on, injected so the command logic is
/// unit-testable without a browser, real config, or the real home directory.
pub struct McpEnv {
    /// Path to the config file to read and rewrite.
    pub config_path: std::path::PathBuf,
    /// Path to the OAuth token store.
    pub store_path: std::path::PathBuf,
    /// How to open the browser during a login.
    pub opener: leviath_mcp::BrowserOpener,
    /// Current Unix time, for token-expiry math.
    pub now: u64,
    /// The global Rhai script-tools directory (`<leviath-home>/tools/`). `lev mcp
    /// list` also surfaces these tools (labeled `script`) so the listing covers
    /// every external tool provider, not only MCP servers. `None`
    /// disables the script scan (used by tests that only care about servers).
    pub tools_dir: Option<std::path::PathBuf>,
    /// Where OAuth grants are kept, already resolved. `lev mcp login` writes a
    /// refresh token, so it has to write it where the user asked for it to be
    /// kept.
    ///
    /// Resolved by the caller rather than here, and *before* any subcommand
    /// runs: a keychain that was asked for but cannot be reached has to fail the
    /// command outright, because falling back to the file would put a refresh
    /// token on disk that the user asked to keep out of it. Doing that once at
    /// the edge also means these code paths carry no error arm that only an
    /// unreachable keychain could take.
    pub credential_store: Option<Box<dyn leviath_core::CredentialStore>>,
    /// `[security] allow_env_vars`: which credential-shaped variables an MCP
    /// server's `${VAR}` headers may interpolate.
    pub allow_env_vars: Vec<String>,
    /// How long `lev mcp test` waits for the `initialize` handshake.
    ///
    /// Production passes [`leviath_mcp::DEFAULT_CONNECT_TIMEOUT`]; the tests
    /// pass a far longer one, because their clock is a CI runner's rather than
    /// a person's. See [`leviath_mcp::MCPClient::with_connect_timeout`] for
    /// the freeze that made this worth a field.
    pub connect_timeout: std::time::Duration,
}

/// Run a `lev mcp` subcommand against the injected environment.
pub async fn execute_with(args: McpArgs, env: &McpEnv) -> anyhow::Result<()> {
    match args.command {
        McpCommand::Add(add) => add_server(add, env).await,
        McpCommand::List(list) => list_servers(list, env),
        McpCommand::Remove(remove) => remove_server(remove, env),
        McpCommand::Login(server) => login(&server.name, env).await,
        McpCommand::Logout(server) => logout(&server.name, env),
        McpCommand::Test(server) => test(&server.name, env).await,
        // Routed in the binary through `McpArgs::route` before this is reached;
        // it needs stdio and the daemon, neither of which `McpEnv` carries.
        McpCommand::Serve(_) => {
            anyhow::bail!("`lev mcp serve` is not run through the config environment")
        }
    }
}

/// Parse `KEY=VALUE` pairs, erroring on a missing `=`.
fn parse_kv(pairs: &[String], what: &str) -> anyhow::Result<Vec<(String, String)>> {
    pairs
        .iter()
        .map(|pair| {
            pair.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| anyhow::anyhow!("{what} must be KEY=VALUE, got '{pair}'"))
        })
        .collect()
}

/// Build the `MCPServerConfig` an `add` describes, validating the transport.
fn config_from_add(add: &AddArgs) -> anyhow::Result<MCPServerConfig> {
    let env = parse_kv(&add.env, "--env")?.into_iter().collect();
    let headers = parse_kv(&add.headers, "--header")?.into_iter().collect();
    let server = MCPServerConfig {
        name: add.name.clone(),
        command: add.command.clone(),
        url: add.url.clone(),
        args: add.args.clone(),
        env,
        headers,
        transport: None,
    };
    // Reject an ambiguous or incomplete transport before writing it.
    server.validate()?;
    Ok(server)
}

async fn add_server(add: AddArgs, env: &McpEnv) -> anyhow::Result<()> {
    let server = config_from_add(&add)?;
    // `config_from_add` already validated the transport, so resolving here
    // cannot fail.
    let is_http = matches!(
        server.resolve().expect("validated in config_from_add"),
        leviath_mcp::ResolvedTransport::Http { .. }
    );

    let mut config = Config::load_from_path_public(&env.config_path)?;
    if config.mcp_servers.iter().any(|s| s.name == server.name) {
        anyhow::bail!(
            "an MCP server named '{}' already exists; remove it first",
            server.name
        );
    }
    config.mcp_servers.push(server.clone());
    config.save_to_path_public(&env.config_path)?;
    println!("Added MCP server '{}'.", server.name);

    // Auto-login for an HTTP server that isn't opted out - this is what makes
    // `add` a one-step setup for an authenticated server.
    if is_http && !add.no_login {
        match login(&server.name, env).await {
            Ok(()) => {}
            Err(e) => {
                // The server is saved; a failed login is recoverable with
                // `lev mcp login`, so don't unwind the add.
                println!("Could not complete login now ({e}).");
                println!("Run `lev mcp login {}` to try again.", server.name);
            }
        }
    }
    Ok(())
}

async fn login(name: &str, env: &McpEnv) -> anyhow::Result<()> {
    let config = Config::load_from_path_public(&env.config_path)?;
    let server = find_server(&config, name)?;
    // A loaded config's entries are validated at load, so this resolves.
    let url = match server
        .resolve()
        .expect("config entries are validated at load")
    {
        leviath_mcp::ResolvedTransport::Http { url, .. } => url.to_string(),
        leviath_mcp::ResolvedTransport::Stdio { .. } => {
            anyhow::bail!("server '{name}' uses stdio transport and does not require login");
        }
    };

    let mut store = AuthStore::load_with(&env.store_path, env.credential_store.as_deref())?;
    // Reuse a prior registration if we have one, so re-login doesn't re-register.
    let reuse = store.get(name).map(|a| a.client_id.clone());
    let outcome = OAuthClient::new()
        .login(
            &url,
            &server.headers,
            &env.allow_env_vars,
            env.opener.clone(),
            env.now,
            reuse.as_deref(),
        )
        .await?;
    match outcome {
        LoginOutcome::Authenticated(auth) => {
            store.set(name, *auth);
            store.save_with(&env.store_path, env.credential_store.as_deref())?;
            println!("✓ Authenticated with '{name}'.");
        }
        LoginOutcome::NotRequired => {
            println!("'{name}' does not need a login: it accepted the configured request.");
        }
    }
    Ok(())
}

fn logout(name: &str, env: &McpEnv) -> anyhow::Result<()> {
    let mut store = AuthStore::load_with(&env.store_path, env.credential_store.as_deref())?;
    if store.remove(name) {
        store.save_with(&env.store_path, env.credential_store.as_deref())?;
        println!("Removed stored credentials for '{name}'.");
    } else {
        println!("No stored credentials for '{name}'.");
    }
    Ok(())
}

fn remove_server(remove: RemoveArgs, env: &McpEnv) -> anyhow::Result<()> {
    let mut config = Config::load_from_path_public(&env.config_path)?;
    let before = config.mcp_servers.len();
    config.mcp_servers.retain(|s| s.name != remove.name);
    if config.mcp_servers.len() == before {
        anyhow::bail!("no MCP server named '{}'", remove.name);
    }
    config.save_to_path_public(&env.config_path)?;
    // Drop any stored credentials too, so a removed server leaves nothing behind.
    let mut store = AuthStore::load_with(&env.store_path, env.credential_store.as_deref())?;
    if store.remove(&remove.name) {
        store.save_with(&env.store_path, env.credential_store.as_deref())?;
    }
    println!("Removed MCP server '{}'.", remove.name);
    Ok(())
}

async fn test(name: &str, env: &McpEnv) -> anyhow::Result<()> {
    let config = Config::load_from_path_public(&env.config_path)?;
    let server = find_server(&config, name)?;
    let auth_header = OAuthClient::new()
        .authorization_header(name, &env.store_path, env.now)
        .await?;
    let mut client = MCPClient::from_config_with_auth(server, auth_header, &env.allow_env_vars)
        .await?
        .with_connect_timeout(env.connect_timeout);
    client.connect().await?;
    let tools = client.list_tools().await?;
    println!("✓ '{name}' connected · {} tool(s):", tools.len());
    for tool in &tools {
        println!("  - {}", tool.name);
    }
    // `shutdown` swallows subprocess errors by design, so it never fails.
    let _ = client.shutdown().await;
    Ok(())
}

fn list_servers(list: ListArgs, env: &McpEnv) -> anyhow::Result<()> {
    let config = Config::load_from_path_public(&env.config_path)?;
    let store = AuthStore::load_with(&env.store_path, env.credential_store.as_deref())?;

    let mut rows: Vec<ServerRow> = config
        .mcp_servers
        .iter()
        .map(|s| ServerRow::describe(s, &store, env.now))
        .collect();
    // Also surface the global Rhai script tools (labeled `script`) so the listing
    // covers every external tool provider, not just MCP servers.
    rows.extend(script_tool_rows(env.tools_dir.as_deref()));

    if list.json {
        // `ServerRow` is plain data; serialization is infallible.
        let json = serde_json::to_string_pretty(&rows).expect("ServerRow serializes");
        println!("{json}");
    } else if rows.is_empty() {
        println!("No MCP servers configured. Add one with `lev mcp add`.");
    } else {
        for row in &rows {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                row.kind, row.name, row.transport, row.auth, row.endpoint
            );
        }
    }
    Ok(())
}

/// The `script`-kind rows for `lev mcp list`: one per compiled global script
/// tool. A `None`/absent tools dir yields no rows.
fn script_tool_rows(tools_dir: Option<&std::path::Path>) -> Vec<ServerRow> {
    let dirs: Vec<std::path::PathBuf> = tools_dir
        .map(std::path::Path::to_path_buf)
        .into_iter()
        .collect();
    let (set, _skipped) = leviath_scripting::ScriptToolSet::discover(&dirs);
    let endpoint = tools_dir
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let mut metas = set.metas();
    metas.sort_by(|a, b| a.name.cmp(&b.name));
    metas
        .into_iter()
        // Only tools the platform can actually load (the daemon's own gate), so
        // the listing reflects what's really usable.
        .filter(|m| crate::daemon::spawn::current_platform_satisfies(&m.required_caps))
        .map(|m| ServerRow {
            kind: "script".to_string(),
            name: m.name,
            transport: "rhai".to_string(),
            endpoint: endpoint.clone(),
            auth: "n/a".to_string(),
        })
        .collect()
}

/// One row of `lev mcp list`, also the JSON shape. `kind` is `mcp` for a
/// configured server or `script` for a discovered Rhai script tool.
#[derive(serde::Serialize)]
struct ServerRow {
    kind: String,
    name: String,
    transport: String,
    endpoint: String,
    auth: String,
}

impl ServerRow {
    fn describe(server: &MCPServerConfig, store: &AuthStore, now: u64) -> Self {
        // A malformed entry still lists - with its problem shown - rather than
        // being hidden.
        let (transport, endpoint) = match server.resolve() {
            Ok(leviath_mcp::ResolvedTransport::Stdio { command, .. }) => {
                ("stdio".to_string(), command.to_string())
            }
            Ok(leviath_mcp::ResolvedTransport::Http { url, .. }) => {
                ("http".to_string(), url.to_string())
            }
            Err(_) => ("invalid".to_string(), String::new()),
        };
        let auth = auth_status(server, store, now);
        Self {
            kind: "mcp".to_string(),
            name: server.name.clone(),
            transport,
            endpoint,
            auth,
        }
    }
}

/// A one-word description of a server's auth state, for display.
fn auth_status(server: &MCPServerConfig, store: &AuthStore, now: u64) -> String {
    let is_http = matches!(
        server.resolve(),
        Ok(leviath_mcp::ResolvedTransport::Http { .. })
    );
    if !is_http {
        return "n/a".to_string();
    }
    match store.get(&server.name) {
        Some(auth) if auth.is_expired_at(now) => "expired".to_string(),
        Some(_) => "authenticated".to_string(),
        // A configured `Authorization` header is a credential too. Calling it
        // "none" reads as "log in here", which is the prompt that sends people
        // into an OAuth flow their server never wanted.
        None if server.has_auth_header() => "header".to_string(),
        None => "none".to_string(),
    }
}

/// Look up a configured server by name.
fn find_server<'a>(config: &'a Config, name: &str) -> anyhow::Result<&'a MCPServerConfig> {
    config
        .mcp_servers
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow::anyhow!("no MCP server named '{name}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serve_does_not_run_through_the_config_environment() {
        let dir = tempfile::tempdir().unwrap();
        let err = execute_with(
            McpArgs::serve_for_test(),
            &env_at(dir.path(), never_opens, 0),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("lev mcp serve"), "{err}");
    }

    fn env_at(
        dir: &std::path::Path,
        opener: impl Fn(&str) -> bool + Send + Sync + 'static,
        now: u64,
    ) -> McpEnv {
        McpEnv {
            config_path: dir.join("config.toml"),
            store_path: dir.join("mcp-auth.json"),
            opener: std::sync::Arc::new(opener),
            now,
            // Default: no script scan, so server-focused tests stay hermetic. The
            // script-row path has its own dedicated test with a seeded dir.
            tools_dir: None,
            credential_store: None,
            allow_env_vars: Vec::new(),
            connect_timeout: TEST_CONNECT_TIMEOUT,
        }
    }

    /// The handshake deadline for tests: long enough that only a real hang
    /// trips it.
    ///
    /// Production's 30s asks "how long should a person's agent startup hang on
    /// a broken server". A test asks something else, and answering it with a
    /// person's number means the suite fails when the *machine* stalls rather
    /// than when the server does. One did: a `windows-latest` job froze for
    /// 159 seconds on 2026-08-21 - zero tests completed, the binary took 241s
    /// against a normal 40s - and this deadline was the only casualty, its
    /// panic reported the instant the process was scheduled again. The stub
    /// had answered nothing because nothing was running.
    ///
    /// Five minutes is not a guess at how slow a runner gets; it is "longer
    /// than any stall we have seen, and still bounded", so a genuinely wedged
    /// server still fails the test rather than hanging CI forever.
    const TEST_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

    fn never_opens(_: &str) -> bool {
        false
    }

    fn add_args(name: &str, url: Option<&str>, command: Option<&str>) -> AddArgs {
        AddArgs {
            name: name.to_string(),
            url: url.map(String::from),
            command: command.map(String::from),
            args: vec![],
            env: vec![],
            headers: vec![],
            no_login: true,
        }
    }

    // ─── parse_kv ─────────────────────────────────────────────────────────

    #[test]
    fn parse_kv_splits_pairs() {
        let pairs = parse_kv(&["A=1".to_string(), "B=x=y".to_string()], "--env").unwrap();
        assert_eq!(
            pairs,
            vec![("A".into(), "1".into()), ("B".into(), "x=y".into())]
        );
    }

    #[test]
    fn parse_kv_rejects_a_missing_equals() {
        let err = parse_kv(&["bad".to_string()], "--header").expect_err("no = must fail");
        assert!(
            err.to_string().contains("--header must be KEY=VALUE"),
            "got: {err}"
        );
    }

    // ─── config_from_add ──────────────────────────────────────────────────

    #[test]
    fn config_from_add_builds_an_http_server() {
        let mut add = add_args("remote", Some("https://e.com/mcp"), None);
        add.headers = vec!["Authorization=Bearer x".to_string()];
        let server = config_from_add(&add).unwrap();
        assert_eq!(server.url.as_deref(), Some("https://e.com/mcp"));
        assert_eq!(server.headers.get("Authorization").unwrap(), "Bearer x");
    }

    #[test]
    fn config_from_add_rejects_an_ambiguous_transport() {
        let add = add_args("x", Some("https://e.com"), Some("npx"));
        let err = config_from_add(&add).expect_err("both url and command must fail");
        assert!(err.to_string().contains("transport"), "got: {err}");
    }

    #[test]
    fn config_from_add_propagates_a_bad_env_pair() {
        let mut add = add_args("x", None, Some("npx"));
        add.env = vec!["NOEQUALS".to_string()];
        assert!(config_from_add(&add).is_err());
    }

    // ─── add / list / remove (no network) ─────────────────────────────────

    #[tokio::test]
    async fn add_writes_a_stdio_server_and_list_shows_it() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        execute_with(
            McpArgs {
                command: McpCommand::Add(add_args("local", None, Some("npx"))),
            },
            &env,
        )
        .await
        .unwrap();

        let config = Config::load_from_path_public(&env.config_path).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].command.as_deref(), Some("npx"));

        // list (json) reports it as a stdio server needing no auth.
        list_servers(ListArgs { json: true }, &env).unwrap();
        let rows: Vec<ServerRow> = vec![ServerRow::describe(
            &config.mcp_servers[0],
            &AuthStore::default(),
            0,
        )];
        assert_eq!(rows[0].transport, "stdio");
        assert_eq!(rows[0].auth, "n/a");
    }

    #[tokio::test]
    async fn add_rejects_a_duplicate_name() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        let mk = || McpArgs {
            command: McpCommand::Add(add_args("dup", None, Some("npx"))),
        };
        execute_with(mk(), &env).await.unwrap();
        let err = execute_with(mk(), &env).await.expect_err("dup must fail");
        assert!(err.to_string().contains("already exists"), "got: {err}");
    }

    #[tokio::test]
    async fn remove_deletes_the_server_and_its_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        execute_with(
            McpArgs {
                command: McpCommand::Add(add_args("gone", Some("https://e.com/mcp"), None)),
            },
            &env,
        )
        .await
        .unwrap();
        // Seed a credential to prove removal clears it too.
        let mut store = AuthStore::default();
        store.set("gone", leviath_mcp::ServerAuth::default());
        store.save(&env.store_path).unwrap();

        execute_with(
            McpArgs {
                command: McpCommand::Remove(RemoveArgs {
                    name: "gone".to_string(),
                }),
            },
            &env,
        )
        .await
        .unwrap();

        let config = Config::load_from_path_public(&env.config_path).unwrap();
        assert!(config.mcp_servers.is_empty());
        assert!(
            AuthStore::load(&env.store_path)
                .unwrap()
                .get("gone")
                .is_none()
        );
    }

    #[tokio::test]
    async fn remove_without_stored_credentials_still_removes_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        execute_with(
            McpArgs {
                command: McpCommand::Add(add_args("plain", None, Some("npx"))),
            },
            &env,
        )
        .await
        .unwrap();
        // No credentials were ever stored, so removal skips the store write.
        remove_server(
            RemoveArgs {
                name: "plain".to_string(),
            },
            &env,
        )
        .unwrap();
        assert!(
            Config::load_from_path_public(&env.config_path)
                .unwrap()
                .mcp_servers
                .is_empty()
        );
    }

    #[tokio::test]
    async fn remove_of_an_unknown_server_errors() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        let err = execute_with(
            McpArgs {
                command: McpCommand::Remove(RemoveArgs {
                    name: "ghost".to_string(),
                }),
            },
            &env,
        )
        .await
        .expect_err("removing a missing server must fail");
        assert!(
            err.to_string().contains("no MCP server named"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn list_of_nothing_is_friendly() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        // No config file yet; list must still succeed with an empty result -
        // routed through execute_with to cover the List dispatch arm.
        execute_with(
            McpArgs {
                command: McpCommand::List(ListArgs { json: false }),
            },
            &env,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn list_prints_a_table_row_per_server() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        execute_with(
            McpArgs {
                command: McpCommand::Add(add_args("local", None, Some("npx"))),
            },
            &env,
        )
        .await
        .unwrap();
        // Non-JSON list with a server present: the table branch.
        execute_with(
            McpArgs {
                command: McpCommand::List(ListArgs { json: false }),
            },
            &env,
        )
        .await
        .unwrap();
    }

    // ─── logout ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn logout_removes_stored_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        let mut store = AuthStore::default();
        store.set("srv", leviath_mcp::ServerAuth::default());
        store.save(&env.store_path).unwrap();

        execute_with(
            McpArgs {
                command: McpCommand::Logout(ServerArg {
                    name: "srv".to_string(),
                }),
            },
            &env,
        )
        .await
        .unwrap();
        assert!(
            AuthStore::load(&env.store_path)
                .unwrap()
                .get("srv")
                .is_none()
        );
    }

    #[test]
    fn logout_of_an_unauthenticated_server_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        logout("srv", &env).unwrap();
    }

    // ─── login guards ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn login_of_an_unknown_server_errors() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        let err = login("nope", &env)
            .await
            .expect_err("unknown server must fail");
        assert!(
            err.to_string().contains("no MCP server named"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn login_of_a_stdio_server_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        execute_with(
            McpArgs {
                command: McpCommand::Add(add_args("local", None, Some("npx"))),
            },
            &env,
        )
        .await
        .unwrap();
        // Through execute_with to cover the Login dispatch arm.
        let err = execute_with(
            McpArgs {
                command: McpCommand::Login(ServerArg {
                    name: "local".to_string(),
                }),
            },
            &env,
        )
        .await
        .expect_err("stdio login must fail");
        assert!(
            err.to_string().contains("does not require login"),
            "got: {err}"
        );
    }

    // ─── auth_status / ServerRow for the HTTP + token states ──────────────

    #[test]
    fn auth_status_reports_each_state() {
        let http = MCPServerConfig::http("s", "https://e.com/mcp");
        let mut store = AuthStore::default();
        assert_eq!(auth_status(&http, &store, 0), "none");

        store.set(
            "s",
            leviath_mcp::ServerAuth {
                expires_at: 10_000,
                ..Default::default()
            },
        );
        assert_eq!(auth_status(&http, &store, 1_000), "authenticated");
        assert_eq!(auth_status(&http, &store, 20_000), "expired");

        let stdio = MCPServerConfig::stdio("s", "npx", vec![]);
        assert_eq!(auth_status(&stdio, &store, 0), "n/a");
    }

    // ─── auto-login on add, and `test`, against real mock servers ─────────

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::{Json, Router};

    /// A standards-correct mock authorization server + MCP endpoint, enough for
    /// the CLI's add→login→store round trip. Returns its base URL.
    async fn mock_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = base.clone();
        let app = Router::new()
            .route(
                "/mcp",
                post(|State(base): State<String>| async move {
                    let hint = format!(
                        "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\""
                    );
                    (
                        StatusCode::UNAUTHORIZED,
                        [(reqwest::header::WWW_AUTHENTICATE, hint)],
                    )
                }),
            )
            .route(
                "/.well-known/oauth-protected-resource",
                get(|State(base): State<String>| async move {
                    Json(serde_json::json!({
                        "resource": format!("{base}/mcp"),
                        "authorization_servers": [base],
                    }))
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(|State(base): State<String>| async move {
                    Json(serde_json::json!({
                        "issuer": base,
                        "authorization_endpoint": format!("{base}/authorize"),
                        "token_endpoint": format!("{base}/token"),
                        "registration_endpoint": format!("{base}/register"),
                        "scopes_supported": ["openid"],
                    }))
                }),
            )
            .route(
                "/register",
                post(|| async { Json(serde_json::json!({ "client_id": "cli-client" })) }),
            )
            .route(
                "/token",
                post(|| async {
                    Json(serde_json::json!({
                        "access_token": "cli-access",
                        "refresh_token": "cli-refresh",
                        "expires_in": 3600,
                    }))
                }),
            )
            .with_state(state);
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        base
    }

    /// A browser stub that consents by GETting the loopback callback itself.
    fn auto_consent(authorize_url: &str) -> bool {
        let url = reqwest::Url::parse(authorize_url).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        let redirect = params["redirect_uri"].clone();
        let state = params["state"].clone();
        tokio::spawn(async move {
            let cb = format!("{redirect}?code=cli-code&state={state}");
            let _ = reqwest::Client::new().get(&cb).send().await;
        });
        true
    }

    /// An MCP endpoint that takes its own API token and publishes no OAuth
    /// metadata whatsoever, which is what a server like GitHub's looks like once
    /// you have configured a header for it.
    async fn mock_token_authenticated_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // Answers everything, and publishes no OAuth metadata at all, so a login
        // that goes looking for some fails loudly. That the header is what earns
        // the `200` is `leviath-mcp`'s test to make; this one is about what the
        // CLI does with the answer.
        let app = Router::new().route("/mcp", post(|| async { StatusCode::OK }));
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        base
    }

    /// Adding a header-authenticated server must not chase an OAuth login it
    /// cannot complete. Before this, `add` printed a discovery 404 and told the
    /// user to run `lev mcp login`, which then printed the same 404 forever.
    #[tokio::test]
    async fn add_with_an_auth_header_does_not_chase_an_oauth_login() {
        let base = mock_token_authenticated_server().await;
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), auto_consent, 1_000);

        let add = AddArgs {
            name: "hub".to_string(),
            url: Some(format!("{base}/mcp")),
            command: None,
            args: vec![],
            env: vec![],
            headers: vec!["Authorization=Bearer configured-token".to_string()],
            no_login: false,
        };
        execute_with(
            McpArgs {
                command: McpCommand::Add(add),
            },
            &env,
        )
        .await
        .expect("adding a header-authenticated server must succeed");

        let config = Config::load_from_path_public(&env.config_path).unwrap();
        assert_eq!(
            config.mcp_servers[0].headers.get("Authorization").unwrap(),
            "Bearer configured-token"
        );
        // Nothing to store: the header is the credential, and it stays in the
        // config rather than being duplicated into the OAuth store.
        let stored = AuthStore::load(&env.store_path).unwrap();
        assert!(stored.get("hub").is_none());

        // And an explicit login says so rather than failing.
        login("hub", &env)
            .await
            .expect("an explicit login on such a server is a no-op, not an error");
        assert!(
            AuthStore::load(&env.store_path)
                .unwrap()
                .get("hub")
                .is_none()
        );

        // The listing reports the header as the credential it is. "none" here
        // is what tells a user to go and log in.
        let store = AuthStore::load(&env.store_path).unwrap();
        assert_eq!(auth_status(&config.mcp_servers[0], &store, 1_000), "header");
    }

    #[tokio::test]
    async fn add_http_server_auto_starts_login_and_stores_the_token() {
        let base = mock_server().await;
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), auto_consent, 1_000);

        // `add` with login enabled (no_login=false).
        let add = AddArgs {
            name: "navigator".to_string(),
            url: Some(format!("{base}/mcp")),
            command: None,
            args: vec![],
            env: vec![],
            headers: vec![],
            no_login: false,
        };
        execute_with(
            McpArgs {
                command: McpCommand::Add(add),
            },
            &env,
        )
        .await
        .unwrap();

        // The server is in config and the token landed in the store.
        let config = Config::load_from_path_public(&env.config_path).unwrap();
        assert_eq!(config.mcp_servers[0].name, "navigator");
        let stored = AuthStore::load(&env.store_path).unwrap();
        assert_eq!(stored.get("navigator").unwrap().access_token, "cli-access");
        // And no token leaked into the config file.
        let config_text = std::fs::read_to_string(&env.config_path).unwrap();
        assert!(
            !config_text.contains("cli-access"),
            "token must not be in config"
        );
    }

    #[tokio::test]
    async fn add_http_server_survives_a_failed_login() {
        // A server whose /mcp probe leads nowhere: the add still persists, and
        // the command succeeds with a "run login later" message.
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        let add = AddArgs {
            name: "remote".to_string(),
            url: Some("http://127.0.0.1:1/mcp".to_string()),
            command: None,
            args: vec![],
            env: vec![],
            headers: vec![],
            no_login: false,
        };
        execute_with(
            McpArgs {
                command: McpCommand::Add(add),
            },
            &env,
        )
        .await
        .expect("add should not fail just because login did");
        let config = Config::load_from_path_public(&env.config_path).unwrap();
        assert_eq!(config.mcp_servers.len(), 1, "the server is still saved");
    }

    #[tokio::test]
    async fn explicit_login_reuses_a_prior_client_id() {
        let base = mock_server().await;
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), auto_consent, 1_000);
        execute_with(
            McpArgs {
                command: McpCommand::Add(add_args("navigator", Some(&format!("{base}/mcp")), None)),
            },
            &env,
        )
        .await
        .unwrap();
        // First login registers, second reuses the stored client_id.
        login("navigator", &env).await.unwrap();
        login("navigator", &env).await.unwrap();
        let stored = AuthStore::load(&env.store_path).unwrap();
        assert_eq!(stored.get("navigator").unwrap().client_id, "cli-client");
    }

    /// A minimal stdio MCP server for the `test` command.
    const STUB: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line); m = req.get("method",""); i = req.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":i,"result":{"capabilities":{},"protocolVersion":"2024-11-05"}}), flush=True)
    elif m == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":i,"result":{"tools":[{"name":"ping","inputSchema":{}}]}}), flush=True)
"#;

    #[tokio::test]
    async fn test_command_connects_and_lists_tools() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        let mut add = add_args("local", None, Some("python3"));
        add.args = vec!["-c".to_string(), STUB.to_string()];
        execute_with(
            McpArgs {
                command: McpCommand::Add(add),
            },
            &env,
        )
        .await
        .unwrap();

        // Through execute_with to cover the Test dispatch arm.
        execute_with(
            McpArgs {
                command: McpCommand::Test(ServerArg {
                    name: "local".to_string(),
                }),
            },
            &env,
        )
        .await
        .expect("test should connect and list tools");
    }

    #[tokio::test]
    async fn test_command_errors_for_an_unknown_server() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        assert!(test("ghost", &env).await.is_err());
    }

    #[test]
    fn server_row_describes_an_http_server() {
        let http = MCPServerConfig::http("remote", "https://e.com/mcp");
        let row = ServerRow::describe(&http, &AuthStore::default(), 0);
        assert_eq!(row.kind, "mcp");
        assert_eq!(row.transport, "http");
        assert_eq!(row.endpoint, "https://e.com/mcp");
        assert_eq!(row.auth, "none");
    }

    #[test]
    fn script_tool_rows_lists_compiled_tools() {
        // None → no rows.
        assert!(script_tool_rows(None).is_empty());
        // A tools dir with two valid + a broken script → two `script` rows,
        // sorted by name (the broken one is silently omitted, like the daemon).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("up.rhai"), "// @tool up\nparams.x").unwrap();
        std::fs::write(dir.path().join("down.rhai"), "// @tool down\n1").unwrap();
        std::fs::write(dir.path().join("bad.rhai"), "no directive\nlet").unwrap();
        // A tool requiring an unsatisfiable capability is filtered out (not usable).
        std::fs::write(
            dir.path().join("gpu.rhai"),
            "// @tool gpu\n// @requires gpu\n1",
        )
        .unwrap();
        let rows = script_tool_rows(Some(dir.path()));
        assert_eq!(rows.len(), 2, "the gpu tool is filtered out");
        assert!(rows.iter().all(|r| r.name != "gpu"));
        assert_eq!(rows[0].kind, "script");
        assert_eq!(rows[0].name, "down", "sorted by name");
        assert_eq!(rows[1].name, "up");
        assert_eq!(rows[0].transport, "rhai");
        assert_eq!(rows[0].auth, "n/a");
        assert!(rows[0].endpoint.contains(dir.path().to_str().unwrap()));
    }

    #[tokio::test]
    async fn list_includes_script_tools_when_tools_dir_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = env_at(dir.path(), never_opens, 0);
        // Seed a global tools dir with one script.
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(tools.join("up.rhai"), "// @tool up\nparams.x").unwrap();
        env.tools_dir = Some(tools);
        // No MCP servers configured, but the script tool still lists (text + JSON).
        list_servers(ListArgs { json: false }, &env).unwrap();
        list_servers(ListArgs { json: true }, &env).unwrap();
    }

    #[test]
    fn never_opens_reports_no_browser() {
        // The stub opener used where a login should not reach the browser.
        assert!(!never_opens("https://x"));
    }

    // ─── I/O failure arms ─────────────────────────────────────────────────
    //
    // Each config/store read or write has an error-propagation `?`. A directory
    // where a file is expected makes a read fail; a read-only file makes a
    // rewrite fail. These drive each arm portably and deterministically.

    /// An env whose config and store paths are directories, so reads of them
    /// fail.
    fn env_with_unreadable_paths(dir: &std::path::Path) -> McpEnv {
        let cfg = dir.join("config-dir");
        let store = dir.join("store-dir");
        std::fs::create_dir(&cfg).unwrap();
        std::fs::create_dir(&store).unwrap();
        McpEnv {
            config_path: cfg,
            store_path: store,
            opener: std::sync::Arc::new(never_opens),
            now: 0,
            tools_dir: None,
            credential_store: None,
            allow_env_vars: Vec::new(),
            connect_timeout: TEST_CONNECT_TIMEOUT,
        }
    }

    /// Seed a config file holding `server`, bypassing the network-touching add.
    fn seed_config(env: &McpEnv, server: MCPServerConfig) {
        let mut config = Config::default();
        config.mcp_servers.push(server);
        config.save_to_path_public(&env.config_path).unwrap();
    }

    /// Seed a store file holding `name`, then make it read-only so a later
    /// rewrite fails while reads still succeed.
    fn seed_readonly_store(env: &McpEnv, name: &str) {
        let mut store = AuthStore::default();
        store.set(name, leviath_mcp::ServerAuth::default());
        store.save(&env.store_path).unwrap();
        let mut perms = std::fs::metadata(&env.store_path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&env.store_path, perms).unwrap();
    }

    #[tokio::test]
    async fn commands_surface_an_unreadable_config() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_unreadable_paths(dir.path());
        assert!(
            execute_with(
                McpArgs {
                    command: McpCommand::Add(add_args("x", None, Some("npx")))
                },
                &env
            )
            .await
            .is_err()
        );
        assert!(list_servers(ListArgs { json: false }, &env).is_err());
        assert!(
            remove_server(
                RemoveArgs {
                    name: "x".to_string()
                },
                &env
            )
            .is_err()
        );
        assert!(login("x", &env).await.is_err());
        assert!(test("x", &env).await.is_err());
        assert!(logout("x", &env).is_err());
    }

    #[tokio::test]
    async fn add_surfaces_a_bad_header_and_an_unwritable_config() {
        let dir = tempfile::tempdir().unwrap();
        // Bad --header: config_from_add fails inside add_server (parse_kv arm).
        let env = env_at(dir.path(), never_opens, 0);
        let mut bad = add_args("x", None, Some("npx"));
        bad.headers = vec!["NOEQUALS".to_string()];
        assert!(
            execute_with(
                McpArgs {
                    command: McpCommand::Add(bad)
                },
                &env
            )
            .await
            .is_err()
        );

        // Unwritable config: parent is a file, so the save cannot create it.
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let ro_env = McpEnv {
            config_path: file.join("config.toml"),
            store_path: dir.path().join("s.json"),
            opener: std::sync::Arc::new(never_opens),
            now: 0,
            tools_dir: None,
            credential_store: None,
            allow_env_vars: Vec::new(),
            connect_timeout: TEST_CONNECT_TIMEOUT,
        };
        assert!(
            execute_with(
                McpArgs {
                    command: McpCommand::Add(add_args("x", None, Some("npx")))
                },
                &ro_env
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn login_surfaces_an_unreadable_store() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        seed_config(
            &env,
            MCPServerConfig::http("remote", "http://127.0.0.1:1/mcp"),
        );
        // Config + resolve succeed; the store is a directory, so its load fails
        // before any browser flow.
        std::fs::create_dir(&env.store_path).unwrap();
        assert!(login("remote", &env).await.is_err());
    }

    #[tokio::test]
    async fn remove_surfaces_an_unwritable_config() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        seed_config(&env, MCPServerConfig::stdio("x", "npx", vec![]));
        // Make the config file read-only: load reads it, but the rewrite fails.
        let mut perms = std::fs::metadata(&env.config_path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&env.config_path, perms).unwrap();
        assert!(
            remove_server(
                RemoveArgs {
                    name: "x".to_string()
                },
                &env
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn login_surfaces_an_unwritable_store() {
        let base = mock_server().await;
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), auto_consent, 1_000);
        execute_with(
            McpArgs {
                command: McpCommand::Add(add_args("navigator", Some(&format!("{base}/mcp")), None)),
            },
            &env,
        )
        .await
        .unwrap();
        // Store reads fine (empty) but is read-only, so persisting the token fails.
        seed_readonly_store(&env, "other");
        assert!(login("navigator", &env).await.is_err());
    }

    #[tokio::test]
    async fn logout_surfaces_an_unwritable_store() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        seed_readonly_store(&env, "srv");
        // Load returns the seeded cred (read is allowed), remove is true, but
        // the rewrite fails.
        assert!(logout("srv", &env).is_err());
    }

    #[tokio::test]
    async fn remove_surfaces_an_unreadable_store() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        seed_config(&env, MCPServerConfig::stdio("x", "npx", vec![]));
        // Config load + save succeed; the store is a directory, so its load fails.
        std::fs::create_dir(&env.store_path).unwrap();
        assert!(
            remove_server(
                RemoveArgs {
                    name: "x".to_string()
                },
                &env
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn remove_surfaces_an_unwritable_store() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        seed_config(&env, MCPServerConfig::stdio("x", "npx", vec![]));
        seed_readonly_store(&env, "x");
        // Config rewrite ok; the store has "x" so remove is true, but the store
        // rewrite fails.
        assert!(
            remove_server(
                RemoveArgs {
                    name: "x".to_string()
                },
                &env
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn list_surfaces_an_unreadable_store() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        seed_config(&env, MCPServerConfig::http("remote", "https://e.com/mcp"));
        std::fs::create_dir(&env.store_path).unwrap();
        assert!(list_servers(ListArgs { json: false }, &env).is_err());
    }

    #[tokio::test]
    async fn test_surfaces_an_unrefreshable_token() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 1_000);
        seed_config(
            &env,
            MCPServerConfig::http("remote", "http://127.0.0.1:1/mcp"),
        );
        // An expired token with a dead refresh endpoint: authorization_header
        // errors before any connection is attempted.
        let mut store = AuthStore::default();
        store.set(
            "remote",
            leviath_mcp::ServerAuth {
                token_endpoint: "http://127.0.0.1:1/token".to_string(),
                refresh_token: Some("good".to_string()),
                expires_at: 1,
                ..Default::default()
            },
        );
        store.save(&env.store_path).unwrap();
        assert!(test("remote", &env).await.is_err());
    }

    #[tokio::test]
    async fn test_surfaces_a_spawn_failure() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        seed_config(
            &env,
            MCPServerConfig::stdio("x", "definitely-not-a-real-binary-xyz", vec![]),
        );
        // Auth resolves to None (stdio), then from_config's spawn fails.
        assert!(test("x", &env).await.is_err());
    }

    #[tokio::test]
    async fn test_surfaces_a_connect_failure() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        seed_config(
            &env,
            MCPServerConfig::http("remote", "http://127.0.0.1:1/mcp"),
        );
        // The transport builds, but connecting to a dead port fails.
        assert!(test("remote", &env).await.is_err());
    }

    #[tokio::test]
    async fn test_surfaces_a_list_tools_failure() {
        // A stdio server that answers initialize but errors tools/list.
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path(), never_opens, 0);
        let stub = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line); m = req.get("method",""); i = req.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":i,"result":{"capabilities":{},"protocolVersion":"2024-11-05"}}), flush=True)
    elif m == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":i,"error":{"code":-32603,"message":"boom"}}), flush=True)
"#;
        seed_config(
            &env,
            MCPServerConfig::stdio("x", "python3", vec!["-c".to_string(), stub.to_string()]),
        );
        assert!(test("x", &env).await.is_err());
    }

    #[test]
    fn server_row_marks_an_invalid_entry() {
        // Neither command nor url → invalid, but still listed.
        let bad = MCPServerConfig {
            name: "broken".to_string(),
            ..Default::default()
        };
        let row = ServerRow::describe(&bad, &AuthStore::default(), 0);
        assert_eq!(row.transport, "invalid");
        assert_eq!(row.auth, "n/a");
    }
}
