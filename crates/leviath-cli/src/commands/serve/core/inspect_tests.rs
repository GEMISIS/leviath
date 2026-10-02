//! Reading one run off its file: spec, state, steps, graph, window, ledger
//! and parts.

use leviath_runtime::control_socket::{ControlRequest, ControlResponse};
use leviath_runtime::runfile::{CheckpointPolicy, RunFileReader, RunFileWriter};
use leviath_runtime::spec::env::CodeFiles;
use leviath_runtime::spec::launch::{Callback, Secret};
use leviath_runtime::spec::names::{Digest, HttpUrl};
use leviath_runtime::state::context::{BlobState, PartBody, PartState};
use leviath_runtime::state::{
    EntryKind, EntryMeta, EntryState, TransitionReason, TransitionRecord,
};

use super::super::run_file::tests::{garbage, recorded, step};
use super::*;
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client, state_with_agent_paths};

/// A state that reaches no daemon.
fn offline() -> AppState {
    let mut state = state_with_agent_paths(Vec::new());
    state.control = no_daemon_client();
    state
}

/// Rewrite `run_id`'s file with `edit` made to its spec, from the state it
/// started in.
fn respec(run_id: &str, edit: impl FnOnce(&mut RunSpec)) {
    let path = run_file::path(run_id);
    let reader = RunFileReader::open(&path).unwrap();
    let mut spec = reader.spec().clone();
    edit(&mut spec);
    let start = reader.state_at(0).unwrap();
    RunFileWriter::create(
        &path,
        &spec,
        &CodeFiles::new(),
        &start,
        CheckpointPolicy::default(),
    )
    .unwrap();
}

#[tokio::test]
async fn a_spec_is_served_with_its_webhook_secret_hidden() {
    crate::runstate::with_isolated_runs_dir_async("inspect-spec", |_d| async move {
        let run_id = recorded();
        assert!(spec(&run_id).unwrap().delivery.callback.is_none());
        respec(&run_id, |spec| {
            spec.delivery.callback = Some(Callback {
                url: HttpUrl::new("https://example.com/hook").unwrap(),
                secret: Some(Secret::new("hunter2")),
            });
        });
        let served = spec(&run_id).unwrap();
        let callback = served.delivery.callback.unwrap();
        assert_eq!(callback.secret.unwrap().expose(), "[redacted]");
        assert_eq!(callback.url.as_str(), "https://example.com/hook");

        // A webhook with no secret has none to hide.
        respec(&run_id, |spec| {
            spec.delivery.callback = Some(Callback {
                url: HttpUrl::new("https://example.com/hook").unwrap(),
                secret: None,
            });
        });
        let plain = spec(&run_id).unwrap().delivery.callback.unwrap();
        assert!(plain.secret.is_none());

        assert_eq!(spec("ghost").unwrap_err().code(), "NOT_FOUND");
    })
    .await;
}

#[tokio::test]
async fn a_state_is_read_at_any_step_or_live_from_the_daemon() {
    crate::runstate::with_isolated_runs_dir_async("inspect-state", |_d| async move {
        let run_id = recorded();
        step(&run_id, 10, Vec::new(), |s| s.title = Some("first".into()));
        step(&run_id, 20, Vec::new(), |s| s.title = Some("second".into()));
        let app = offline();

        let at_one = state(&app, &run_id, Some(1)).await.unwrap();
        assert_eq!(at_one.title.as_deref(), Some("first"));
        let past = state(&app, &run_id, Some(9)).await.unwrap_err();
        assert_eq!(past.code(), "RANGE_NOT_SATISFIABLE");
        assert!(past.to_string().contains("last step is 2"), "{past}");

        // No daemon: the file's last step.
        let now = state(&app, &run_id, None).await.unwrap();
        assert_eq!(now.title.as_deref(), Some("second"));

        // A daemon holding the run answers for it.
        let mut live = now.clone();
        live.title = Some("live".into());
        let reply = live.clone();
        let (client, _dir, _task) = fake_daemon(move |req| match req {
            ControlRequest::Inspect { run_id } => {
                assert!(!run_id.is_empty());
                ControlResponse::State {
                    state: Box::new(reply.clone()),
                }
            }
            other => panic!("unexpected {other:?}"),
        });
        let mut app = offline();
        app.control = client;
        let got = state(&app, &run_id, None).await.unwrap();
        assert_eq!(got.title.as_deref(), Some("live"));

        // A daemon that does not hold it hands over to the file, and a run
        // with no file is a miss.
        let (client, _dir, _task) = fake_daemon(|_| ControlResponse::Error {
            message: "no run".into(),
        });
        let mut app = offline();
        app.control = client;
        assert_eq!(
            state(&app, "ghost", None).await.unwrap_err().code(),
            "NOT_FOUND"
        );
        assert_eq!(
            state(&offline(), "ghost", Some(0))
                .await
                .unwrap_err()
                .code(),
            "NOT_FOUND"
        );
    })
    .await;
}

/// A file whose steps do not decode is an error on every read that walks
/// them, never a short answer.
#[tokio::test]
async fn a_file_whose_steps_do_not_decode_is_an_error() {
    crate::runstate::with_isolated_runs_dir_async("inspect-torn", |_d| async move {
        let run_id = recorded();
        step(&run_id, 10, Vec::new(), |s| s.title = Some("t".into()));
        // Two spec frames: the second one is a file this build never wrote.
        let path = run_file::path(&run_id);
        let mut bytes = std::fs::read(&path).unwrap();
        let spec = RunFileReader::open(&path).unwrap().spec().clone();
        bytes.extend(
            leviath_runtime::runfile::codec::encode(
                leviath_runtime::runfile::codec::FrameKind::Spec,
                &spec,
            )
            .unwrap(),
        );
        garbage(&run_id, &bytes);
        assert_eq!(context(&run_id).unwrap_err().code(), "INTERNAL");

        // A state frame that is not a state.
        let run_id = recorded();
        let path = run_file::path(&run_id);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend(
            leviath_runtime::runfile::codec::encode(
                leviath_runtime::runfile::codec::FrameKind::State,
                &5u64,
            )
            .unwrap(),
        );
        garbage(&run_id, &bytes);
        assert_eq!(stages(&run_id).unwrap_err().code(), "INTERNAL");
        assert_eq!(context(&run_id).unwrap_err().code(), "INTERNAL");
        assert_eq!(graph(&run_id).unwrap_err().code(), "INTERNAL");
        assert_eq!(blobs(&run_id).unwrap_err().code(), "INTERNAL");
        assert_eq!(deltas(&run_id, None, None).unwrap().len(), 0);
        garbage(&run_id, b"not a run file");
        let sha = Digest::of(b"x");
        assert_eq!(blob(&run_id, sha.as_str()).unwrap_err().code(), "INTERNAL");
    })
    .await;
}

/// A step or a part that does not decode is an error on every read of it.
#[tokio::test]
async fn a_step_or_a_part_that_does_not_decode_is_an_error() {
    crate::runstate::with_isolated_runs_dir_async("inspect-bad-frames", |_d| async move {
        let run_id = recorded();
        super::super::run_file::tests::bad_step(&run_id, 1);
        assert_eq!(deltas(&run_id, None, None).unwrap_err().code(), "INTERNAL");
        assert_eq!(graph(&run_id).unwrap_err().code(), "INTERNAL");

        let run_id = recorded();
        let digest = Digest::of(b"half a part");
        let path = run_file::path(&run_id);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend(
            leviath_runtime::runfile::codec::encode(
                leviath_runtime::runfile::codec::FrameKind::Blob,
                &digest,
            )
            .unwrap(),
        );
        garbage(&run_id, &bytes);
        assert_eq!(
            blob(&run_id, digest.as_str()).unwrap_err().code(),
            "INTERNAL"
        );
    })
    .await;
}

#[tokio::test]
async fn steps_are_read_in_any_window() {
    crate::runstate::with_isolated_runs_dir_async("inspect-deltas", |_d| async move {
        let run_id = recorded();
        for at in [10, 20, 30] {
            step(&run_id, at, Vec::new(), |s| s.cursor.iteration += 1);
        }
        let seqs = |d: Vec<StateDelta>| d.into_iter().map(|d| d.seq).collect::<Vec<_>>();
        assert_eq!(seqs(deltas(&run_id, None, None).unwrap()), vec![1, 2, 3]);
        assert_eq!(seqs(deltas(&run_id, Some(2), None).unwrap()), vec![2, 3]);
        assert_eq!(seqs(deltas(&run_id, None, Some(1)).unwrap()), vec![1]);
        let backwards = deltas(&run_id, Some(3), Some(1)).unwrap_err();
        assert_eq!(backwards.code(), "BAD_USER_INPUT");
        assert_eq!(deltas("ghost", None, None).unwrap_err().code(), "NOT_FOUND");
    })
    .await;
}

#[tokio::test]
async fn a_graph_counts_the_edges_a_run_took() {
    crate::runstate::with_isolated_runs_dir_async("inspect-graph", |_d| async move {
        let run_id = recorded();
        let spec = run_file::require(&run_id).unwrap().spec().clone();
        let edge = spec.graph.edges[0].clone();
        let record = |edge_name| TransitionRecord {
            from: edge.from.clone(),
            to: edge.to.clone(),
            edge: edge_name,
            reason: TransitionReason::Condition,
            visit: "v".into(),
        };
        let taken = record(Some(edge.name.clone()));
        step(&run_id, 10, Vec::new(), |s| {
            s.last_transition = Some(taken.clone());
            s.cursor.stage = edge.to.clone();
            s.visits.insert(edge.to.clone(), 1);
        });
        // A forced move that names no edge counts against the edge between
        // the same two stages; then the edge is taken again.
        step(&run_id, 20, Vec::new(), |s| {
            s.last_transition = Some(record(None));
        });
        step(&run_id, 30, Vec::new(), |s| {
            s.last_transition = Some(taken.clone());
            s.visits.insert(edge.to.clone(), 2);
        });
        // A move no edge joins (a fan-out stage sent to its merge stage) is
        // an edge of its own, named by nothing and counted. Twice is one
        // edge taken twice.
        for at in [40, 50] {
            let unjoined = TransitionRecord {
                from: edge.to.clone(),
                to: edge.to.clone(),
                edge: None,
                reason: TransitionReason::Forced,
                visit: format!("v{at}"),
            };
            step(&run_id, at, Vec::new(), |s| {
                s.last_transition = Some(unjoined);
            });
        }

        let view = graph(&run_id).unwrap();
        assert_eq!(view.nodes.len(), spec.graph.stages.len());
        let entered = view
            .nodes
            .iter()
            .find(|n| n.stage == edge.to.as_str())
            .unwrap();
        assert_eq!(entered.visits, 2);
        assert!(entered.current);
        assert_eq!(view.nodes.iter().filter(|n| n.current).count(), 1);
        assert_eq!(view.edges[0].taken, 3);
        assert_eq!(view.edges[0].name.as_deref(), Some(edge.name.as_str()));
        let declared = spec.graph.edges.len();
        assert!(view.edges[1..declared].iter().all(|e| e.taken == 0));
        assert_eq!(view.edges.len(), declared + 1);
        let extra = &view.edges[declared];
        assert_eq!((extra.name.as_ref(), extra.condition), (None, None));
        assert_eq!(extra.reason, Some(TransitionReason::Forced));
        assert_eq!(extra.taken, 2);
        let json = serde_json::to_value(&view).unwrap();
        assert!(json["edges"][0]["condition"].is_string(), "{json}");
        assert!(json["edges"][0]["reason"].is_null(), "{json}");
        assert_eq!(json["edges"][declared]["reason"], "Forced", "{json}");

        assert_eq!(graph("ghost").unwrap_err().code(), "NOT_FOUND");
    })
    .await;
}

#[tokio::test]
async fn the_window_and_ledger_are_read_as_of_the_last_step() {
    crate::runstate::with_isolated_runs_dir_async("inspect-window", |_d| async move {
        let run_id = recorded();
        let window = context(&run_id).unwrap();
        assert!(!window.regions.is_empty());
        let last = run_file::require(&run_id).unwrap().latest_state().unwrap();
        assert_eq!(stages(&run_id).unwrap().len(), last.ledger.len());
        assert_eq!(context("ghost").unwrap_err().code(), "NOT_FOUND");
        assert_eq!(stages("ghost").unwrap_err().code(), "NOT_FOUND");
    })
    .await;
}

#[tokio::test]
async fn parts_are_read_from_the_run_file_or_the_blob_directory() {
    crate::runstate::with_isolated_runs_dir_async("inspect-blobs", |_d| async move {
        let run_id = recorded();
        assert!(blobs(&run_id).unwrap().is_empty());
        assert_eq!(blobs("ghost").unwrap_err().code(), "NOT_FOUND");

        let in_file = b"kept in the run file";
        let digest = Digest::of(in_file);
        let mut writer = RunFileWriter::open(&run_file::path(&run_id), Default::default()).unwrap();
        writer.add_blob(&digest, in_file).unwrap();
        drop(writer);
        assert_eq!(
            blob(&run_id, digest.as_str()).unwrap().unwrap(),
            in_file.to_vec()
        );

        let on_disk = b"kept beside it";
        let sha = Digest::of(on_disk);
        let dir = crate::runstate::run_dir(&run_id).join(leviath_core::files::BLOBS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(sha.as_str()), on_disk).unwrap();
        assert_eq!(
            blob(&run_id, sha.as_str()).unwrap().unwrap(),
            on_disk.to_vec()
        );

        assert!(blob(&run_id, "not-a-hash").unwrap().is_none());
        assert!(blob("ghost", sha.as_str()).unwrap().is_none());

        // The window names three parts: one whose bytes are in the file, one
        // beside it, and one nowhere.
        let lost = Digest::of(b"lost");
        step(&run_id, 10, Vec::new(), |s| {
            let region = &mut s.context.regions[0];
            for (d, n) in [(&digest, in_file.len()), (&sha, on_disk.len()), (&lost, 4)] {
                region.entries.push(stored_entry(d, n));
            }
        });
        let listed = blobs(&run_id).unwrap();
        let stored: Vec<(String, bool)> =
            listed.into_iter().map(|b| (b.sha256, b.stored)).collect();
        assert_eq!(
            stored,
            vec![
                (digest.to_string(), true),
                (sha.to_string(), true),
                (lost.to_string(), false),
            ]
        );
    })
    .await;
}

/// An entry carrying one stored part, `size` bytes hashed `digest`.
fn stored_entry(digest: &Digest, size: usize) -> EntryState {
    EntryState {
        text: String::new(),
        parts: vec![PartState {
            mime_type: "image/png".into(),
            body: PartBody::Stored(BlobState {
                digest: digest.clone(),
                size: size as u64,
                width: None,
                height: None,
                duration_ms: None,
                tokens: 1,
                stand_in: "[image]".into(),
            }),
            name: None,
            deliver: None,
        }],
        tokens: 1,
        timestamp: 0,
        kind: EntryKind::Text,
        meta: EntryMeta::None,
        key: None,
        reasoning: None,
    }
}
