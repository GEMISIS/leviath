//! File-based interaction artifacts under a run directory.
//!
//! The shared-world daemon resolves interactions in memory (see
//! [`leviath_runtime::interaction_hub`]); these helpers remain for the dashboard
//! and `lev respond` to read/write the on-disk `pending.json` / `response.json`
//! view of a run's outstanding interaction:
//!
//! - `pending.json`  — the request an agent is waiting on
//! - `response.json` — the user's answer

use std::path::PathBuf;

use crate::runstate::run_dir;

// ─── Value types (re-exported from `leviath-core`) ────────────
//
// The plain serde value types and their pure resolver helpers live in
// `leviath_core::interaction` so the engine in `leviath-runtime` can reference
// them without depending on the CLI. Re-exported here so `crate::interaction::*`
// paths resolve.
pub use leviath_core::interaction::{
    ApprovalScope, BodyFormat, InteractionKind, InteractionRequest, InteractionResponse,
    make_interaction_id, response_approved, response_as_choice, response_as_text,
};

// ─── File paths ─────────────────────────────────────────────────────────────

pub fn pending_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("pending.json")
}

pub fn response_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("response.json")
}

// ─── Write helpers (used by the dashboard / `lev respond`) ─────────────────

/// Atomically write pre-serialized `json` to `path` (via a `.json.tmp`
/// sibling + rename).
///
/// Non-generic (takes an already-serialized string) so it has a single
/// monomorphization and every region — including the `std::fs` error `?`
/// arms — is exercised by real tests. Serialization is performed by the
/// callers, whose concrete types (`InteractionRequest`/`InteractionResponse`)
/// are provably infallible to serialize (see the `.expect` sites).
fn write_json_atomic(path: &std::path::Path, json: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Write an interaction request to disk (called by the worker).
pub fn write_request(run_id: &str, req: &InteractionRequest) -> anyhow::Result<()> {
    // `InteractionRequest` is a plain struct with no maps keyed by non-strings
    // and no non-finite floats, so `to_string_pretty` cannot fail.
    let json = serde_json::to_string_pretty(req)
        .expect("infallible: InteractionRequest always serializes to JSON");
    write_json_atomic(&pending_path(run_id), &json)
}

/// Read the current interaction request for a run (used by the dashboard).
pub fn read_request(run_id: &str) -> Option<InteractionRequest> {
    let path = pending_path(run_id);
    let json = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&json).ok()
}

/// Write an interaction response to disk (called by the dashboard / `lev respond`).
pub fn write_response(run_id: &str, resp: &InteractionResponse) -> anyhow::Result<()> {
    // `InteractionResponse` is a plain struct that cannot fail to serialize.
    let json = serde_json::to_string_pretty(resp)
        .expect("infallible: InteractionResponse always serializes to JSON");
    write_json_atomic(&response_path(run_id), &json)
}

/// Read and atomically consume the response file, returning `None` if none has
/// been written yet. Pairs with [`write_response`] for the dashboard's
/// file-based interaction view.
pub fn take_response(run_id: &str) -> Option<InteractionResponse> {
    let path = response_path(run_id);
    let json = std::fs::read_to_string(&path).ok()?;
    let resp: InteractionResponse = serde_json::from_str(&json).ok()?;
    // Remove the file so a response is only consumed once.
    let _ = std::fs::remove_file(&path);
    Some(resp)
}

/// Delete both pending and response files (cleanup after handling).
pub fn clear_interaction(run_id: &str) {
    let _ = std::fs::remove_file(pending_path(run_id));
    let _ = std::fs::remove_file(response_path(run_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── File I/O roundtrip (write_request, read_request, etc.) ────────────

    #[test]
    fn test_write_and_read_request() {
        crate::runstate::with_isolated_runs_dir("test_write_and_read_request", |_d| {
            let run_id = "test-interaction-rw-req";
            let dir = crate::runstate::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();

            let req = InteractionRequest::free_text("rw1", "What now?", "plan", true);
            write_request(run_id, &req).unwrap();

            let back = read_request(run_id);
            assert!(back.is_some());
            let back = back.unwrap();
            assert_eq!(back.id, "rw1");
            assert_eq!(back.prompt, "What now?");
            assert_eq!(back.kind, InteractionKind::FreeText);

            let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        });
    }

    #[test]
    fn test_write_request_rename_fails_when_target_is_a_directory() {
        crate::runstate::with_isolated_runs_dir(
            "test_write_request_rename_fails_when_target_is_a_directory",
            |_d| {
                let run_id = "test-write-request-target-is-dir";
                let dir = crate::runstate::run_dir(run_id);
                std::fs::create_dir_all(&dir).unwrap();
                // Pre-create the target path as a directory: the tmp-file write
                // succeeds, but `fs::rename` onto an existing directory fails,
                // exercising `write_request`'s rename `?`.
                std::fs::create_dir_all(pending_path(run_id)).unwrap();

                let req = InteractionRequest::free_text("rw1", "What now?", "plan", true);
                let result = write_request(run_id, &req);
                assert!(result.is_err());

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    #[test]
    fn test_write_request_tmp_write_fails_when_target_is_a_directory() {
        crate::runstate::with_isolated_runs_dir(
            "test_write_request_tmp_write_fails_when_target_is_a_directory",
            |_d| {
                let run_id = "test-write-request-tmp-is-dir";
                let dir = crate::runstate::run_dir(run_id);
                std::fs::create_dir_all(&dir).unwrap();
                // Pre-create the *tmp* path (not the final path, covered by
                // `test_write_request_rename_fails_when_target_is_a_directory` above)
                // as a directory: `std::fs::write(&tmp, &json)` itself fails with
                // EISDIR before `fs::rename` is ever reached, exercising
                // `write_request`'s tmp-file-write `?` -- a distinct branch from the
                // rename failure, since a well-formed JSON body is never itself
                // capable of making `serde_json::to_string_pretty` fail.
                std::fs::create_dir_all(pending_path(run_id).with_extension("json.tmp")).unwrap();

                let req = InteractionRequest::free_text("rw1", "What now?", "plan", true);
                let result = write_request(run_id, &req);
                assert!(result.is_err());

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    #[test]
    fn test_write_and_read_response() {
        crate::runstate::with_isolated_runs_dir("test_write_and_read_response", |_d| {
            let run_id = "test-interaction-rw-resp";
            let dir = crate::runstate::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();

            let resp = InteractionResponse::text("rw2", "my answer");
            write_response(run_id, &resp).unwrap();

            let back = take_response(run_id);
            assert!(back.is_some());
            let back = back.unwrap();
            assert_eq!(back.request_id, "rw2");
            assert_eq!(back.value.as_deref(), Some("my answer"));

            // take_response should have removed the file
            assert!(take_response(run_id).is_none());

            let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        });
    }

    #[test]
    fn test_write_response_rename_fails_when_target_is_a_directory() {
        crate::runstate::with_isolated_runs_dir(
            "test_write_response_rename_fails_when_target_is_a_directory",
            |_d| {
                let run_id = "test-write-response-target-is-dir";
                let dir = crate::runstate::run_dir(run_id);
                std::fs::create_dir_all(&dir).unwrap();
                // Pre-create the target path as a directory: the tmp-file write
                // succeeds, but `fs::rename` onto an existing directory fails,
                // exercising `write_response`'s rename `?`.
                std::fs::create_dir_all(response_path(run_id)).unwrap();

                let resp = InteractionResponse::text("rw2", "my answer");
                let result = write_response(run_id, &resp);
                assert!(result.is_err());

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    #[test]
    fn test_write_response_tmp_write_fails_when_target_is_a_directory() {
        crate::runstate::with_isolated_runs_dir(
            "test_write_response_tmp_write_fails_when_target_is_a_directory",
            |_d| {
                let run_id = "test-write-response-tmp-is-dir";
                let dir = crate::runstate::run_dir(run_id);
                std::fs::create_dir_all(&dir).unwrap();
                // See `test_write_request_tmp_write_fails_when_target_is_a_directory`
                // -- same distinction, for `write_response`'s own tmp-file write.
                std::fs::create_dir_all(response_path(run_id).with_extension("json.tmp")).unwrap();

                let resp = InteractionResponse::text("rw2", "my answer");
                let result = write_response(run_id, &resp);
                assert!(result.is_err());

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    #[test]
    fn test_clear_interaction() {
        crate::runstate::with_isolated_runs_dir("test_clear_interaction", |_d| {
            let run_id = "test-interaction-clear";
            let dir = crate::runstate::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();

            let req = InteractionRequest::free_text("c1", "prompt", "stage", true);
            write_request(run_id, &req).unwrap();
            let resp = InteractionResponse::text("c1", "answer");
            write_response(run_id, &resp).unwrap();

            assert!(pending_path(run_id).exists());
            assert!(response_path(run_id).exists());

            clear_interaction(run_id);

            assert!(!pending_path(run_id).exists());
            assert!(!response_path(run_id).exists());

            let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        });
    }

    #[test]
    fn test_read_request_missing_returns_none() {
        assert!(read_request("nonexistent-run-interaction").is_none());
    }

    #[test]
    fn test_take_response_missing_returns_none() {
        assert!(take_response("nonexistent-run-interaction").is_none());
    }

    // ─── pending_path / response_path ──────────────────────────────────────

    #[test]
    fn test_pending_path_structure() {
        let path = pending_path("run-abc");
        assert!(path.to_str().unwrap().contains("run-abc"));
        assert!(path.to_str().unwrap().ends_with("pending.json"));
    }

    #[test]
    fn test_response_path_structure() {
        let path = response_path("run-abc");
        assert!(path.to_str().unwrap().contains("run-abc"));
        assert!(path.to_str().unwrap().ends_with("response.json"));
    }

    // ─── write_request/read_request with complex data ─────────────────────

    #[test]
    fn test_write_read_request_tool_approval() {
        crate::runstate::with_isolated_runs_dir("test_write_read_request_tool_approval", |_d| {
            let run_id = "test-interaction-rw-ta";
            let dir = crate::runstate::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();

            let req = InteractionRequest::tool_approval(
                "ta1",
                "bash",
                serde_json::json!({"command": "rm -rf /", "cwd": "/tmp"}),
                "code",
            );
            write_request(run_id, &req).unwrap();

            let back = read_request(run_id).unwrap();
            assert_eq!(back.kind, InteractionKind::ToolApproval);
            assert_eq!(back.tool_name.as_deref(), Some("bash"));
            assert!(back.tool_arguments.is_some());
            assert_eq!(back.options.len(), 3);

            let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        });
    }

    #[test]
    fn test_write_read_request_multiple_choice() {
        crate::runstate::with_isolated_runs_dir("test_write_read_request_multiple_choice", |_d| {
            let run_id = "test-interaction-rw-mc";
            let dir = crate::runstate::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();

            let req = InteractionRequest::multiple_choice(
                "mc1",
                "Pick approach",
                vec!["Fast".into(), "Thorough".into(), "Cancel".into()],
                "plan",
            );
            write_request(run_id, &req).unwrap();

            let back = read_request(run_id).unwrap();
            assert_eq!(back.kind, InteractionKind::MultipleChoice);
            assert_eq!(back.options.len(), 3);
            assert_eq!(back.options[0], "Fast");

            let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        });
    }

    #[test]
    fn test_write_read_request_confirm() {
        crate::runstate::with_isolated_runs_dir("test_write_read_request_confirm", |_d| {
            let run_id = "test-interaction-rw-confirm";
            let dir = crate::runstate::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();

            let req = InteractionRequest::confirm("cf1", "Deploy to prod?", "deploy");
            write_request(run_id, &req).unwrap();

            let back = read_request(run_id).unwrap();
            assert_eq!(back.kind, InteractionKind::Confirm);
            assert_eq!(back.options, vec!["Yes", "No"]);

            let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        });
    }

    #[test]
    fn test_write_read_request_review() {
        crate::runstate::with_isolated_runs_dir("test_write_read_request_review", |_d| {
            let run_id = "test-interaction-rw-review";
            let dir = crate::runstate::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();

            let req = InteractionRequest::review(
                "rev1",
                "Architecture Review",
                "# Architecture\n\n- Component A\n- Component B",
                "plan",
            );
            write_request(run_id, &req).unwrap();

            let back = read_request(run_id).unwrap();
            assert_eq!(back.body_format, BodyFormat::Markdown);
            assert!(back.body.as_deref().unwrap().contains("Component A"));

            let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        });
    }

    // ─── write_response/take_response approval ────────────────────────────

    #[test]
    fn test_write_take_response_approval() {
        crate::runstate::with_isolated_runs_dir("test_write_take_response_approval", |_d| {
            let run_id = "test-interaction-rw-approval";
            let dir = crate::runstate::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();

            let resp = InteractionResponse::approval("ap1", true, ApprovalScope::Session);
            write_response(run_id, &resp).unwrap();

            let back = take_response(run_id).unwrap();
            assert_eq!(back.approved, Some(true));
            assert_eq!(back.scope, Some(ApprovalScope::Session));

            // Should be consumed
            assert!(take_response(run_id).is_none());

            let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        });
    }

    #[test]
    fn test_write_take_response_choice() {
        crate::runstate::with_isolated_runs_dir("test_write_take_response_choice", |_d| {
            let run_id = "test-interaction-rw-choice";
            let dir = crate::runstate::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();

            let resp = InteractionResponse::choice("ch1", 2);
            write_response(run_id, &resp).unwrap();

            let back = take_response(run_id).unwrap();
            assert_eq!(back.choice_index, Some(2));

            let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        });
    }

    // ─── clear_interaction on nonexistent is safe ─────────────────────────

    #[test]
    fn test_clear_interaction_nonexistent_does_not_panic() {
        clear_interaction("nonexistent-run-clear-test");
    }

    // ─── Write/read request with temp directories ─────────────────────────

    #[test]
    fn test_write_read_response_choice_roundtrip() {
        crate::runstate::with_isolated_runs_dir(
            "test_write_read_response_choice_roundtrip",
            |_d| {
                let run_id = "test-interaction-rw-choice-rt";
                let dir = crate::runstate::run_dir(run_id);
                std::fs::create_dir_all(&dir).unwrap();

                let resp = InteractionResponse::choice("ch-rt", 1);
                write_response(run_id, &resp).unwrap();

                let back = take_response(run_id).unwrap();
                assert_eq!(back.request_id, "ch-rt");
                assert_eq!(back.choice_index, Some(1));
                assert!(back.value.is_none());

                let _ = std::fs::remove_dir_all(dir);
            },
        );
    }

    // ─── clear_interaction after only writing request ─────────────────────

    #[test]
    fn test_clear_interaction_only_request() {
        crate::runstate::with_isolated_runs_dir("test_clear_interaction_only_request", |_d| {
            let run_id = "test-interaction-clear-req-only";
            let dir = crate::runstate::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();

            let req = InteractionRequest::free_text("cr1", "prompt", "stage", true);
            write_request(run_id, &req).unwrap();
            assert!(pending_path(run_id).exists());

            clear_interaction(run_id);
            assert!(!pending_path(run_id).exists());
            assert!(!response_path(run_id).exists());

            let _ = std::fs::remove_dir_all(dir);
        });
    }

    // ─── read_request returns None for corrupted JSON ─────────────────────

    #[test]
    fn test_read_request_corrupted_json_returns_none() {
        crate::runstate::with_isolated_runs_dir(
            "test_read_request_corrupted_json_returns_none",
            |_d| {
                let run_id = "test-interaction-corrupt-req";
                let dir = crate::runstate::run_dir(run_id);
                std::fs::create_dir_all(&dir).unwrap();

                std::fs::write(pending_path(run_id), "not valid json {{{").unwrap();
                assert!(read_request(run_id).is_none());

                let _ = std::fs::remove_dir_all(dir);
            },
        );
    }

    // ─── take_response returns None for corrupted JSON ────────────────────

    #[test]
    fn test_take_response_corrupted_json_returns_none() {
        crate::runstate::with_isolated_runs_dir(
            "test_take_response_corrupted_json_returns_none",
            |_d| {
                let run_id = "test-interaction-corrupt-resp";
                let dir = crate::runstate::run_dir(run_id);
                std::fs::create_dir_all(&dir).unwrap();

                std::fs::write(response_path(run_id), "garbage").unwrap();
                assert!(take_response(run_id).is_none());

                let _ = std::fs::remove_dir_all(dir);
            },
        );
    }

    // ─── request_id matching in write/read ─────────────────────────────────

    #[test]
    fn test_request_id_preserved_through_write_read() {
        crate::runstate::with_isolated_runs_dir(
            "test_request_id_preserved_through_write_read",
            |_d| {
                let run_id = "test-interaction-reqid";
                let dir = crate::runstate::run_dir(run_id);
                std::fs::create_dir_all(&dir).unwrap();

                let req = InteractionRequest::tool_approval(
                    "unique-req-42",
                    "write_file",
                    serde_json::json!({"path": "/tmp/foo"}),
                    "code",
                );
                write_request(run_id, &req).unwrap();

                let back = read_request(run_id).unwrap();
                assert_eq!(back.id, "unique-req-42");

                // Write response with matching request_id
                let resp =
                    InteractionResponse::approval("unique-req-42", true, ApprovalScope::Once);
                write_response(run_id, &resp).unwrap();

                let back_resp = take_response(run_id).unwrap();
                assert_eq!(back_resp.request_id, "unique-req-42");

                let _ = std::fs::remove_dir_all(dir);
            },
        );
    }

    // ─── Multiple write_request overwrites previous ───────────────────────

    #[test]
    fn test_write_request_overwrites_previous() {
        crate::runstate::with_isolated_runs_dir("test_write_request_overwrites_previous", |_d| {
            let run_id = "test-interaction-overwrite";
            let dir = crate::runstate::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();

            let req1 = InteractionRequest::free_text("first", "First?", "stage", true);
            write_request(run_id, &req1).unwrap();

            let req2 = InteractionRequest::free_text("second", "Second?", "stage", true);
            write_request(run_id, &req2).unwrap();

            let back = read_request(run_id).unwrap();
            assert_eq!(back.id, "second");
            assert_eq!(back.prompt, "Second?");

            let _ = std::fs::remove_dir_all(dir);
        });
    }
}
