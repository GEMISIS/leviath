//! Tests for the run fields that read the disk: files, one file's text, and the
//! context history.
//!
//! These need a runs directory and a working directory, so they are separate
//! from the plain field tests: what is asserted is the answer over real files.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::run::Run;
use crate::commands::serve::testutil::state_with_agent_paths;
use crate::runstate::{RunMeta, create_run};

/// A run whose working directory is the given one.
fn meta_in(workdir: &std::path::Path) -> RunMeta {
    let mut meta = RunMeta::new(
        "reader".to_string(),
        "coder".to_string(),
        "/agents/coder/agent.leviath".to_string(),
        "read the files".to_string(),
        None,
        workdir.to_string_lossy().into_owned(),
        3,
    );
    meta.started_at = 1_788_924_523;
    meta.updated_at = 1_788_924_600;
    meta.current_stage = "review".to_string();
    meta.stage_index = 1;
    meta
}

/// Ask the schema about one run, with a server behind it.
async fn ask(meta: RunMeta, query: &str) -> async_graphql::Response {
    let run = Run {
        meta: Arc::new(meta),
        now: 1_788_925_000,
    };
    let schema = Schema::build(Probe { run }, EmptyMutation, EmptySubscription)
        .data(state_with_agent_paths(Vec::new()))
        .finish();
    schema.execute(Request::new(query)).await
}

/// The data of a query that is expected to work.
async fn data(meta: RunMeta, query: &str) -> serde_json::Value {
    let answer = ask(meta, query).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// A journal with one context point per token total, so the history has
/// something to page over.
fn write_journal(meta: &RunMeta, totals: &[usize]) {
    use leviath_core::run_archive::{self, RunIdentity, RunRecord};

    let mut buf = Vec::new();
    run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION)
        .expect("a preamble");
    run_archive::write_record(
        &mut buf,
        &RunRecord::Header {
            identity: RunIdentity {
                run_id: meta.run_id.clone(),
                machine_id: "m".to_string(),
                world_id: "w".to_string(),
                created_at: 0,
            },
            meta: Box::new(meta.clone()),
        },
    )
    .expect("a header");
    for (i, total) in totals.iter().enumerate() {
        run_archive::write_record(
            &mut buf,
            &RunRecord::ContextCheckpoint {
                snapshot: crate::runstate::ContextSnapshot {
                    stage_name: "review".to_string(),
                    total_tokens: *total,
                    max_tokens: 1000,
                    regions: Vec::new(),
                },
                at: 1 + i as i64,
            },
        )
        .expect("a point");
    }
    std::fs::write(
        crate::runstate::run_dir(&meta.run_id).join(leviath_core::files::ARCHIVE_FILE),
        &buf,
    )
    .expect("the journal");
}

/// A root handing out one run.
struct Probe {
    run: Run,
}

#[async_graphql::Object]
impl Probe {
    /// The run under test.
    async fn run(&self) -> &Run {
        &self.run
    }
}

/// The working directory listing is what is there now, one level at a time.
#[tokio::test]
async fn a_workdir_listing_reads_one_level() {
    let workdir = tempfile::tempdir().expect("a workdir");
    std::fs::write(workdir.path().join("report.md"), "# findings").expect("a file");
    std::fs::create_dir(workdir.path().join("src")).expect("a directory");
    std::fs::write(workdir.path().join("src/main.rs"), "fn main() {}").expect("a file");
    std::fs::write(workdir.path().join(".hidden"), "x").expect("a dot file");

    let json = data(
        meta_in(workdir.path()),
        r#"{ run { files(source: WORKDIR) {
             source path parent workdir truncated modifiedFilesTruncated
             entries { name path isDir size exists outsideWorkdir mimeType }
           } } }"#,
    )
    .await;
    let listing = &json["run"]["files"];
    assert_eq!(listing["source"], "WORKDIR");
    assert!(listing["parent"].is_null(), "never above the fence");
    assert_eq!(listing["truncated"], false);
    let entries = listing["entries"].as_array().expect("entries");
    // Directories first, then by name, done here rather than in every client.
    assert_eq!(entries[0]["name"], "src");
    assert_eq!(entries[0]["isDir"], true);
    assert_eq!(entries[0]["mimeType"], "", "a directory has no type");
    assert_eq!(entries[1]["name"], "report.md");
    assert_eq!(entries[1]["mimeType"], "text/markdown");
    assert_eq!(entries[1]["exists"], true);
    assert_eq!(entries[1]["size"], 10);
    assert!(
        !entries.iter().any(|entry| entry["name"] == ".hidden"),
        "a dot file stays out unless asked for: {entries:?}"
    );

    // One level down, by passing an entry's own path back.
    let json = data(
        meta_in(workdir.path()),
        r#"{ run { files(source: WORKDIR, path: "src") { path parent entries { name } } } }"#,
    )
    .await;
    assert_eq!(json["run"]["files"]["entries"][0]["name"], "main.rs");
    assert!(
        json["run"]["files"]["parent"].as_str().is_some(),
        "a level down has somewhere to go back to"
    );

    // And with the dot files asked for.
    let json = data(
        meta_in(workdir.path()),
        r#"{ run { files(source: WORKDIR, hidden: true) { entries { name } } } }"#,
    )
    .await;
    assert!(
        json["run"]["files"]["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|entry| entry["name"] == ".hidden")
    );
}

/// The recorded listing is the run's own account, including what has gone.
///
/// A deleted path stays in the list and says it is gone, because the list is a
/// record of what the run did rather than of what is on the disk.
#[tokio::test]
async fn a_recorded_listing_keeps_what_the_run_touched() {
    let workdir = tempfile::tempdir().expect("a workdir");
    std::fs::write(workdir.path().join("kept.txt"), "still here").expect("a file");
    let mut meta = meta_in(workdir.path());
    meta.flags.modified_files = vec!["kept.txt".to_string(), "gone.txt".to_string()];
    meta.flags.modified_file_count = 5;

    let json = data(
        meta,
        r#"{ run { files { source modifyingToolCalls modifiedFilesTruncated
             entries { name exists outsideWorkdir } } } }"#,
    )
    .await;
    let listing = &json["run"]["files"];
    assert_eq!(listing["source"], "MODIFIED");
    // Not a file count: a run that edits one file three times records three.
    assert_eq!(listing["modifyingToolCalls"], 5);
    assert_eq!(listing["modifiedFilesTruncated"], false);
    let entries = listing["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["exists"], true);
    assert_eq!(entries[1]["name"], "gone.txt");
    assert_eq!(entries[1]["exists"], false, "reported, not filtered away");
}

/// A file is read a window at a time, and the windows concatenate into it.
#[tokio::test]
async fn a_file_is_read_a_window_at_a_time() {
    let workdir = tempfile::tempdir().expect("a workdir");
    std::fs::write(workdir.path().join("report.md"), "abcdefghij").expect("a file");

    let json = data(
        meta_in(workdir.path()),
        r#"{ run { fileContent(path: "report.md") {
             path size offset nextOffset content truncated } } }"#,
    )
    .await;
    let window = &json["run"]["fileContent"];
    assert_eq!(window["content"], "abcdefghij");
    assert_eq!(window["size"], 10);
    assert_eq!(window["truncated"], false);
    assert!(
        window["nextOffset"].is_null(),
        "this window reached the end"
    );

    // A window from the middle, which is how a caller pages a large file.
    let json = data(
        meta_in(workdir.path()),
        r#"{ run { fileContent(path: "report.md", offset: 4) { offset content } } }"#,
    )
    .await;
    assert_eq!(json["run"]["fileContent"]["offset"], 4);
    assert_eq!(json["run"]["fileContent"]["content"], "efghij");
}

/// Each way of asking for the wrong thing has its own code, so a client knows
/// what to do about it.
#[tokio::test]
async fn reading_the_wrong_thing_says_which_wrong_thing_it_was() {
    let workdir = tempfile::tempdir().expect("a workdir");
    std::fs::create_dir(workdir.path().join("src")).expect("a directory");
    std::fs::write(workdir.path().join("report.md"), "abc").expect("a file");
    std::fs::write(workdir.path().join("blob.bin"), [0xff, 0xfe, 0xff]).expect("a binary file");

    let code = |answer: &async_graphql::Response| -> String {
        answer
            .errors
            .first()
            .expect("a refusal")
            .extensions
            .as_ref()
            .and_then(|e| e.get("code"))
            .map(ToString::to_string)
            .unwrap_or_default()
    };

    // A directory is not text, and `files` is the field that answers what is in
    // one. The message says so rather than leaving a client guessing.
    let answer = ask(
        meta_in(workdir.path()),
        r#"{ run { fileContent(path: "src") { content } } }"#,
    )
    .await;
    assert_eq!(code(&answer), "\"BAD_USER_INPUT\"");
    assert!(
        answer.errors[0].message.contains("files"),
        "{}",
        answer.errors[0].message
    );

    // Outside the fence: refused rather than followed, even though the path
    // resolves to something real.
    let answer = ask(
        meta_in(workdir.path()),
        r#"{ run { fileContent(path: "../outside.txt") { content } } }"#,
    )
    .await;
    assert_eq!(code(&answer), "\"FORBIDDEN\"");

    // Nothing there at all.
    let answer = ask(
        meta_in(workdir.path()),
        r#"{ run { fileContent(path: "nope.md") { content } } }"#,
    )
    .await;
    assert_eq!(code(&answer), "\"NOT_FOUND\"");

    // Past the end of a file that is there: a different window of the same file
    // would work, which is why this is not a bad request.
    let answer = ask(
        meta_in(workdir.path()),
        r#"{ run { fileContent(path: "report.md", offset: 99) { content } } }"#,
    )
    .await;
    assert_eq!(code(&answer), "\"RANGE_NOT_SATISFIABLE\"");

    // Not text. The file is there, and a signed link fetches it whole.
    let answer = ask(
        meta_in(workdir.path()),
        r#"{ run { fileContent(path: "blob.bin") { content } } }"#,
    )
    .await;
    assert_eq!(code(&answer), "\"UNSUPPORTED_MEDIA_TYPE\"");

    // And a negative offset, which is a client that built its query wrong.
    let answer = ask(
        meta_in(workdir.path()),
        r#"{ run { fileContent(path: "report.md", offset: -1) { content } } }"#,
    )
    .await;
    assert_eq!(code(&answer), "\"BAD_USER_INPUT\"");
}

/// A lost working directory is told apart from a run that touched nothing.
#[tokio::test]
async fn a_lost_working_directory_says_so() {
    let gone = {
        let workdir = tempfile::tempdir().expect("a workdir");
        workdir.path().to_path_buf()
    };
    let answer = ask(
        meta_in(&gone),
        "{ run { files(source: WORKDIR) { entries { name } } } }",
    )
    .await;
    let error = answer.errors.first().expect("a refusal");
    assert!(
        error.message.contains("no longer exists"),
        "an empty listing would read as a run that touched nothing: {}",
        error.message
    );
}

/// The history is paged, newest-first on request, and the cursor carries on.
#[tokio::test]
async fn the_context_history_pages_in_either_direction() {
    crate::runstate::with_isolated_runs_dir_async("graphql-history", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let meta = meta_in(workdir.path());
        create_run(&meta).expect("run written");
        // Three points, each a whole window, which is why this field pages.
        write_journal(&meta, &[10, 20, 30]);

        let json = data(
            meta_in(workdir.path()),
            r#"{ run { contextHistory(first: 2) {
                 total pageInfo { hasNextPage endCursor }
                 edges { node { at stage window { totalTokens } } }
               } } }"#,
        )
        .await;
        let page = &json["run"]["contextHistory"];
        assert_eq!(page["total"], 3);
        assert_eq!(page["pageInfo"]["hasNextPage"], true);
        assert_eq!(page["edges"].as_array().map(Vec::len), Some(2));
        assert_eq!(page["edges"][0]["node"]["window"]["totalTokens"], 10);
        assert_eq!(page["edges"][0]["node"]["stage"], "review");

        // The cursor carries on from where that page stopped.
        let cursor = page["pageInfo"]["endCursor"].as_str().expect("a cursor");
        let json = data(
            meta_in(workdir.path()),
            &format!(
                r#"{{ run {{ contextHistory(first: 2, after: "{cursor}") {{
                     pageInfo {{ hasNextPage }}
                     edges {{ node {{ window {{ totalTokens }} }} }}
                   }} }} }}"#
            ),
        )
        .await;
        let page = &json["run"]["contextHistory"];
        assert_eq!(page["edges"].as_array().map(Vec::len), Some(1));
        assert_eq!(page["edges"][0]["node"]["window"]["totalTokens"], 30);
        assert_eq!(page["pageInfo"]["hasNextPage"], false);

        // Newest first is the same points in the other order.
        let json = data(
            meta_in(workdir.path()),
            r#"{ run { contextHistory(first: 3, descending: true) {
                 edges { node { window { totalTokens } } } } } }"#,
        )
        .await;
        let edges = json["run"]["contextHistory"]["edges"]
            .as_array()
            .expect("edges");
        assert_eq!(edges[0]["node"]["window"]["totalTokens"], 30);
        assert_eq!(edges[2]["node"]["window"]["totalTokens"], 10);
    })
    .await;
}

/// A page size over the history's own cap is refused rather than clamped, and
/// the message says what the cap is for.
#[tokio::test]
async fn an_oversized_history_page_is_refused() {
    let workdir = tempfile::tempdir().expect("a workdir");
    let answer = ask(
        meta_in(workdir.path()),
        "{ run { contextHistory(first: 5000) { total } } }",
    )
    .await;
    let error = answer.errors.first().expect("a refusal");
    assert!(
        error.message.contains("whole"),
        "it says why the cap is lower here: {}",
        error.message
    );

    let answer = ask(
        meta_in(workdir.path()),
        "{ run { contextHistory(first: 0) { total } } }",
    )
    .await;
    assert!(
        answer
            .errors
            .first()
            .expect("a refusal")
            .message
            .contains("at least 1")
    );
}

/// A run with no journal has no history, which is not an empty page.
#[tokio::test]
async fn a_run_with_no_journal_has_no_history() {
    crate::runstate::with_isolated_runs_dir_async("graphql-history-none", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        create_run(&meta_in(workdir.path())).expect("run written");
        let answer = ask(
            meta_in(workdir.path()),
            "{ run { contextHistory(first: 5) { total } } }",
        )
        .await;
        assert_eq!(
            answer
                .errors
                .first()
                .expect("a refusal")
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"NOT_FOUND\"".to_string())
        );
    })
    .await;
}

/// The stage a run is in, in its blueprint's own terms.
#[tokio::test]
async fn a_run_says_which_stage_it_is_in() {
    let workdir = tempfile::tempdir().expect("a workdir");
    let json = data(
        meta_in(workdir.path()),
        "{ run { currentStage { name index of } } }",
    )
    .await;
    assert_eq!(json["run"]["currentStage"]["name"], "review");
    assert_eq!(json["run"]["currentStage"]["index"], 1);
    assert_eq!(json["run"]["currentStage"]["of"], 3);

    // Before the first stage is entered there is no answer, rather than an
    // empty name that reads as a stage called nothing.
    let mut fresh = meta_in(workdir.path());
    fresh.current_stage = String::new();
    let json = data(fresh, "{ run { currentStage { name } } }").await;
    assert!(json["run"]["currentStage"].is_null());
}
