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
