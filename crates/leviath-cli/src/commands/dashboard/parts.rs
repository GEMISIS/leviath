//! Stored parts on the dashboard: the file under the Context tree's cursor,
//! opened with the operating system or written into the run's workdir, and
//! the files a typed answer names with `@path`.
//!
//! Nothing here plays or draws a file. `v` hands a copy to whatever the OS
//! opens that kind of file with, and `w` puts it where the run's own files
//! are, which is where a person editing them expects to find it.

use std::path::Path;

use leviath_core::media::{InboundPart, MediaRegistry, Part};

use super::context_tree::TreeRow;
use super::state::Dashboard;
use super::types::ToastLevel;
use crate::commands::result::export;

impl Dashboard {
    /// The stored part under the Context tree's cursor, with the run and
    /// working directory it belongs to.
    fn context_part_under_cursor(&self) -> Option<(String, String, Part)> {
        let agent = self.selected_agent()?;
        let snapshot = self.current_context_snapshot()?;
        let searching = self.search_mode || !self.search_query.is_empty();
        let rows = super::context_tree::rows(&snapshot, &self.context_tree, searching);
        let (region, index, part) = match rows.get(self.context_tree.cursor) {
            Some(TreeRow::Part {
                region,
                index,
                part,
            }) => (region.clone(), *index, *part),
            _ => return None,
        };
        snapshot
            .regions
            .iter()
            .find(|r| r.name == region)
            .and_then(|r| r.entries.get(index))
            .and_then(|e| e.content.parts().get(part))
            .map(|p| (agent.id.clone(), agent.workdir.clone(), p.clone()))
    }

    /// `v` in the Context view: hand the stored part under the cursor to
    /// the operating system, through a copy under the temp directory that
    /// has a name and an extension the opener can type it by.
    pub(super) fn open_context_part(&mut self) {
        let Some((run_id, _, part)) = self.context_part_under_cursor() else {
            self.toast("Move the cursor onto a stored part first", ToastLevel::Info);
            return;
        };
        let name = export_name_of(&part);
        let opener = self.mcp_ctx.opener.clone();
        let outcome = bytes_of(&run_id, &part)
            .and_then(|bytes| export::export_and_open(&run_id, &name, &bytes, &*opener));
        match outcome {
            Ok(path) => {
                self.add_log(format!("opened {}", path.display()));
                self.toast(format!("Opened {name}"), ToastLevel::Info);
            }
            Err(e) => self.toast(format!("Could not open {name}: {e}"), ToastLevel::Error),
        }
    }

    /// `w` in the Context view: write the stored part under the cursor into
    /// the run's working directory, under its own name.
    pub(super) fn write_context_part(&mut self) {
        let Some((run_id, workdir, part)) = self.context_part_under_cursor() else {
            self.toast("Move the cursor onto a stored part first", ToastLevel::Info);
            return;
        };
        let name = export_name_of(&part);
        let outcome = bytes_of(&run_id, &part)
            .and_then(|bytes| export::write_into(Path::new(&workdir), &name, &bytes));
        match outcome {
            Ok(path) => {
                self.add_log(format!("wrote {}", path.display()));
                self.toast(format!("Wrote {}", path.display()), ToastLevel::Info);
            }
            Err(e) => self.toast(format!("Could not write {name}: {e}"), ToastLevel::Error),
        }
    }

    /// The files a typed answer or message names with `@path`, read from
    /// the run's working directory. A token naming nothing stays text and
    /// gets a warning; a file that cannot be read keeps the text and says
    /// why, so the words are never lost to a bad attachment.
    pub(super) fn answer_parts(&mut self, text: &str, workdir: &str) -> (String, Vec<InboundPart>) {
        match crate::commands::run::attach::inline_parts(text, None, Path::new(workdir)) {
            Ok((kept, parts, unresolved)) => {
                for token in unresolved {
                    self.toast(
                        format!(
                            "'@{token}' names no file in the run's working directory; sent as text"
                        ),
                        ToastLevel::Warning,
                    );
                }
                (kept, parts)
            }
            Err(e) => {
                self.toast(
                    format!("Could not attach a file, so the text goes alone: {e}"),
                    ToastLevel::Error,
                );
                (text.to_string(), Vec::new())
            }
        }
    }
}

/// The file name a part exports under.
fn export_name_of(part: &Part) -> String {
    let (sha, media_type) = part
        .blob()
        .map(|b| (b.sha256.as_str(), b.media_type.as_str()))
        .unwrap_or_default();
    crate::blobs::export_name(
        part.name.as_deref(),
        sha,
        media_type,
        &MediaRegistry::builtin(),
    )
}

/// A stored part's bytes from the run's store.
fn bytes_of(run_id: &str, part: &Part) -> anyhow::Result<Vec<u8>> {
    let sha = part.blob().map(|b| b.sha256.as_str()).unwrap_or_default();
    crate::blobs::read(run_id, sha)
        .map_err(|e| anyhow::anyhow!("the bytes are not in the run's store: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use crate::commands::dashboard::types::{AgentDisplayStatus, DashboardAgent};
    use crate::runstate;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use leviath_core::media::{Blob, BlobStore, MediaType};
    use leviath_core::region::EntryContent;
    use leviath_core::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn test_agent(id: &str) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 2,
            status: AgentDisplayStatus::Active,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 0,
            broken_scripts: Vec::new(),
            waiting_prompt: None,
            wait_reason: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp".to_string(),
            task: "test".to_string(),
            title: None,
            model: None,
            parent_id: None,
            started_at: 1000,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
            accepts_messages: true,
        }
    }

    /// A run under the current runs dir holding one PNG, and a dashboard
    /// looking at its context with the cursor on the part's row.
    fn dashboard_on_a_part(run_id: &str, workdir: &Path) -> (Dashboard, String) {
        runstate::create_run(&crate::test_support::fixtures::run_meta(run_id)).unwrap();
        let store = leviath_runtime::blob_store::FsBlobStore::new(runstate::runs_dir());
        let png = Blob::new(
            MediaType::parse("image/png").unwrap(),
            b"\x89PNG\r\n\x1a\nhero".to_vec(),
        )
        .named("hero.png");
        let stored = Part::stored(store.put(run_id, &png, &MediaRegistry::builtin()).unwrap())
            .named("hero.png");
        let sha = stored.blob().unwrap().sha256.clone();
        let mut agent = test_agent(run_id);
        agent.workdir = workdir.to_string_lossy().to_string();
        agent.context_snapshot = Some(std::sync::Arc::new(ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 1,
            max_tokens: 100,
            regions: vec![RegionSnapshot {
                name: "task".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 1,
                max_tokens: 100,
                entries: vec![RegionEntrySnapshot {
                    content: EntryContent::from_parts(vec![Part::text("see"), stored]),
                    tokens: 1,
                    kind: Default::default(),
                    metadata: None,
                    key: None,
                    taint: Default::default(),
                    reasoning: None,
                }],
                description: None,
            }],
        }));
        let mut dash = make_test_dashboard();
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('c')));
        // Header, stub, then the part once the entry is open.
        dash.handle_key(key(KeyCode::Char('j')));
        dash.handle_key(key(KeyCode::Char(' ')));
        dash.handle_key(key(KeyCode::Char('j')));
        (dash, sha)
    }

    #[test]
    fn v_opens_and_w_writes_the_part_under_the_cursor() {
        runstate::with_isolated_runs_dir("dash-parts-keys", |_d| {
            let workdir = tempfile::tempdir().unwrap();
            let (mut dash, sha) = dashboard_on_a_part("parts-run", workdir.path());
            let on_part = TreeRow::Part {
                region: "task".to_string(),
                index: 0,
                part: 1,
            };
            assert_eq!(dash.context_tree_rows()[dash.context_tree.cursor], on_part);
            // Enter on a part row folds nothing: the row stays where it is.
            dash.handle_key(key(KeyCode::Enter));
            assert_eq!(dash.context_tree_rows()[dash.context_tree.cursor], on_part);

            dash.mcp_ctx.opener = std::sync::Arc::new(|_| true);
            dash.handle_key(key(KeyCode::Char('v')));
            let toasts = dash.toast_messages_for_test();
            assert!(toasts.iter().any(|t| t == "Opened hero.png"), "{toasts:?}");
            assert!(export::export_dir("parts-run").join("hero.png").is_file());
            let _ = std::fs::remove_dir_all(export::export_dir("parts-run"));

            dash.handle_key(key(KeyCode::Char('w')));
            assert_eq!(
                std::fs::read(workdir.path().join("hero.png")).unwrap(),
                b"\x89PNG\r\n\x1a\nhero"
            );
            let toasts = dash.toast_messages_for_test();
            assert!(toasts.iter().any(|t| t.starts_with("Wrote ")), "{toasts:?}");

            // An opener that refuses, then bytes the store no longer holds.
            dash.mcp_ctx.opener = std::sync::Arc::new(|_| false);
            dash.handle_key(key(KeyCode::Char('v')));
            let toasts = dash.toast_messages_for_test();
            assert!(
                toasts
                    .iter()
                    .any(|t| t.starts_with("Could not open hero.png")),
                "{toasts:?}"
            );
            let _ = std::fs::remove_dir_all(export::export_dir("parts-run"));
            std::fs::remove_file(crate::blobs::blob_path("parts-run", &sha)).unwrap();
            dash.handle_key(key(KeyCode::Char('w')));
            let toasts = dash.toast_messages_for_test();
            assert!(
                toasts
                    .iter()
                    .any(|t| t.starts_with("Could not write hero.png")),
                "{toasts:?}"
            );

            // On a row that is not a part, both keys only say so.
            dash.handle_key(key(KeyCode::Char('k')));
            dash.handle_key(key(KeyCode::Char('v')));
            dash.handle_key(key(KeyCode::Char('w')));
            let toasts = dash.toast_messages_for_test();
            assert_eq!(
                toasts
                    .iter()
                    .filter(|t| t.contains("Move the cursor onto a stored part"))
                    .count(),
                2,
                "{toasts:?}"
            );
        });
    }

    #[test]
    fn with_no_run_selected_there_is_no_part_to_act_on() {
        runstate::with_isolated_runs_dir("dash-parts-none", |_d| {
            let mut dash = make_test_dashboard();
            assert!(dash.context_part_under_cursor().is_none());
            dash.open_context_part();
            assert!(!dash.toast_messages_for_test().is_empty());
            // A run with no context to read has no parts either.
            dash.agents.push(test_agent("bare"));
            dash.update_display_indices();
            assert!(dash.context_part_under_cursor().is_none());
        });
    }

    #[test]
    fn a_part_with_no_name_exports_under_its_hash() {
        let part = Part::text("words");
        assert_eq!(export_name_of(&part), "");
        assert!(bytes_of("run", &part).is_err());
    }
}
