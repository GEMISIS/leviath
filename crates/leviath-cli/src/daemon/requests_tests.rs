use super::*;

const MANIFEST: &str = r#"[blueprint]
name = "a"
version = "0.1.0"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-4-6" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#;

/// The blueprint name a request asks for.
fn name_of(source: &SpawnSource) -> String {
    match source {
        SpawnSource::Blueprint(reference) => reference.name.to_string(),
        SpawnSource::BlueprintFile(path) => path.to_string(),
        SpawnSource::Raw(_) => String::new(),
    }
}

/// An installed blueprint is named by its name, and a path into the
/// installed blueprints names the same one. Any other path is named by its
/// absolute directory, whether it names the manifest or the directory.
#[test]
fn a_blueprint_is_named_by_its_installed_name_or_its_directory() {
    crate::config::with_isolated_config_path("requests_blueprint_source", |home| {
        let installed = home.join(".leviath").join("agents").join("coder");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("agent.toml"), MANIFEST).unwrap();
        assert_eq!(name_of(&blueprint_source("coder").unwrap()), "coder");
        assert_eq!(
            name_of(&blueprint_source(&installed.to_string_lossy()).unwrap()),
            "coder"
        );

        let elsewhere = tempfile::tempdir().unwrap();
        let manifest = elsewhere.path().join("agent.toml");
        std::fs::write(&manifest, MANIFEST).unwrap();
        let dir = std::fs::canonicalize(elsewhere.path()).unwrap();
        for asked in [&manifest, &elsewhere.path().to_path_buf()] {
            assert_eq!(
                name_of(&blueprint_source(&asked.to_string_lossy()).unwrap()),
                dir.to_string_lossy()
            );
        }

        assert!(blueprint_source("/no/such/blueprint").is_err());
    });
}

/// An installed blueprint whose directory name cannot be a blueprint name
/// (one longer than a name may be) is refused, naming it.
#[test]
fn an_installed_directory_that_cannot_be_a_name_is_refused() {
    crate::config::with_isolated_config_path("requests_long_name", |home| {
        let long = "a".repeat(130);
        let installed = home.join(".leviath").join("agents").join(&long);
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("agent.toml"), MANIFEST).unwrap();
        let asked = installed.to_string_lossy().into_owned();
        let err = blueprint_source(&asked).unwrap_err();
        assert!(err.contains(&long), "{err}");
    });
}

/// Every flag lands where the request carries it.
#[test]
fn every_flag_reaches_the_request() {
    let source = SpawnSource::Blueprint(BlueprintRef::parse("coder").unwrap());
    let part =
        leviath_core::mime::InboundPart::from_bytes("hero.png", b"\x89PNG\r\n\x1a\nx".to_vec())
            .in_region("art");
    let request = TaskLaunch {
        blueprint: "coder".to_string(),
        task: "do it".to_string(),
        regions: HashMap::from([("notes".to_string(), "be brief".to_string())]),
        parts: vec![part],
        model: Some("anthropic/m".to_string()),
        workdir: Some("relative/dir".to_string()),
        unattended: true,
        profile: None,
        allow: vec!["write_file".to_string()],
        max_depth: Some(1000),
        no_seed_commands: true,
        output: Some(leviath_core::output::OutputSpec::default()),
        capture_model_input: true,
        metadata: HashMap::from([("k".to_string(), "v".to_string())]),
        callback_url: Some("https://example.com/hook".to_string()),
        callback_secret: Some("s".to_string()),
    }
    .into_request_for(source)
    .unwrap();

    assert_eq!(request.inputs["task"], RawInput::Text("do it".to_string()));
    assert_eq!(
        request.inputs["notes"],
        RawInput::Text("be brief".to_string())
    );
    assert_eq!(request.attachments.len(), 1);
    assert_eq!(
        request.attachments[0].region.as_ref().map(|r| r.as_str()),
        Some("art")
    );
    assert_eq!(request.model.unwrap().to_string(), "anthropic/m");
    assert!(request.output.is_some());
    let workdir = request.workdir.unwrap();
    assert!(workdir.is_absolute() && workdir.ends_with("relative/dir"));
    assert_eq!(request.launch.unattended, Unattended::All);
    assert_eq!(request.launch.allow[0].as_str(), "write_file");
    assert_eq!(
        request.launch.max_depth,
        Some(u8::MAX),
        "clamped, not wrapped"
    );
    assert!(!request.launch.seed_commands);
    assert!(request.launch.capture_model_input);
    assert_eq!(request.delivery.metadata["k"], "v");
    let callback = request.delivery.callback.unwrap();
    assert_eq!(callback.url.as_str(), "https://example.com/hook");
    assert!(callback.secret.is_some());
}

/// A blank task, a blank model and no workdir are left out rather than sent
/// empty; a profile names the unattended mode, and an attended run ignores
/// one.
#[test]
fn blanks_are_left_out_and_a_profile_names_the_mode() {
    let source = SpawnSource::Blueprint(BlueprintRef::parse("coder").unwrap());
    // Absolute on every host: `/abs` is not, on Windows, and gains a drive.
    let abs = std::env::temp_dir().join("abs");
    let request = TaskLaunch {
        task: "  ".to_string(),
        model: Some(" ".to_string()),
        workdir: Some(abs.to_string_lossy().into_owned()),
        unattended: true,
        profile: Some("careful".to_string()),
        ..TaskLaunch::default()
    }
    .into_request_for(source.clone())
    .unwrap();
    assert!(request.inputs.is_empty());
    assert!(request.model.is_none());
    assert_eq!(request.workdir.as_deref(), Some(abs.as_path()));
    assert!(matches!(request.launch.unattended, Unattended::Profile(p) if p.as_str() == "careful"));
    assert!(request.launch.seed_commands);

    // An empty profile is no profile, and an attended run ignores one.
    let all = TaskLaunch {
        unattended: true,
        profile: Some(String::new()),
        ..TaskLaunch::default()
    }
    .into_request_for(source.clone())
    .unwrap();
    assert_eq!(all.launch.unattended, Unattended::All);
    let off = TaskLaunch {
        profile: Some("careful".to_string()),
        ..TaskLaunch::default()
    }
    .into_request_for(source)
    .unwrap();
    assert_eq!(off.launch.unattended, Unattended::Off);
    assert!(off.workdir.is_none());
}

/// A flag that does not read is refused, naming the flag.
#[test]
fn a_flag_that_does_not_read_is_refused_by_name() {
    let source = || SpawnSource::Blueprint(BlueprintRef::parse("coder").unwrap());
    let refused = |launch: TaskLaunch| launch.into_request_for(source()).unwrap_err();
    assert!(
        refused(TaskLaunch {
            model: Some("bad model".to_string()),
            ..TaskLaunch::default()
        })
        .contains("model 'bad model'")
    );
    assert!(
        refused(TaskLaunch {
            unattended: true,
            profile: Some("bad\nprofile".to_string()),
            ..TaskLaunch::default()
        })
        .contains("yolo profile 'bad\nprofile'")
    );
    assert!(
        refused(TaskLaunch {
            allow: vec!["bad tool".to_string()],
            ..TaskLaunch::default()
        })
        .contains("allow 'bad tool'")
    );
    assert!(
        refused(TaskLaunch {
            callback_url: Some("not a url".to_string()),
            ..TaskLaunch::default()
        })
        .contains("callback url 'not a url'")
    );
    let bad_output: leviath_core::output::OutputSpec = serde_json::from_value(serde_json::json!({
        "artifacts": [{ "name": "a", "type": "not a mime type" }]
    }))
    .unwrap();
    assert!(
        TaskLaunch {
            output: Some(bad_output),
            ..TaskLaunch::default()
        }
        .into_request_for(source())
        .is_err()
    );
    // The blueprint itself is read first.
    assert!(
        TaskLaunch {
            blueprint: "/no/such/blueprint".to_string(),
            ..TaskLaunch::default()
        }
        .into_request()
        .is_err()
    );
}
