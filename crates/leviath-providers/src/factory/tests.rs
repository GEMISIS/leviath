use super::*;
use crate::ProviderError;

fn client() -> Result<HttpClient> {
    Ok(crate::provider::build_http_client(None).expect("an HTTPS client builds in tests"))
}

fn no_client() -> Result<HttpClient> {
    Err(ProviderError::ClientBuild(
        "no certificate store".to_string(),
    ))
}

/// What `spec` builds, with a working client and an Ollama that answers.
fn built(spec: Spec) -> Option<Arc<dyn Provider>> {
    build(spec, &mut client, &|_| true).expect("builds")
}

fn keyed(kind: &str) -> Spec {
    let mut spec = Spec::new(kind);
    spec.api_key = Some("k".to_string());
    spec.base_url = Some("https://gw.example/v1".to_string());
    spec.request_timeout_secs = Some(30);
    spec.options
        .insert("effort".to_string(), "high".to_string());
    spec
}

fn signin(kind: &str, dir: &std::path::Path) -> Spec {
    let mut spec = Spec::new(kind);
    spec.options.insert(
        "auth_store_path".to_string(),
        dir.join("auth.json").display().to_string(),
    );
    spec
}

#[test]
fn every_keyed_provider_builds_with_a_key_and_registers_under_its_name() {
    for kind in [
        "anthropic",
        "openai",
        "google",
        "openrouter",
        "meshy",
        "bedrock",
        "xai",
        "meta",
    ] {
        let provider = built(keyed(kind)).unwrap_or_else(|| panic!("{kind} builds"));
        assert_eq!(provider.name(), kind);
    }
}

#[test]
fn a_keyed_provider_with_no_key_builds_nothing_and_asks_for_no_client() {
    for kind in ["anthropic", "openai", "bedrock", "xai"] {
        let built = build(Spec::new(kind), &mut no_client, &|_| true).expect("no client asked");
        assert!(built.is_none(), "{kind}");
    }
}

#[test]
fn a_provider_that_cannot_get_a_client_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let keyed_kinds = [
        "anthropic",
        "openai",
        "google",
        "openrouter",
        "meshy",
        "bedrock",
        "xai",
        "meta",
    ];
    for spec in keyed_kinds
        .into_iter()
        .map(keyed)
        .chain([signin("codex", dir.path()), signin("grok", dir.path())])
    {
        let kind = spec.kind.clone();
        let err = build(spec, &mut no_client, &|_| true)
            .err()
            .unwrap_or_else(|| panic!("{kind} needs a client"));
        assert!(matches!(err, ProviderError::ClientBuild(_)), "{kind}");
    }
    let mut ollama = Spec::new("ollama");
    ollama.base_url = Some("http://127.0.0.1:1".to_string());
    assert!(build(ollama, &mut no_client, &|_| true).is_err());
}

#[test]
fn a_kind_nobody_knows_builds_nothing() {
    assert!(built(keyed("nonsense")).is_none());
}

#[test]
fn a_kind_this_build_left_out_builds_nothing() {
    let built = build_in(keyed("bedrock"), &mut client, &|_| true, &["anthropic"]).expect("builds");
    assert!(built.is_none());
}

#[test]
fn the_anthropic_cache_ttl_is_read_and_a_bad_one_keeps_the_default() {
    for ttl in [Some("1h"), Some("5m"), Some("nonsense"), None] {
        let mut spec = keyed("anthropic");
        if let Some(ttl) = ttl {
            spec.options
                .insert("cache_ttl".to_string(), ttl.to_string());
        }
        assert!(built(spec).is_some(), "{ttl:?}");
    }
}

#[test]
fn a_second_openai_host_registers_under_its_own_name() {
    let mut spec = keyed("openai");
    spec.name = "azure".to_string();
    spec.auth_header = Some("api-key".to_string());
    spec.serves = vec!["my-deployment".to_string()];
    assert_eq!(built(spec).expect("builds").name(), "azure");
}

#[test]
fn bedrock_builds_with_or_without_a_region() {
    let mut spec = keyed("bedrock");
    spec.options
        .insert("region".to_string(), "eu-west-1".to_string());
    assert!(built(spec).is_some());
    assert!(built(keyed("bedrock")).is_some());
}

#[test]
fn ollama_registers_only_when_something_answers() {
    let answers = build(Spec::new("ollama"), &mut client, &|url| {
        assert_eq!(url, "http://localhost:11434", "the default address");
        true
    })
    .expect("builds");
    assert_eq!(answers.expect("registered").name(), "ollama");

    let mut spec = Spec::new("ollama");
    spec.base_url = Some("http://127.0.0.1:1".to_string());
    let silent = build(spec, &mut no_client, &|_| false).expect("no client asked");
    assert!(silent.is_none());
}

#[test]
fn the_sign_in_providers_build_before_anyone_signs_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut codex = signin("codex", dir.path());
    for (key, value) in [
        ("originator", "Codex Leviath"),
        ("effort", "xhigh"),
        ("verbosity", "high"),
        ("replay_reasoning", "false"),
    ] {
        codex.options.insert(key.to_string(), value.to_string());
    }
    let codex = built(codex).expect("codex builds");
    assert_eq!(codex.name(), "codex");
    // The catalog is not promised until a plan tier is known, which is the
    // observable consequence of the provider having been built at all.
    assert!(codex.served_catalog().is_none());

    let grok = built(signin("grok", dir.path())).expect("grok builds");
    assert_eq!(grok.name(), "grok");
}

#[test]
fn a_sign_in_provider_with_nowhere_to_keep_its_grant_is_skipped() {
    for kind in ["codex", "grok"] {
        let built = build(Spec::new(kind), &mut no_client, &|_| true).expect("no client asked");
        assert!(built.is_none(), "{kind}");
    }
}
