//! A run's webhook secret lives in the secret store beside the runs, never in
//! its run file: spawned there, read from there when the run comes back, and
//! a run whose secret has gone is held until it is put back.

use super::*;
use crate::config::Config;
use crate::daemon::starter::testing::{manifest_in, starter, task_request};
use crate::test_support::FakeProvider;
use leviath_runtime::ProviderRegistry;
use leviath_runtime::secret_store::SecretStore;
use leviath_runtime::spec::launch::{Callback, Secret};
use leviath_runtime::spec::names::HttpUrl;
use std::path::PathBuf;
use std::sync::Arc;

const SECRET: &str = "whsec-the-receivers-signing-key";

const WORKER: &str = r#"[blueprint]
name = "worker"
version = "0.0.0"
description = "One stage."

[graph]
entry = "work"
inputs = [{ name = "task", type = { kind = "text", multiline = true }, binds = [{ region = "task" }] }]

[graph.layout]
total_budget_tokens = 50000
regions = [
    { name = "task", kind = "pinned", budget = 2000 },
    { name = "conversation", kind = { kind = "sliding_window", max_items = 40 }, budget = 20000 },
]

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "m" }] }
system_prompt = "Work."
"#;

fn registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.register(
        "anthropic".to_string(),
        Arc::new(FakeProvider::new().replying("done").context_window(100_000)),
    );
    registry
}

/// A home with a runs directory in it, so the store beside the runs is the
/// test's own, and a blueprint to run.
struct Home {
    _dir: tempfile::TempDir,
    runs: PathBuf,
    manifest: PathBuf,
}

fn home() -> Home {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path().join("runs");
    std::fs::create_dir_all(&runs).unwrap();
    let manifest = manifest_in(dir.path(), WORKER);
    Home {
        _dir: dir,
        runs,
        manifest,
    }
}

/// Start a run with a signed webhook the way the daemon does, and leave it
/// on disk, loaded nowhere. Returns its id.
async fn signed_run(home: &Home) -> String {
    let starter = starter(Config::default(), registry(), &home.runs);
    let mut request = task_request(&home.manifest, "carry on");
    request.delivery.callback = Some(Callback {
        url: HttpUrl::new("https://example.com/hook").unwrap(),
        secret: Some(Secret::new(SECRET)),
    });
    let env = starter.env_for(&request, starter.config.current());
    let prepared = starter
        .start_with(env, request, leviath_runtime::spec::env::Caller::TopLevel)
        .await
        .expect("the run starts");
    prepared.spec.run_id.to_string()
}

/// Every file under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .flat_map(|e| match e.path().is_dir() {
            true => files_under(&e.path()),
            false => vec![e.path()],
        })
        .collect()
}

/// The secret is in the store and nowhere in the run's directory: not in
/// any frame of its run file, not in any file beside it. The run comes back
/// on a restart, and its webhook is signed with the secret it was given.
#[tokio::test]
async fn a_webhook_secret_is_kept_beside_the_runs_and_never_in_the_run_file() {
    let home = home();
    let run = signed_run(&home).await;
    let dir = home.runs.join(&run);
    let file = crate::runstate::run_file::path_in(&dir);
    assert!(!crate::test_support::run_file_holds(&file, SECRET));
    for path in files_under(&dir) {
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()));
    }
    let spec = crate::runstate::run_file::spec_in(&dir).unwrap();
    let reference = spec.delivery.callback_secret().unwrap();
    assert_eq!(reference.run(), Some(run.as_str()));
    let store = SecretStore::of_runs(&home.runs);
    assert_eq!(store.read(reference), Some(Secret::new(SECRET)));

    // A restart takes the run back, and its webhook signs with the secret.
    let starter = starter(Config::default(), registry(), &home.runs);
    let mut world = leviath_runtime::world::PipelineWorld::new(
        starter.providers.registry(),
        starter.tool_service.clone(),
        leviath_runtime::inference_pool::InferencePoolConfig::new(),
        1,
        Some(home.runs.clone()),
        tokio::runtime::Handle::current(),
    );
    let recovered = resume_all(&mut world, &starter, &home.runs);
    assert_eq!(recovered.reloaded.len(), 1);
    assert!(recovered.held.is_empty());
    let spec = crate::runstate::run_file::spec_in(&dir).unwrap();
    let secret = crate::runstate::run_file::callback_secret(&dir, &spec);
    assert_eq!(secret, Some(Ok(Secret::new(SECRET))));
}

/// A run whose secret has gone from the store is held on a restart, with
/// the issue at the secret, rather than coming back to post its webhook
/// unsigned. Put back, it resumes.
#[tokio::test]
async fn a_run_whose_secret_is_gone_is_held_until_it_is_back() {
    let home = home();
    let run = signed_run(&home).await;
    let dir = home.runs.join(&run);
    let store = SecretStore::of_runs(&home.runs);
    assert_eq!(store.forget_run(&run), 1);

    let starter = starter(Config::default(), registry(), &home.runs);
    let mut world = leviath_runtime::world::PipelineWorld::new(
        starter.providers.registry(),
        starter.tool_service.clone(),
        leviath_runtime::inference_pool::InferencePoolConfig::new(),
        1,
        Some(home.runs.clone()),
        tokio::runtime::Handle::current(),
    );
    let recovered = resume_all(&mut world, &starter, &home.runs);
    assert!(recovered.reloaded.is_empty());
    assert_eq!(recovered.held.len(), 1);
    let state =
        leviath_runtime::runfile::RunFileReader::open(&crate::runstate::run_file::path_in(&dir))
            .unwrap()
            .latest_state()
            .unwrap();
    let held = state.held.expect("the run is held");
    assert_eq!(held.0[0].path.to_string(), "delivery.callback.signed_with");
    assert_eq!(
        held.0[0].code,
        leviath_runtime::spec::issues::IssueCode::Unavailable
    );

    // The secret put back, the next start takes the run back.
    let spec = crate::runstate::run_file::spec_in(&dir).unwrap();
    store
        .keep(
            spec.delivery.callback_secret().unwrap(),
            &Secret::new(SECRET),
        )
        .unwrap();
    let mut world = leviath_runtime::world::PipelineWorld::new(
        starter.providers.registry(),
        starter.tool_service.clone(),
        leviath_runtime::inference_pool::InferencePoolConfig::new(),
        1,
        Some(home.runs.clone()),
        tokio::runtime::Handle::current(),
    );
    assert_eq!(
        resume_all(&mut world, &starter, &home.runs).reloaded.len(),
        1
    );
}

/// Deleting a run forgets its secret (every delete forgets them as this
/// does before it removes the directory); a secret whose run directory went
/// some other way is swept at the next start.
#[tokio::test]
async fn deleting_a_run_forgets_its_secret() {
    crate::runstate::with_isolated_runs_dir_async("secret-delete", |_d| async move {
        let runs = crate::runstate::runs_dir();
        let home = Home {
            manifest: manifest_in(runs.parent().unwrap(), WORKER),
            runs: runs.clone(),
            _dir: tempfile::tempdir().unwrap(),
        };
        let store = SecretStore::of_runs(&runs);
        let deleted = signed_run(&home).await;
        crate::runstate::forget_secrets(&deleted);
        std::fs::remove_dir_all(runs.join(&deleted)).unwrap();
        assert!(store.secrets().is_empty());

        let removed_by_hand = signed_run(&home).await;
        std::fs::remove_dir_all(runs.join(&removed_by_hand)).unwrap();
        assert_eq!(store.secrets().len(), 1);
        assert_eq!(store.sweep(&runs), 1);
        assert!(store.secrets().is_empty());
    })
    .await;
}
