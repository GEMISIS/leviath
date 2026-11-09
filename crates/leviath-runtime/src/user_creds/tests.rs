use super::*;
use std::collections::HashMap;

/// A source reading `dir` alone: no environment, no keychain.
fn bare(dir: &std::path::Path) -> UserCredsSource<'static> {
    UserCredsSource {
        data_dir: dir.to_path_buf(),
        config_path: None,
        env: None,
        keychain: None,
    }
}

/// `dir` holding `config.toml` with `text`.
fn with_config(text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), text).unwrap();
    dir
}

/// The creds for `name`, which must be there.
fn cred<'a>(creds: &'a UserCreds, name: &str) -> &'a ProviderCreds {
    creds
        .creds
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} is registered"))
}

/// An environment of exactly these variables.
fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let vars: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name| vars.get(name).cloned()
}

/// No file is the defaults: nothing registered, `anthropic` as the default
/// provider, and the two paths under the data directory.
#[test]
fn a_missing_file_is_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let loaded = load_user_creds(&bare(dir.path())).unwrap();
    assert!(loaded.creds.is_empty());
    assert_eq!(loaded.defaults.provider, "anthropic");
    assert!(loaded.defaults.retention.file_uploads);
    assert_eq!(loaded.credential_store, CredentialStoreKind::File);
    assert_eq!(loaded.grants_path, dir.path().join("provider-auth.json"));
    assert_eq!(
        loaded.capability_cache,
        dir.path().join("model_capabilities.json")
    );
    assert!(loaded.origins.is_empty());
    assert!(loaded.warnings.is_empty());
}

/// Every entry of [`KEYED_PROVIDERS`] has its fields, found by name, and
/// those fields are the ones named for it: a key written as
/// `<provider>_api_key` registers that provider and no other, and its
/// variable stands in for it. Bound by name, so reordering or inserting an
/// entry cannot move one provider's key onto another.
#[test]
fn every_keyed_provider_reads_its_own_field_and_variable() {
    let mut names: Vec<&str> = KEYED_PROVIDERS.iter().map(|k| k.provider).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(
        names.len(),
        KEYED_PROVIDERS.len(),
        "a provider listed twice"
    );
    let mut slotted: Vec<&str> = settings::SLOTS.iter().map(|s| s.provider).collect();
    slotted.sort_unstable();
    assert_eq!(
        slotted, names,
        "fields for a provider the table does not list"
    );
    assert_eq!(keyed_provider("ollama"), None);

    for keyed in KEYED_PROVIDERS {
        let (provider, var) = (keyed.provider, keyed.key_env);
        assert_eq!(keyed_provider(provider), Some(keyed));
        assert_eq!(settings::slots(provider).provider, provider);
        let table = if provider == "openrouter" {
            ""
        } else {
            "[providers]\n"
        };
        let dir = with_config(&format!("{table}{provider}_api_key = \"k-{provider}\"\n"));
        let loaded = load_user_creds(&bare(dir.path())).unwrap();
        assert_eq!(loaded.creds.len(), 1, "{provider}");
        assert_eq!(loaded.creds[0].name, provider);
        assert_eq!(
            loaded.creds[0].api_key.as_deref(),
            Some(format!("k-{provider}").as_str())
        );
        assert_eq!(loaded.origins[provider], KeyOrigin::File);

        let empty = tempfile::tempdir().unwrap();
        let env = env_of(&[(var, "from-env")]);
        let source = UserCredsSource {
            env: Some(&env),
            ..bare(empty.path())
        };
        let loaded = load_user_creds(&source).unwrap();
        assert_eq!(loaded.creds[0].name, provider, "{var}");
        assert_eq!(loaded.origins[provider], KeyOrigin::Env(var.to_string()));

        // The host variable reaches this provider's host and no other's.
        let host = keyed.base_url_env.expect("every keyed provider has one");
        let dir = with_config(&format!("{table}{provider}_api_key = \"k\"\n"));
        let env = env_of(&[(host, "https://gateway.example")]);
        let source = UserCredsSource {
            env: Some(&env),
            ..bare(dir.path())
        };
        let loaded = load_user_creds(&source).unwrap();
        assert_eq!(
            loaded.creds[0].base_url.as_deref(),
            Some("https://gateway.example"),
            "{host}"
        );
    }
}

/// A key variable that is set but blank fills the slot, as it does for
/// `lev`, so the provider is not registered and the keychain is not asked
/// for it; a warning names the variable so the disappearance is explained.
#[test]
fn a_blank_key_variable_is_explained() {
    let dir = with_config("[security]\ncredential_store = \"keychain\"\n");
    let env = env_of(&[("OPENAI_API_KEY", "  ")]);
    let store = Arc::new(leviath_core::MemoryStore::new());
    store
        .set(&leviath_core::provider_account("openai"), "kc-openai")
        .unwrap();
    let open = move || Ok(store.clone() as Arc<dyn CredentialStore>);
    let source = UserCredsSource {
        env: Some(&env),
        keychain: Some(&open),
        ..bare(dir.path())
    };
    let loaded = load_user_creds(&source).unwrap();
    assert!(loaded.creds.is_empty(), "{:?}", loaded.creds);
    assert!(loaded.origins.is_empty());
    assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
    assert!(
        loaded.warnings[0].starts_with("OPENAI_API_KEY is set but empty"),
        "{:?}",
        loaded.warnings
    );
}

/// A store that counts every read, and an opener that counts every open.
struct Counting {
    reads: std::sync::atomic::AtomicUsize,
}

impl CredentialStore for Counting {
    fn get(&self, _: &str) -> Result<Option<String>, String> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Some("from-store".to_string()))
    }
    fn set(&self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn delete(&self, _: &str) -> Result<bool, String> {
        Ok(false)
    }
}

/// The keychain is asked only for the keys the file and the environment
/// left unset, and not opened at all when there are none.
#[test]
fn the_keychain_is_asked_only_for_keys_still_missing() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = with_config(
        "[security]\ncredential_store = \"keychain\"\n[providers]\nanthropic_api_key = \"f\"\n",
    );
    let store = Arc::new(Counting {
        reads: AtomicUsize::new(0),
    });
    let opens = AtomicUsize::new(0);
    let open = || {
        opens.fetch_add(1, Ordering::SeqCst);
        Ok(store.clone() as Arc<dyn CredentialStore>)
    };

    // Every other key from the environment: nothing left to ask for.
    let vars: Vec<(&str, &str)> = KEYED_PROVIDERS
        .iter()
        .filter(|k| k.provider != "anthropic")
        .map(|k| (k.key_env, "from-env"))
        .collect();
    let env = env_of(&vars);
    let source = UserCredsSource {
        env: Some(&env),
        keychain: Some(&open),
        ..bare(dir.path())
    };
    let loaded = load_user_creds(&source).unwrap();
    assert_eq!(loaded.creds.len(), KEYED_PROVIDERS.len());
    assert_eq!(opens.load(Ordering::SeqCst), 0, "the store was opened");
    assert_eq!(store.reads.load(Ordering::SeqCst), 0, "the store was read");
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);

    // Two keys missing: two reads, one for each, and nothing else.
    let vars: Vec<(&str, &str)> = KEYED_PROVIDERS
        .iter()
        .filter(|k| !["anthropic", "google", "meta"].contains(&k.provider))
        .map(|k| (k.key_env, "from-env"))
        .collect();
    let env = env_of(&vars);
    let source = UserCredsSource {
        env: Some(&env),
        keychain: Some(&open),
        ..bare(dir.path())
    };
    let loaded = load_user_creds(&source).unwrap();
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    assert_eq!(store.reads.load(Ordering::SeqCst), 2);
    assert_eq!(loaded.origins["google"], KeyOrigin::Keychain);
    assert_eq!(loaded.origins["meta"], KeyOrigin::Keychain);
    assert_eq!(loaded.origins["anthropic"], KeyOrigin::File);
}

/// File, then environment, then keychain; a blank key in the file blocks the
/// rest and registers nothing.
#[test]
fn the_file_beats_the_environment_which_beats_the_keychain() {
    let dir = with_config(
        "[security]\ncredential_store = \"keychain\"\n\
         [providers]\nanthropic_api_key = \"file\"\ngoogle_api_key = \"\"\n",
    );
    let env = env_of(&[
        ("ANTHROPIC_API_KEY", "env-anthropic"),
        ("OPENAI_API_KEY", "env-openai"),
        ("GOOGLE_API_KEY", "env-google"),
    ]);
    let store = Arc::new(leviath_core::MemoryStore::new());
    for provider in ["anthropic", "openai", "google", "xai"] {
        store
            .set(
                &leviath_core::provider_account(provider),
                &format!("kc-{provider}"),
            )
            .unwrap();
    }
    let open = move || Ok(store.clone() as Arc<dyn CredentialStore>);
    let source = UserCredsSource {
        env: Some(&env),
        keychain: Some(&open),
        ..bare(dir.path())
    };
    let loaded = load_user_creds(&source).unwrap();
    let key = |name: &str| cred(&loaded, name).api_key.clone().unwrap();
    assert_eq!(key("anthropic"), "file");
    assert_eq!(key("openai"), "env-openai");
    assert_eq!(key("xai"), "kc-xai");
    assert!(loaded.creds.iter().all(|c| c.name != "google"));
    assert_eq!(loaded.credential_store, CredentialStoreKind::Keychain);
    assert_eq!(
        loaded.origins,
        BTreeMap::from([
            ("anthropic".to_string(), KeyOrigin::File),
            (
                "openai".to_string(),
                KeyOrigin::Env("OPENAI_API_KEY".to_string())
            ),
            ("xai".to_string(), KeyOrigin::Keychain),
        ])
    );
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    let shown = format!("{loaded:?}");
    assert!(
        !shown.contains("kc-xai") && !shown.contains("env-openai"),
        "{shown}"
    );
}

/// A keychain the config asks for and the source cannot open is a warning,
/// as is a source that never opens one; in file mode the opener is not
/// called at all.
#[test]
fn an_unreachable_keychain_is_a_warning_and_the_file_mode_never_opens_one() {
    let keychain = with_config(
        "[security]\ncredential_store = \"keychain\"\n[providers]\nopenai_api_key = \"f\"\n",
    );
    let refuse = || Err::<Arc<dyn CredentialStore>, String>("no keychain here".to_string());
    let source = UserCredsSource {
        keychain: Some(&refuse),
        ..bare(keychain.path())
    };
    let loaded = load_user_creds(&source).unwrap();
    assert_eq!(cred(&loaded, "openai").api_key.as_deref(), Some("f"));
    assert_eq!(loaded.warnings.len(), 1);
    assert!(
        loaded.warnings[0].contains("no keychain here"),
        "{:?}",
        loaded.warnings
    );

    let loaded = load_user_creds(&bare(keychain.path())).unwrap();
    assert_eq!(loaded.warnings.len(), 1);
    assert!(
        loaded.warnings[0].contains("does not read"),
        "{:?}",
        loaded.warnings
    );

    let file = with_config("[providers]\nopenai_api_key = \"f\"\n");
    let never = || -> Result<Arc<dyn CredentialStore>, String> {
        panic!("the file backend opened the keychain")
    };
    let source = UserCredsSource {
        keychain: Some(&never),
        ..bare(file.path())
    };
    assert!(load_user_creds(&source).unwrap().warnings.is_empty());
}

/// Hosts, Bedrock's region and Ollama's address fall back to their variables
/// as the keys do, and a source with no environment reads none of them.
#[test]
fn hosts_regions_and_ollama_fall_back_to_the_environment() {
    let dir = with_config(
        "[providers]\nopenai_api_key = \"o\"\nbedrock_api_key = \"b\"\n\
         anthropic_api_key = \"a\"\nanthropic_base_url = \"https://file.example\"\n",
    );
    let env = env_of(&[
        ("OPENAI_BASE_URL", "https://gateway.example"),
        ("ANTHROPIC_BASE_URL", "https://env.example"),
        ("AWS_DEFAULT_REGION", " eu-west-2 "),
        ("OLLAMA_HOST", "http://box:11434"),
    ]);
    let source = UserCredsSource {
        env: Some(&env),
        ..bare(dir.path())
    };
    let loaded = load_user_creds(&source).unwrap();
    assert_eq!(
        cred(&loaded, "openai").base_url.as_deref(),
        Some("https://gateway.example")
    );
    assert_eq!(
        cred(&loaded, "anthropic").base_url.as_deref(),
        Some("https://file.example")
    );
    assert_eq!(cred(&loaded, "bedrock").options["region"], "eu-west-2");
    assert_eq!(
        cred(&loaded, "ollama").base_url.as_deref(),
        Some("http://box:11434")
    );

    // `AWS_REGION` first, and blank is no region.
    let env = env_of(&[("AWS_REGION", "us-west-2"), ("AWS_DEFAULT_REGION", "x")]);
    let source = UserCredsSource {
        env: Some(&env),
        ..bare(dir.path())
    };
    let loaded = load_user_creds(&source).unwrap();
    assert_eq!(cred(&loaded, "bedrock").options["region"], "us-west-2");
    let env = env_of(&[("AWS_REGION", "  ")]);
    let source = UserCredsSource {
        env: Some(&env),
        ..bare(dir.path())
    };
    let loaded = load_user_creds(&source).unwrap();
    assert!(!cred(&loaded, "bedrock").options.contains_key("region"));

    let loaded = load_user_creds(&bare(dir.path())).unwrap();
    assert_eq!(cred(&loaded, "openai").base_url, None);
    assert!(loaded.creds.iter().all(|c| c.name != "ollama"));

    // A region and an address in the file win over the environment's.
    let dir = with_config(
        "ollama_base_url = \"http://file:11434\"\n\
         [providers]\nbedrock_api_key = \"b\"\nbedrock_region = \"us-east-2\"\n",
    );
    let env = env_of(&[("AWS_REGION", "eu-west-1"), ("OLLAMA_HOST", "http://env:1")]);
    let source = UserCredsSource {
        env: Some(&env),
        ..bare(dir.path())
    };
    let loaded = load_user_creds(&source).unwrap();
    assert_eq!(cred(&loaded, "bedrock").options["region"], "us-east-2");
    assert_eq!(
        cred(&loaded, "ollama").base_url.as_deref(),
        Some("http://file:11434")
    );
}

/// Every per-provider setting reaches the provider it names: cache lifetime,
/// region, headers, rate limits, capability overrides and the timeout.
#[test]
fn per_provider_settings_travel_with_the_creds() {
    let dir = with_config(
        "request_timeout_secs = 42\n\
         [model_capabilities.my-model]\nmax_output_tokens = 99\n\
         [rate_limits.anthropic]\nrequests_per_minute = 5\ntokens_per_minute = 9\n\
         [providers]\nanthropic_api_key = \" a \"\nanthropic_cache_ttl = \"1h\"\n\
         bedrock_api_key = \"b\"\nbedrock_region = \"ap-south-1\"\n\
         meta_api_key = \"m\"\nmeta_base_url = \"  \"\n\
         [providers.meta_headers]\nX-Tenant = \"t\"\n",
    );
    let loaded = load_user_creds(&bare(dir.path())).unwrap();
    let anthropic = cred(&loaded, "anthropic");
    assert_eq!(anthropic.api_key.as_deref(), Some("a"));
    assert_eq!(anthropic.options["cache_ttl"], "1h");
    assert_eq!(anthropic.request_timeout_secs, Some(42));
    assert_eq!(
        anthropic.rate_limit.as_ref().unwrap().requests_per_minute,
        5
    );
    assert!(anthropic.model_capabilities.contains_key("my-model"));
    assert_eq!(cred(&loaded, "bedrock").options["region"], "ap-south-1");
    let meta = cred(&loaded, "meta");
    assert_eq!(meta.base_url, None, "blank is not a host");
    assert_eq!(
        meta.headers().unwrap(),
        vec![("X-Tenant".to_string(), "t".to_string())]
    );

    let dir = with_config("[providers]\nanthropic_api_key = \"a\"\nanthropic_cache_ttl = \"5m\"\n");
    let loaded = load_user_creds(&bare(dir.path())).unwrap();
    assert_eq!(cred(&loaded, "anthropic").options["cache_ttl"], "5m");
}

/// The sign-in transports and Ollama register on their switches, carrying
/// the grant location and the keychain flag.
#[test]
fn the_sign_in_transports_and_ollama_register_on_their_switches() {
    let dir = with_config(
        "[security]\ncredential_store = \"keychain\"\n\
         [providers]\nollama_enabled = true\ncodex_enabled = true\ngrok_enabled = true\n\
         codex_originator = \"o\"\ncodex_reasoning_effort = \"high\"\ncodex_verbosity = \"low\"\n\
         codex_replay_reasoning = false\nxai_base_url = \"https://x.example\"\n\
         [providers.xai_headers]\nX-A = \"1\"\n",
    );
    let store = || Ok(Arc::new(leviath_core::MemoryStore::new()) as Arc<dyn CredentialStore>);
    let source = UserCredsSource {
        keychain: Some(&store),
        ..bare(dir.path())
    };
    let loaded = load_user_creds(&source).unwrap();
    let names: Vec<&str> = loaded.creds.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["ollama", "codex", "grok"]);
    assert_eq!(
        cred(&loaded, "ollama").base_url.as_deref(),
        Some("http://localhost:11434")
    );
    let grants = loaded.grants_path.display().to_string();
    let codex = &cred(&loaded, "codex").options;
    assert_eq!(codex["auth_store_path"], grants);
    assert_eq!(codex["credential_store"], "keychain");
    assert_eq!(codex["originator"], "o");
    assert_eq!(codex["effort"], "high");
    assert_eq!(codex["verbosity"], "low");
    assert_eq!(codex["replay_reasoning"], "false");
    let grok = cred(&loaded, "grok");
    assert_eq!(grok.base_url.as_deref(), Some("https://x.example"));
    assert_eq!(grok.options["auth_store_path"], grants);
    assert_eq!(
        grok.headers().unwrap(),
        vec![("X-A".to_string(), "1".to_string())]
    );

    let dir = with_config("[providers]\ncodex_enabled = true\n");
    let loaded = load_user_creds(&bare(dir.path())).unwrap();
    let codex = &cred(&loaded, "codex").options;
    assert!(!codex.contains_key("credential_store"));
    assert_eq!(codex["replay_reasoning"], "true");
}

/// The defaults carry what a stage resolves against, a malformed fallback
/// entry is a warning, and the old `default_model` name loads as
/// `fallback_model` with a notice saying so. A renamed key in a table this
/// loader does not read (`[sandbox] persist`) is not its to report.
#[test]
fn the_defaults_carry_the_routing_preferences() {
    let dir = with_config(
        "default_provider = \"openai\"\noverride_model = \"o\"\ndefault_model = \"old\"\n\
         [model_capabilities.kept]\nretention = \"none\"\n\
         [model_capabilities.plain]\nmax_output_tokens = 1\n\
         [providers]\nprovider_order = [\"codex\", \"openai\"]\n\
         fallback_order = [\"openrouter/a/b\", \"bare\"]\n\
         zero_retention = true\nzero_retention_agreements = [\"openai\"]\nfile_uploads = false\n\
         [sandbox]\npersist = true\n",
    );
    let loaded = load_user_creds(&bare(dir.path())).unwrap();
    let defaults = &loaded.defaults;
    assert_eq!(defaults.provider, "openai");
    assert_eq!(defaults.override_model.as_deref(), Some("o"));
    assert_eq!(defaults.fallback_model.as_deref(), Some("old"));
    assert_eq!(defaults.provider_order, ["codex", "openai"]);
    assert_eq!(defaults.fallback_order.len(), 1);
    assert_eq!(defaults.fallback_order[0].model.to_string(), "a/b");
    assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
    assert!(
        loaded.warnings[0].contains("`default_model = \"old\"`")
            && loaded.warnings[0].contains("`fallback_model = \"old\"`"),
        "{:?}",
        loaded.warnings
    );
    assert!(
        loaded.warnings[1].contains("\"bare\""),
        "{:?}",
        loaded.warnings
    );
    assert!(defaults.retention.zero_requested);
    assert_eq!(defaults.retention.agreements, ["openai"]);
    assert!(!defaults.retention.file_uploads);
    assert_eq!(
        defaults
            .retention
            .model_overrides
            .keys()
            .collect::<Vec<_>>(),
        ["kept"]
    );

    // The current name wins when the file has both, and nothing was read
    // under another name, so there is nothing to say.
    let dir = with_config("fallback_model = \"new\"\ndefault_model = \"old\"\n");
    let loaded = load_user_creds(&bare(dir.path())).unwrap();
    assert_eq!(loaded.defaults.fallback_model.as_deref(), Some("new"));
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
}

/// Keys this loader does not read are ignored, so the CLI's full config
/// loads; `config_path` names a file outside the data directory.
#[test]
fn unknown_keys_are_ignored_and_the_config_path_can_be_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = dir.path().join("other.toml");
    std::fs::write(
        &elsewhere,
        "taint_tracking = true\n[[mcp_servers]]\nname = \"x\"\ncommand = \"y\"\n\
         [model_providers.groq]\nbase_url = \"https://groq.example\"\n\
         [providers]\nopenai_api_key = \"o\"\nsome_future_key = 1\n",
    )
    .unwrap();
    let source = UserCredsSource {
        config_path: Some(elsewhere),
        ..bare(dir.path())
    };
    let loaded = load_user_creds(&source).unwrap();
    let names: Vec<&str> = loaded.creds.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["openai"]);
}

/// A file that will not parse, or will not read, is an error naming it.
#[test]
fn an_unreadable_or_malformed_file_is_an_error() {
    use std::error::Error;

    let dir = with_config("[providers]\nopenai_api_key = 7\n");
    let e = load_user_creds(&bare(dir.path())).unwrap_err();
    assert!(matches!(e, UserCredsError::Parse { .. }), "{e:?}");
    assert!(e.to_string().starts_with("could not parse"), "{e}");
    assert!(e.to_string().contains("config.toml"), "{e}");
    assert!(e.source().is_some());

    // A config path under a directory that does not exist is the defaults,
    // as `Path::exists` says for `lev`.
    let dir = tempfile::tempdir().unwrap();
    let source = UserCredsSource {
        config_path: Some(dir.path().join("nowhere").join("config.toml")),
        ..bare(dir.path())
    };
    assert!(load_user_creds(&source).unwrap().creds.is_empty());

    // A directory where the file should be reads as an error, not as absent.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("config.toml")).unwrap();
    let e = load_user_creds(&bare(dir.path())).unwrap_err();
    assert!(matches!(e, UserCredsError::Read { .. }), "{e:?}");
    assert!(e.to_string().starts_with("could not read"), "{e}");
    assert!(e.source().is_some());
}

/// The real install's source: the data directory and config path from the
/// environment, the process environment, and the OS store, here a mock one
/// installed as the process default (no CI runner has an unlocked keychain,
/// and a developer's real one would prompt).
#[test]
fn the_system_source_reads_the_real_install() {
    assert!(UserCredsSource::system_at(None, None).is_none());

    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("elsewhere.toml");
    temp_env::with_vars(
        [
            ("LEVIATH_HOME", Some(home.path().as_os_str())),
            ("LEVIATH_CONFIG_PATH", Some(config.as_os_str())),
            ("LEVIATH_USER_CREDS_PROBE", Some("seen".as_ref())),
        ],
        || {
            let source = UserCredsSource::system().expect("a home");
            assert_eq!(source.data_dir, home.path().join(".leviath"));
            assert_eq!(source.config_path.as_deref(), Some(config.as_path()));
            let env = source.env.expect("the process environment");
            assert_eq!(env("LEVIATH_USER_CREDS_PROBE").as_deref(), Some("seen"));
            assert_eq!(env("LEVIATH_USER_CREDS_UNSET"), None);

            let _guard = with_mock_keychain();
            let open = source.keychain.expect("the OS store");
            let store = open().expect("the mock store answers the probe");
            assert_eq!(store.get("provider/none").unwrap(), None);
        },
    );
}

/// Install a fresh in-memory store as the process default, holding a lock for
/// the caller's lifetime so no other test swaps the default meanwhile: the
/// pattern `leviath-providers` and `leviath-cli` use for the same store.
/// Tolerates poisoning, since a test that panicked holding it has already
/// failed.
fn with_mock_keychain() -> std::sync::MutexGuard<'static, ()> {
    static STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = STORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    keyring_core::set_default_store(keyring_core::mock::Store::new().expect("mock store"));
    guard
}

/// A `[model_providers]` endpoint is a provider under its own name, built
/// with the constructor its kind names, and listed by name between Ollama
/// and the sign-in transports. A script entry is not a provider here.
#[test]
fn endpoints_register_under_their_own_names_in_name_order() {
    let dir = with_config(
        r#"
ollama_base_url = "http://o"
request_timeout_secs = 9

[model_capabilities.m]
max_output_tokens = 1

[providers]
openai_api_key = "k"
codex_enabled = true
grok_enabled = true

[model_providers.zeta]
kind = "openai-compatible"
base_url = " http://z/v1 "
api_key = " zk "
models = ["m1"]
serves = ["s1"]
rate_limit = { requests_per_minute = 5, tokens_per_minute = 6 }
headers = { X-Org = "r" }

[model_providers.alpha]
kind = "openai"
base_url = "https://a/v1"
api_key = "ak"
auth_header = "api-key"
serves = ["dep"]
models = ["dep2"]

[model_providers.keyless]
kind = "openai-compatible"
base_url = "http://l/v1"

[model_providers.scripted]
script = "s.rhai"
base_url = "http://ignored"

[rate_limits.zeta]
requests_per_minute = 99
tokens_per_minute = 99
"#,
    );
    let loaded = load_user_creds(&bare(dir.path())).unwrap();
    let names: Vec<&str> = loaded.creds.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "openai", "ollama", "alpha", "keyless", "zeta", "codex", "grok"
        ]
    );
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);

    // What every endpoint carries from the top of the file.
    let shared = |mut cred: ProviderCreds| {
        cred.model_capabilities = cred_caps(&loaded);
        cred.request_timeout_secs = Some(9);
        cred
    };
    // OpenAI's own API at another host: `models` routes there as `serves`
    // does, and the key goes in the header the entry names.
    assert_eq!(
        cred(&loaded, "alpha"),
        &shared(
            ProviderCreds::openai_host(
                "alpha",
                "https://a/v1",
                "ak".to_string(),
                Vec::new(),
                vec!["dep".to_string(), "dep2".to_string()],
            )
            .with_auth_header(Some("api-key".to_string()))
        )
    );
    // A key is optional for an OpenAI-compatible server.
    assert_eq!(
        cred(&loaded, "keyless"),
        &shared(ProviderCreds::openai_compatible(
            "keyless",
            "http://l/v1",
            None,
            Vec::new(),
            None,
            Vec::new(),
        ))
    );
    // Trimmed address and key, the entry's own headers and lists, and the
    // entry's own rate limit rather than a `[rate_limits]` row of its name.
    let mut zeta = shared(ProviderCreds::openai_compatible(
        "zeta",
        "http://z/v1",
        Some("zk".to_string()),
        vec![("X-Org".to_string(), "r".to_string())],
        Some(vec!["m1".to_string()]),
        vec!["s1".to_string()],
    ));
    zeta.rate_limit = Some(leviath_providers::RateLimitConfig {
        requests_per_minute: 5,
        tokens_per_minute: 6,
    });
    assert_eq!(cred(&loaded, "zeta"), &zeta);
}

/// The capability overrides every provider in `loaded` was handed.
fn cred_caps(loaded: &UserCreds) -> HashMap<String, leviath_providers::ModelCapabilityOverride> {
    loaded.creds[0].model_capabilities.clone()
}

/// Settings built by hand skip what a file would have been refused for: an
/// endpoint with no address, and OpenAI's API with no key. With no grants
/// path the sign-in providers are still listed, carrying no
/// `auth_store_path`, which is how the registry knows to skip them.
#[test]
fn hand_built_settings_skip_endpoints_that_cannot_work() {
    let endpoint = |kind, base_url: Option<&str>, api_key: Option<&str>| ModelProviderConfig {
        kind: Some(kind),
        base_url: base_url.map(str::to_string),
        api_key: api_key.map(str::to_string),
        ..Default::default()
    };
    let settings = UserSettings {
        providers: ProviderConfig {
            codex_enabled: true,
            grok_enabled: true,
            ..Default::default()
        },
        model_providers: HashMap::from([
            (
                "nowhere".to_string(),
                endpoint(ModelProviderKind::OpenaiCompatible, Some("  "), None),
            ),
            (
                "unaddressed".to_string(),
                endpoint(ModelProviderKind::OpenaiCompatible, None, Some("k")),
            ),
            (
                "keyless".to_string(),
                endpoint(ModelProviderKind::Openai, Some("https://h/v1"), Some(" ")),
            ),
            (
                "works".to_string(),
                endpoint(ModelProviderKind::Openai, Some("https://h/v1"), Some("k")),
            ),
        ]),
        ..Default::default()
    };
    let creds = provider_creds(&settings, None);
    let names: Vec<&str> = creds.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["works", "codex", "grok"]);
    assert!(!creds[1].options.contains_key("auth_store_path"));
    assert!(!creds[2].options.contains_key("auth_store_path"));
}

/// A credential check builds a sign-in provider with the options a run
/// gives it: Codex's own, Grok's, and nothing for any other name.
#[test]
fn signin_options_are_the_ones_a_run_builds_the_provider_with() {
    let settings = UserSettings {
        providers: ProviderConfig {
            codex_enabled: true,
            grok_enabled: true,
            codex_originator: Some("o".to_string()),
            ..Default::default()
        },
        security: SecuritySettings {
            credential_store: CredentialStoreKind::Keychain,
        },
        ..Default::default()
    };
    let grants = std::path::Path::new("/data/provider-auth.json");
    let creds = provider_creds(&settings, Some(grants));
    let options = |name: &str| &creds.iter().find(|c| c.name == name).unwrap().options;

    let codex = signin_options(&settings, Some(grants), "codex");
    assert_eq!(&codex, options("codex"));
    assert_eq!(codex["originator"], "o");
    assert_eq!(codex["auth_store_path"], "/data/provider-auth.json");
    let grok = signin_options(&settings, Some(grants), "grok");
    assert_eq!(&grok, options("grok"));
    assert_eq!(grok["credential_store"], "keychain");
    assert!(!grok.contains_key("replay_reasoning"));
    assert!(signin_options(&settings, Some(grants), "openai").is_empty());
}

/// The retention words on `[model_providers]` entries reach the defaults,
/// from script entries as well as endpoints. A word that is not a retention,
/// and a request field named after a provider that has none, are warnings in
/// name order; a value that is not text at all is some script's own key.
#[test]
fn retention_words_on_model_providers_reach_the_defaults() {
    use leviath_providers::retention::Retention;

    let dir = with_config(
        r#"
[model_providers.azure]
kind = "openai-compatible"
base_url = "https://r.openai.azure.com/openai/v1"
retention = "zero"
zero_retention_request = "openai"

[model_providers.scripted]
script = "s.rhai"
retention = "30d"

[model_providers.b-typo]
script = "t.rhai"
retention = "sometimes"
zero_retention_request = "nobody"

[model_providers.a-numeric]
script = "n.rhai"
retention = 5
zero_retention_request = true
"#,
    );
    let loaded = load_user_creds(&bare(dir.path())).unwrap();
    let retention = &loaded.defaults.retention;
    assert_eq!(
        retention.provider_declarations,
        HashMap::from([
            ("azure".to_string(), Retention::Zero),
            ("scripted".to_string(), Retention::Days(30)),
        ])
    );
    assert_eq!(
        retention.request_knob_aliases,
        HashMap::from([("azure".to_string(), "openai".to_string())])
    );
    assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
    assert!(
        loaded.warnings[0].starts_with("ignoring [model_providers.b-typo] retention: "),
        "{:?}",
        loaded.warnings
    );
    assert!(
        loaded.warnings[1]
            .starts_with("ignoring [model_providers.b-typo] zero_retention_request = \"nobody\""),
        "{:?}",
        loaded.warnings
    );

    // The same answer without the defaults around it, for a caller that
    // wants only this.
    let mut warnings = Vec::new();
    let settings: UserSettings =
        toml::from_str("[model_providers.x]\nretention = \"zero\"\n").unwrap();
    assert_eq!(
        retention_settings(&settings, &mut warnings).provider_declarations,
        HashMap::from([("x".to_string(), Retention::Zero)])
    );
    assert!(warnings.is_empty());
}

/// An entry that cannot work fails the load as it does for `lev`, naming the
/// table; with two of them it is always the first by name.
#[test]
fn a_model_provider_entry_that_cannot_work_is_an_error() {
    use std::error::Error;

    let dir = with_config(
        "[model_providers.b]\nkind = \"openai\"\nbase_url = \"https://h/v1\"\n\
         [model_providers.a]\nkind = \"openai-compatible\"\n\
         [providers]\nopenai_api_key = \"k\"\n",
    );
    let e = load_user_creds(&bare(dir.path())).unwrap_err();
    let UserCredsError::Invalid { path, message } = &e else {
        panic!("{e:?}");
    };
    assert_eq!(path, &dir.path().join("config.toml"));
    assert!(message.starts_with("[model_providers.a] "), "{message}");
    assert!(
        e.to_string().contains("config.toml cannot be used: "),
        "{e}"
    );
    assert!(e.source().is_none());

    // A kind nothing implements is refused by the parse, before any of that.
    let dir = with_config("[model_providers.x]\nkind = \"vllm\"\nbase_url = \"http://h\"\n");
    let e = load_user_creds(&bare(dir.path())).unwrap_err();
    assert!(matches!(e, UserCredsError::Parse { .. }), "{e:?}");
}

/// [`env_names`] is every variable the fill asks for and no other, so a host
/// that lists them, or a test that clears them, covers exactly what counts.
#[test]
fn env_names_are_the_variables_the_fill_reads() {
    let asked = std::sync::Mutex::new(Vec::new());
    let recording = |name: &str| {
        asked.lock().unwrap().push(name.to_string());
        None
    };
    UserSettings::default().fill_from_env(&recording, &mut Vec::new());
    let mut asked = asked.into_inner().unwrap();
    asked.sort_unstable();
    let mut names = env_names();
    names.sort_unstable();
    assert_eq!(asked, names);
    assert!(names.contains(&"AWS_DEFAULT_REGION") && names.contains(&"OLLAMA_HOST"));
}

/// `AWS_REGION` is asked before `AWS_DEFAULT_REGION`, as AWS's tooling does.
#[test]
fn the_region_comes_from_aws_region_first() {
    let mut settings = UserSettings::default();
    settings.fill_from_env(
        &env_of(&[
            ("AWS_REGION", "us-west-2"),
            ("AWS_DEFAULT_REGION", "eu-west-1"),
        ]),
        &mut Vec::new(),
    );
    assert_eq!(
        settings.providers.bedrock_region.as_deref(),
        Some("us-west-2")
    );
}

/// The fill says what it took from the environment, and that can be taken
/// back out: a host that saves the settings does not write a key the user
/// keeps in their environment. A slot changed since the fill is the host's
/// own value and stays, and a slot the file had filled was never the
/// environment's.
#[test]
fn what_the_environment_filled_can_be_taken_back_out() {
    let env = env_of(&[
        ("ANTHROPIC_API_KEY", "ant-env"),
        ("OPENAI_API_KEY", "openai-env"),
        ("OPENROUTER_API_KEY", "or-env"),
        ("GOOGLE_API_KEY", "google-env"),
        ("ANTHROPIC_BASE_URL", "https://ant.gateway"),
        ("OPENAI_BASE_URL", "https://openai.gateway"),
        ("AWS_REGION", "eu-west-1"),
        ("OLLAMA_HOST", "http://ollama.box"),
    ]);
    let mut settings = UserSettings::default();
    settings.providers.google_api_key = Some("google-file".to_string());
    let fill = settings.fill_from_env(&env, &mut Vec::new());
    assert_eq!(
        fill.keys().collect::<Vec<_>>(),
        ["anthropic", "openai", "openrouter"]
    );
    let rendered = format!("{fill:?}");
    assert!(rendered.contains("\"openrouter\""), "{rendered}");
    assert!(
        !rendered.contains("-env") && !rendered.contains("gateway") && !rendered.contains("box"),
        "a value was printed: {rendered}"
    );

    // Changed since the fill: these are whoever is saving's own.
    settings.providers.openai_api_key = Some("openai-typed".to_string());
    settings.providers.openai_base_url = Some("https://typed.gateway".to_string());
    let mut stored = settings.clone();
    stored.without_env_fill(&fill);
    assert_eq!(stored.providers.anthropic_api_key, None);
    assert_eq!(stored.openrouter_api_key, None);
    assert_eq!(stored.providers.anthropic_base_url, None);
    assert_eq!(stored.providers.bedrock_region, None);
    assert_eq!(stored.ollama_base_url, None);
    assert_eq!(
        stored.providers.openai_api_key.as_deref(),
        Some("openai-typed")
    );
    assert_eq!(
        stored.providers.openai_base_url.as_deref(),
        Some("https://typed.gateway")
    );
    assert_eq!(
        stored.providers.google_api_key.as_deref(),
        Some("google-file")
    );

    // The region and the address, changed since the fill, stay too.
    let mut stored = settings.clone();
    stored.providers.bedrock_region = Some("ap-south-1".to_string());
    stored.ollama_base_url = Some("http://typed".to_string());
    stored.without_env_fill(&fill);
    assert_eq!(
        stored.providers.bedrock_region.as_deref(),
        Some("ap-south-1")
    );
    assert_eq!(stored.ollama_base_url.as_deref(), Some("http://typed"));

    // Nothing filled is nothing to take out: settings the environment never
    // touched come through as they were, unset fields included.
    let mut untouched = UserSettings::default();
    let nothing = untouched.fill_from_env(&env_of(&[]), &mut Vec::new());
    assert_eq!(nothing.keys().count(), 0);
    untouched.without_env_fill(&nothing);
    assert_eq!(untouched.providers.bedrock_region, None);
    assert_eq!(untouched.ollama_base_url, None);
}

/// Each keyed provider's extra headers are reached by its table entry, so a
/// host that walks the table to scrub or redact them misses none.
#[test]
fn every_keyed_providers_headers_are_reached_through_the_table() {
    let text: String = std::iter::once("[providers]\n".to_string())
        .chain(
            KEYED_PROVIDERS
                .iter()
                .map(|k| format!("{p}_headers = {{ h = \"hdr-{p}\" }}\n", p = k.provider)),
        )
        .collect();
    let settings: UserSettings = toml::from_str(&text).unwrap();
    for keyed in KEYED_PROVIDERS {
        assert_eq!(
            settings.headers(keyed).get("h").map(String::as_str),
            Some(format!("hdr-{}", keyed.provider).as_str()),
            "{}",
            keyed.provider
        );
    }
}
