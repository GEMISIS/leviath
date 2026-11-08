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
/// entry is a warning, and the old `default_model` name still loads.
#[test]
fn the_defaults_carry_the_routing_preferences() {
    let dir = with_config(
        "default_provider = \"openai\"\noverride_model = \"o\"\ndefault_model = \"old\"\n\
         [model_capabilities.kept]\nretention = \"none\"\n\
         [model_capabilities.plain]\nmax_output_tokens = 1\n\
         [providers]\nprovider_order = [\"codex\", \"openai\"]\n\
         fallback_order = [\"openrouter/a/b\", \"bare\"]\n\
         zero_retention = true\nzero_retention_agreements = [\"openai\"]\nfile_uploads = false\n",
    );
    let loaded = load_user_creds(&bare(dir.path())).unwrap();
    let defaults = &loaded.defaults;
    assert_eq!(defaults.provider, "openai");
    assert_eq!(defaults.override_model.as_deref(), Some("o"));
    assert_eq!(defaults.fallback_model.as_deref(), Some("old"));
    assert_eq!(defaults.provider_order, ["codex", "openai"]);
    assert_eq!(defaults.fallback_order.len(), 1);
    assert_eq!(defaults.fallback_order[0].model.to_string(), "a/b");
    assert_eq!(loaded.warnings.len(), 1);
    assert!(
        loaded.warnings[0].contains("\"bare\""),
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

    let dir = with_config("fallback_model = \"new\"\ndefault_model = \"old\"\n");
    let loaded = load_user_creds(&bare(dir.path())).unwrap();
    assert_eq!(loaded.defaults.fallback_model.as_deref(), Some("new"));
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
