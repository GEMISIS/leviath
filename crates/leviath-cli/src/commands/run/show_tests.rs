//! `lev run show`: a run's spec, its state at a step, and its steps.

use crate::runstate::run_file::tests::{recorded, say, step};

use super::*;

fn args(run_id: &str) -> ShowArgs {
    ShowArgs {
        run_id: run_id.to_string(),
        ..ShowArgs::default()
    }
}

/// A run with three steps after its start.
fn three_steps() -> String {
    let dir = recorded(&runstate::runs_dir());
    step(&dir, 10, |s| say(s, "one"));
    step(&dir, 20, |s| say(s, "two"));
    step(&dir, 30, |s| say(s, "three"));
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

#[tokio::test]
async fn the_spec_a_state_and_the_steps_print_as_toml_or_json() {
    runstate::with_isolated_runs_dir_async("run-show", |_d| async move {
        let id = three_steps();
        let spec = render(&args(&id)).unwrap();
        assert!(spec.starts_with("[spec]"), "{spec}");
        let json: serde_json::Value = serde_json::from_str(
            &render(&ShowArgs {
                json: true,
                ..args(&id)
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(json["run_id"], id.as_str());

        let state = render(&ShowArgs {
            at: Some(2),
            toml: true,
            ..args(&id)
        })
        .unwrap();
        assert!(state.starts_with("[state]"), "{state}");
        assert!(
            state.contains("\"two\"") || state.contains("two"),
            "{state}"
        );
        assert!(!state.contains("three"), "{state}");
        let json: serde_json::Value = serde_json::from_str(
            &render(&ShowArgs {
                at: Some(0),
                json: true,
                ..args(&id)
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(json["seq"], 0);

        let deltas = render(&ShowArgs {
            deltas: Some("2..".to_string()),
            ..args(&id)
        })
        .unwrap();
        assert_eq!(deltas.matches("[[delta]]").count(), 2, "{deltas}");
        for (range, count) in [("..", 3), ("..1", 1), ("1..2", 2), (" 3 .. 3 ", 1)] {
            let json: serde_json::Value = serde_json::from_str(
                &render(&ShowArgs {
                    deltas: Some(range.to_string()),
                    json: true,
                    ..args(&id)
                })
                .unwrap(),
            )
            .unwrap();
            assert_eq!(json.as_array().unwrap().len(), count, "{range}");
        }
        execute(args(&id)).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn a_step_or_a_range_that_is_not_there_is_refused() {
    runstate::with_isolated_runs_dir_async("run-show-refusals", |_d| async move {
        let id = three_steps();
        let err = render(&ShowArgs {
            at: Some(9),
            ..args(&id)
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("no step 9: its last step is 3"),
            "{err}"
        );
        for (range, said) in [
            ("3", "not a range"),
            ("x..2", "'x' is not a step number"),
            ("1..y", "'y' is not a step number"),
            ("3..1", "runs backwards"),
        ] {
            let err = render(&ShowArgs {
                deltas: Some(range.to_string()),
                ..args(&id)
            })
            .unwrap_err();
            assert!(err.to_string().contains(said), "{range}: {err}");
        }
        let file = runstate::run_dir(&id).join(leviath_core::files::RUN_FILE);
        let mut bytes = std::fs::read(&file).unwrap();
        bytes.extend(
            leviath_runtime::runfile::codec::encode(
                leviath_runtime::runfile::codec::FrameKind::Delta,
                &4u64,
            )
            .unwrap(),
        );
        std::fs::write(&file, bytes).unwrap();
        for broken in [
            ShowArgs {
                deltas: Some("..".to_string()),
                ..args(&id)
            },
            ShowArgs {
                at: Some(4),
                ..args(&id)
            },
        ] {
            assert!(render(&broken).is_err());
        }
        let err = render(&args("ghost")).unwrap_err();
        assert!(
            err.to_string().contains("run 'ghost' has no run file"),
            "{err}"
        );
        assert!(execute(args("ghost")).await.is_err());
    })
    .await;
}

/// A run file that ends in a step a crash left half written still shows the
/// steps before it, and says the last one is missing rather than keeping
/// quiet about it. The file itself is left as it is.
#[tokio::test]
async fn a_torn_last_step_is_left_out_and_said_so() {
    runstate::with_isolated_runs_dir_async("run-show-torn", |_d| async move {
        let id = three_steps();
        let file = runstate::run_dir(&id).join(leviath_core::files::RUN_FILE);
        let mut bytes = std::fs::read(&file).unwrap();
        let whole = bytes.len();
        bytes.extend_from_slice(&[7, 0, 0]);
        std::fs::write(&file, &bytes).unwrap();
        let shown = render(&ShowArgs {
            deltas: Some("..".to_string()),
            json: true,
            ..args(&id)
        })
        .unwrap();
        let steps: Vec<serde_json::Value> = serde_json::from_str(&shown).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(std::fs::metadata(&file).unwrap().len() as usize, whole + 3);
    })
    .await;
    let note = torn_note("r-1", 3).expect("a torn tail is said");
    assert!(
        note.contains("run 'r-1'") && note.contains("3 bytes"),
        "{note}"
    );
    assert!(torn_note("r-1", 0).is_none());
}

/// Rewrite the run in `dir` with `edit` made to its spec, from the state it
/// started in.
fn respec(dir: &std::path::Path, edit: impl FnOnce(&mut leviath_runtime::spec::run_spec::RunSpec)) {
    use leviath_runtime::runfile::{CheckpointPolicy, RunFileWriter};
    let reader = runstate::run_file::open_in(dir).unwrap();
    let mut spec = reader.spec().clone();
    edit(&mut spec);
    let start = reader.state_at(0).unwrap();
    RunFileWriter::create(
        &runstate::run_file::path_in(dir),
        &spec,
        &leviath_runtime::spec::env::CodeFiles::new(),
        &start,
        CheckpointPolicy::default(),
    )
    .unwrap();
}

/// The webhook's signing secret is in the run file, so a resumed run can
/// sign, and is never printed: the REST and GraphQL spec and `lev rage` hide
/// it the same way.
#[tokio::test]
async fn the_webhook_secret_is_never_shown() {
    use leviath_runtime::spec::launch::{Callback, Secret};
    runstate::with_isolated_runs_dir_async("run-show-secret", |_d| async move {
        let dir = recorded(&runstate::runs_dir());
        respec(&dir, |spec| {
            spec.delivery.callback = Some(Callback {
                url: leviath_runtime::spec::names::HttpUrl::new("https://example.com/hook")
                    .unwrap(),
                secret: Some(Secret::new("hunter2-signing-key")),
            });
        });
        let id = dir.file_name().unwrap().to_string_lossy().into_owned();
        for json in [false, true] {
            let shown = render(&ShowArgs { json, ..args(&id) }).unwrap();
            assert!(!shown.contains("hunter2"), "{shown}");
            assert!(shown.contains("[redacted]"), "{shown}");
        }
        let kept = runstate::run_file::open_in(&dir).unwrap();
        let secret = kept
            .spec()
            .delivery
            .callback
            .as_ref()
            .unwrap()
            .secret
            .as_ref();
        assert_eq!(secret.map(Secret::expose), Some("hunter2-signing-key"));
    })
    .await;
}

/// A run is named by its id or by a start of it only one run's id has, the
/// way `lev rage` and `lev interactions` take one.
#[tokio::test]
async fn a_run_is_named_by_the_start_of_its_id() {
    runstate::with_isolated_runs_dir_async("run-show-prefix", |_d| async move {
        let first = three_steps();
        let second = three_steps();
        let unique = |id: &str, other: &str| {
            let n = id
                .chars()
                .zip(other.chars())
                .take_while(|(a, b)| a == b)
                .count();
            id.chars().take(n + 1).collect::<String>()
        };
        for (id, other) in [(&first, &second), (&second, &first)] {
            let shown = render(&args(&unique(id, other))).unwrap();
            assert!(shown.contains(id.as_str()), "{shown}");
        }
        let shared: String = first
            .chars()
            .zip(second.chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a)
            .collect();
        let err = render(&args(&shared)).unwrap_err().to_string();
        assert!(
            err.contains(first.as_str()) && err.contains(second.as_str()),
            "{err}"
        );
        let err = render(&args("nobody-")).unwrap_err().to_string();
        assert!(err.contains("run 'nobody-' has no run file"), "{err}");
    })
    .await;
}
