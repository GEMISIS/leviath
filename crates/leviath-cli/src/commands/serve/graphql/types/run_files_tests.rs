//! Tests for the run fields that read the disk: files, one file's text, and the
//! context history.
//!
//! These need a runs directory and a working directory, so they are separate
//! from the plain field tests: what is asserted is the answer over real files.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::run::Run;
use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum, exercise_list};
use crate::commands::serve::graphql::scalars::BigInt;
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
             path parent workdir isTruncated isModifiedFilesTruncated
             results { name path isDir size exists isOutsideWorkdir mimeType }
           } } }"#,
    )
    .await;
    let listing = &json["run"]["files"];
    assert!(listing["parent"].is_null(), "never above the fence");
    assert_eq!(listing["isTruncated"], false);
    let entries = listing["results"].as_array().expect("results");
    // Directories first, then by name, done here rather than in every client.
    // Dot files are always included now, and `.hidden` sorts ahead of
    // `report.md` in that order.
    assert_eq!(entries[0]["name"], "src");
    assert_eq!(entries[0]["isDir"], true);
    assert_eq!(entries[0]["mimeType"], "", "a directory has no type");
    assert_eq!(entries[1]["name"], ".hidden");
    let report = entries
        .iter()
        .find(|entry| entry["name"] == "report.md")
        .expect("report.md is in the listing");
    assert_eq!(report["mimeType"], "text/markdown");
    assert_eq!(report["exists"], true);
    assert_eq!(report["size"], 10);

    // One level down, by passing an entry's own path back.
    let json = data(
        meta_in(workdir.path()),
        r#"{ run { files(source: WORKDIR, path: "src") { path parent results { name } } } }"#,
    )
    .await;
    assert_eq!(json["run"]["files"]["results"][0]["name"], "main.rs");
    assert!(
        json["run"]["files"]["parent"].as_str().is_some(),
        "a level down has somewhere to go back to"
    );

    // Dot files are excluded with a filter naming them out.
    let json = data(
        meta_in(workdir.path()),
        r#"{ run { files(source: WORKDIR, filter: { not: { name: { startsWith: "." } } }) {
             results { name }
           } } }"#,
    )
    .await;
    assert!(
        !json["run"]["files"]["results"]
            .as_array()
            .expect("results")
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
        r#"{ run { files { modifyingToolCallCount isModifiedFilesTruncated
             results { name exists isOutsideWorkdir } } } }"#,
    )
    .await;
    let listing = &json["run"]["files"];
    // Not a file count: a run that edits one file three times records three.
    assert_eq!(listing["modifyingToolCallCount"], 5);
    assert_eq!(listing["isModifiedFilesTruncated"], false);
    let entries = listing["results"].as_array().expect("results");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["exists"], true);
    assert_eq!(entries[1]["name"], "gone.txt");
    assert_eq!(entries[1]["exists"], false, "reported, not filtered away");
}

/// `files` is paged and filtered like every other listing: a cursor resumes
/// it, a page over the cap is refused, and a filter nested too deep is
/// refused rather than walked.
#[tokio::test]
async fn files_are_paged_and_filtered_like_any_other_listing() {
    let workdir = tempfile::tempdir().expect("a workdir");
    std::fs::write(workdir.path().join("a.txt"), "a").expect("a file");
    std::fs::write(workdir.path().join("b.txt"), "b").expect("a file");
    std::fs::write(workdir.path().join("c.txt"), "c").expect("a file");

    let json = data(
        meta_in(workdir.path()),
        r#"{ run { files(source: WORKDIR, first: 2) { cursor results { name } } } }"#,
    )
    .await;
    let page = &json["run"]["files"];
    let cursor = page["cursor"].as_str().expect("more to come").to_string();
    let first_names: Vec<&str> = page["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter_map(|node| node["name"].as_str())
        .collect();

    let json = data(
        meta_in(workdir.path()),
        &format!(
            r#"{{ run {{ files(source: WORKDIR, first: 10, after: "{cursor}") {{ cursor results {{ name }} }} }} }}"#
        ),
    )
    .await;
    let page = &json["run"]["files"];
    assert!(page["cursor"].is_null(), "that was the rest");
    let rest_names: Vec<&str> = page["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter_map(|node| node["name"].as_str())
        .collect();
    assert_eq!(
        first_names.len() + rest_names.len(),
        3,
        "no entry read twice"
    );

    let answer = ask(
        meta_in(workdir.path()),
        "{ run { files(source: WORKDIR, first: 5000) { total } } }",
    )
    .await;
    assert!(
        answer
            .errors
            .first()
            .expect("a refusal")
            .message
            .contains("at most"),
    );

    let mut filter = r#"{ name: { eq: "a.txt" } }"#.to_string();
    for _ in 0..20 {
        filter = format!("{{ not: {filter} }}");
    }
    let answer = ask(
        meta_in(workdir.path()),
        &format!("{{ run {{ files(source: WORKDIR, filter: {filter}) {{ total }} }} }}"),
    )
    .await;
    assert!(
        answer
            .errors
            .first()
            .expect("a refusal")
            .message
            .contains("flatten it"),
    );

    // And a cursor that was never minted for this listing is refused rather
    // than resumed from whatever it decodes to.
    let answer = ask(
        meta_in(workdir.path()),
        r#"{ run { files(source: WORKDIR, after: "not-a-cursor") { total } } }"#,
    )
    .await;
    assert!(!answer.errors.is_empty(), "a cursor is checked");
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
        "{ run { files(source: WORKDIR) { results { name } } } }",
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
                 total cursor
                 results { at stage window { totalTokens } }
               } } }"#,
        )
        .await;
        let page = &json["run"]["contextHistory"];
        assert_eq!(page["total"], 3);
        assert!(page["cursor"].as_str().is_some(), "more to come");
        assert_eq!(page["results"].as_array().map(Vec::len), Some(2));
        assert_eq!(page["results"][0]["window"]["totalTokens"], 10);
        assert_eq!(page["results"][0]["stage"], "review");

        // The cursor carries on from where that page stopped.
        let cursor = page["cursor"].as_str().expect("a cursor");
        let json = data(
            meta_in(workdir.path()),
            &format!(
                r#"{{ run {{ contextHistory(first: 2, after: "{cursor}") {{
                     cursor
                     results {{ window {{ totalTokens }} }}
                   }} }} }}"#
            ),
        )
        .await;
        let page = &json["run"]["contextHistory"];
        assert_eq!(page["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(page["results"][0]["window"]["totalTokens"], 30);
        assert!(page["cursor"].is_null(), "that was the rest");

        // Newest first is the same points in the other order.
        let json = data(
            meta_in(workdir.path()),
            r#"{ run { contextHistory(orderBy: [{ field: SEQUENCE, direction: DESC }], first: 3) {
                 results { window { totalTokens } } } } }"#,
        )
        .await;
        let results = json["run"]["contextHistory"]["results"]
            .as_array()
            .expect("results");
        assert_eq!(results[0]["window"]["totalTokens"], 30);
        assert_eq!(results[2]["window"]["totalTokens"], 10);
    })
    .await;
}

/// A page of two reads two windows, whatever the journal holds.
///
/// A window is the largest thing this API materializes, and reading all of one
/// run's before trimming to a page of two is the whole journal in memory for
/// two rows of it. With no filter every point is on the listing, so which ones
/// the page holds is arithmetic over their own positions and only those are
/// read. A filter has no such shortcut: whether a point matches is a question
/// about the point, so every one of them is read.
#[tokio::test]
async fn an_unfiltered_history_page_reads_only_its_own_windows() {
    crate::runstate::with_isolated_runs_dir_async("graphql-history-reads", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let mut meta = meta_in(workdir.path());
        meta.run_id = "hist8".to_string();
        create_run(&meta).expect("run written");
        write_journal(&meta, &[10, 20, 30, 40, 50, 60, 70, 80, 90, 100]);
        let counted = || crate::commands::serve::testutil::windows_read_for("hist8");

        let before = counted();
        let json = data(
            meta.clone(),
            r#"{ run { contextHistory(first: 2) {
                 total cursor results { window { totalTokens } } } } }"#,
        )
        .await;
        assert_eq!(counted() - before, 2, "a page of two is two windows");
        let page = &json["run"]["contextHistory"];
        assert_eq!(page["total"], 10, "the count is still the whole history");
        assert_eq!(page["results"][0]["window"]["totalTokens"], 10);
        assert_eq!(page["results"][1]["window"]["totalTokens"], 20);

        // Newest first reads the other end of the journal, and the same two.
        let before = counted();
        let json = data(
            meta.clone(),
            r#"{ run { contextHistory(first: 2,
                        orderBy: [{ field: SEQUENCE, direction: DESC }]) {
                 cursor results { window { totalTokens } } } } }"#,
        )
        .await;
        assert_eq!(counted() - before, 2);
        let page = &json["run"]["contextHistory"];
        assert_eq!(page["results"][0]["window"]["totalTokens"], 100);
        assert_eq!(page["results"][1]["window"]["totalTokens"], 90);

        // And the cursor carries on downwards from there, reading two more.
        let cursor = page["cursor"].as_str().expect("a cursor").to_string();
        let before = counted();
        let json = data(
            meta.clone(),
            &format!(
                r#"{{ run {{ contextHistory(first: 2, after: "{cursor}",
                          orderBy: [{{ field: SEQUENCE, direction: DESC }}]) {{
                     results {{ window {{ totalTokens }} }} }} }} }}"#
            ),
        )
        .await;
        assert_eq!(counted() - before, 2);
        let page = &json["run"]["contextHistory"];
        assert_eq!(page["results"][0]["window"]["totalTokens"], 80);
        assert_eq!(page["results"][1]["window"]["totalTokens"], 70);

        // A filter is a question about each point, so each one is read.
        let before = counted();
        let json = data(
            meta,
            r#"{ run { contextHistory(first: 2, filter: { stage: { eq: "review" } }) {
                 total results { window { totalTokens } } } } }"#,
        )
        .await;
        assert_eq!(counted() - before, 10);
        assert_eq!(json["run"]["contextHistory"]["total"], 10);
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

/// A filter nested past the depth limit is refused rather than walked.
#[tokio::test]
async fn a_context_history_filter_past_the_depth_limit_is_refused() {
    let workdir = tempfile::tempdir().expect("a workdir");
    let mut filter = r#"{ stage: { eq: "plan" } }"#.to_string();
    for _ in 0..20 {
        filter = format!("{{ not: {filter} }}");
    }
    let answer = ask(
        meta_in(workdir.path()),
        &format!("{{ run {{ contextHistory(filter: {filter}) {{ total }} }} }}"),
    )
    .await;
    assert!(
        answer
            .errors
            .first()
            .expect("a refusal")
            .message
            .contains("flatten it")
    );
}

/// A run with no journal has no history, and says so with an empty page
/// rather than an error - the same as every other listing on a run that
/// recorded nothing.
#[tokio::test]
async fn a_run_with_no_journal_has_no_history() {
    crate::runstate::with_isolated_runs_dir_async("graphql-history-none", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        create_run(&meta_in(workdir.path())).expect("run written");
        let json = data(
            meta_in(workdir.path()),
            "{ run { contextHistory(first: 5) { total results { at } } } }",
        )
        .await;
        assert_eq!(json["run"]["contextHistory"]["total"], 0);
        assert_eq!(
            json["run"]["contextHistory"]["results"]
                .as_array()
                .map(Vec::len),
            Some(0)
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

/// A byte link is minted per hash and per name, with a download form.
///
/// Signed rather than authorized: the byte route checks the signature, which is
/// what lets a link work in an `<img src>` or a download button, where a header
/// cannot be set.
#[tokio::test]
async fn the_byte_links_carry_their_own_permission() {
    let workdir = tempfile::tempdir().expect("a workdir");
    let json = data(
        meta_in(workdir.path()),
        r#"{ run {
             blob: blobUrl(sha256: "abc123")
             download: blobUrl(sha256: "abc123", download: true)
             artifact: artifactUrl(name: "report.md")
           } }"#,
    )
    .await;
    let blob = json["run"]["blob"].as_str().expect("a link");
    assert!(
        blob.starts_with("/api/agents/reader/blobs/abc123?"),
        "{blob}"
    );
    assert!(blob.contains("exp=") && blob.contains("sig="), "{blob}");
    assert!(!blob.contains("download"), "inline by default: {blob}");

    let download = json["run"]["download"].as_str().expect("a link");
    assert!(download.contains("download=1"), "{download}");
    // A different query means a different signature: the signature covers the
    // whole path, so a client cannot add the download flag to an inline link.
    assert_ne!(
        blob.split("sig=").nth(1),
        download.split("sig=").nth(1),
        "the signature covers the query too"
    );

    let artifact = json["run"]["artifact"].as_str().expect("a link");
    assert!(
        artifact.starts_with("/api/agents/reader/artifacts/report.md?"),
        "{artifact}"
    );
}

/// Whether a message reaches the run is read from the stage it is in.
#[tokio::test]
async fn whether_messages_reach_the_run_comes_from_its_stage() {
    crate::runstate::with_isolated_runs_dir_async("graphql-accepts", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let mut meta = meta_in(workdir.path());
        meta.current_stage = "review".to_string();
        crate::runstate::create_run(&meta).expect("run written");
        // The run's own snapshot, which is what this reads: the installed file
        // may say something else by now.
        std::fs::write(
            crate::runstate::run_dir(&meta.run_id)
                .join(leviath_core::files::BLUEPRINT_SNAPSHOT_FILE),
            "[agent]\nname = \"coder\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
             \n[context.regions.work]\nkind = \"temporary\"\nmax_tokens = 100\n\
             \n[stages.review]\nmode = \"autonomous\"\naccepts_messages = false\n\
             \n[stages.build]\nmode = \"autonomous\"\n",
        )
        .expect("a snapshot");

        let json = data(meta.clone(), "{ run { acceptsMessages } }").await;
        assert_eq!(
            json["run"]["acceptsMessages"], false,
            "the stage it is in says no"
        );

        // Another stage of the same blueprint says otherwise, which is the point
        // of reading the stage rather than the blueprint.
        let mut building = meta.clone();
        building.current_stage = "build".to_string();
        let json = data(building, "{ run { acceptsMessages } }").await;
        assert_eq!(json["run"]["acceptsMessages"], true);
    })
    .await;
}

/// A run whose blueprint cannot be read answers null rather than no.
///
/// Unknown and no are different: a console that greyed out its message box on a
/// failed read would be wrong wherever the blueprint is simply elsewhere.
#[tokio::test]
async fn an_unreadable_blueprint_leaves_it_unknown() {
    crate::runstate::with_isolated_runs_dir_async("graphql-accepts-none", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let meta = meta_in(workdir.path());
        crate::runstate::create_run(&meta).expect("run written");
        let json = data(meta, "{ run { acceptsMessages } }").await;
        assert!(json["run"]["acceptsMessages"].is_null());
    })
    .await;
}

/// The parts a run holds come back with a link each, and a part the context
/// names but the store lacks comes back without one.
///
/// A missing link is the honest answer: the part is recorded, the bytes are not
/// there, and a link that 404s later is worse than no link now.
#[tokio::test]
async fn the_stored_parts_come_back_with_links() {
    use leviath_core::mime::{Blob, BlobStore as _, MimeRegistry, MimeType, Part};
    use leviath_core::region::EntryContent;
    use leviath_core::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot};

    crate::runstate::with_isolated_runs_dir_async("graphql-blobs", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let meta = meta_in(workdir.path());
        crate::runstate::create_run(&meta).expect("run written");

        let registry = MimeRegistry::builtin();
        let store = leviath_runtime::blob_store::FsBlobStore::new(crate::runstate::runs_dir());
        let png = Blob::new(
            MimeType::parse("image/png").expect("a type"),
            b"\x89PNG\r\n\x1a\nhero".to_vec(),
        )
        .named("hero.png");
        let stored = Part::stored(
            store
                .put(&meta.run_id, &png, &registry)
                .expect("the part stores"),
        )
        .named("hero.png");
        let lost = Part::stored(leviath_core::mime::BlobRef {
            sha256: "e".repeat(64),
            mime_type: MimeType::parse("audio/wav").expect("a type"),
            size: 3,
            width: None,
            height: None,
            duration_ms: Some(10),
            tokens: 1,
            stand_in: "[audio/wav] lost.wav".to_string(),
        })
        .named("lost.wav");
        let entry = |content: EntryContent| RegionEntrySnapshot {
            content,
            tokens: 1,
            kind: Default::default(),
            metadata: None,
            key: None,
            taint: leviath_core::taint::TaintLevel::Public,
            reasoning: None,
        };
        crate::runstate::write_context_snapshot(
            &meta.run_id,
            &ContextSnapshot {
                stage_name: "review".to_string(),
                total_tokens: 2,
                max_tokens: 100,
                regions: vec![RegionSnapshot {
                    name: "task".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 2,
                    max_tokens: 100,
                    entries: vec![entry(EntryContent::from_parts(vec![stored, lost]))],
                    description: None,
                }],
            },
        )
        .expect("a snapshot");

        let json = data(
            meta,
            "{ run { blobs { results { sha256 mimeType name size tokens regions stored url
                 width height durationMs } } } }",
        )
        .await;
        let blobs = json["run"]["blobs"]["results"]
            .as_array()
            .expect("the parts");
        assert_eq!(blobs.len(), 2);
        let picture = blobs
            .iter()
            .find(|blob| blob["mimeType"] == "image/png")
            .expect("the picture");
        assert_eq!(picture["name"], "hero.png");
        assert_eq!(picture["stored"], true);
        assert_eq!(picture["regions"][0], "task");
        assert!(
            picture["url"]
                .as_str()
                .is_some_and(|url| url.contains("/blobs/") && url.contains("sig=")),
            "a stored part has a link: {picture}"
        );
        let missing = blobs
            .iter()
            .find(|blob| blob["mimeType"] == "audio/wav")
            .expect("the lost one");
        assert_eq!(missing["stored"], false);
        assert!(
            missing["url"].is_null(),
            "no link for bytes that are not there: {missing}"
        );
        assert_eq!(missing["durationMs"], 10);
    })
    .await;
}

/// The files a run handed back come with a link each and the path they were
/// written at.
#[tokio::test]
async fn the_artifacts_come_back_with_links_and_paths() {
    let workdir = tempfile::tempdir().expect("a workdir");
    let mut meta = meta_in(workdir.path());
    meta.final_output = Some(leviath_core::output::FinalOutputDescriptor {
        format: None,
        stage: "review".to_string(),
        submitted_at: 1_788_924_600,
        bytes: 4,
        truncated: false,
        artifacts: vec![leviath_core::output::Artifact {
            name: "report".to_string(),
            path: "out/report.md".to_string(),
            mime_type: leviath_core::mime::MimeType::parse("text/markdown").expect("a type"),
            size: 12,
            sha256: String::new(),
        }],
    });

    let json = data(
        meta,
        "{ run { artifacts { results { name mimeType size sha256 path url } } } }",
    )
    .await;
    let artifact = &json["run"]["artifacts"]["results"][0];
    assert_eq!(artifact["name"], "report");
    assert_eq!(artifact["path"], "out/report.md");
    assert_eq!(artifact["mimeType"], "text/markdown");
    assert_eq!(artifact["size"], 12);
    // An empty hash is null rather than an empty string: nothing computed it,
    // which is different from computing it to nothing.
    assert!(artifact["sha256"].is_null());
    assert!(
        artifact["url"]
            .as_str()
            .is_some_and(|url| url.contains("/artifacts/report")),
        "{artifact}"
    );
}

/// The parent is a lookup in the index rather than a read, so walking up a
/// fan-out costs nothing per level.
#[tokio::test]
async fn the_parent_comes_from_the_index() {
    crate::runstate::with_isolated_runs_dir_async("graphql-parent", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let mut parent = meta_in(workdir.path());
        parent.run_id = "root".to_string();
        crate::runstate::create_run(&parent).expect("run written");
        let mut child = meta_in(workdir.path());
        child.parent_run_id = Some("root".to_string());
        crate::runstate::create_run(&child).expect("run written");

        let json = data(child, "{ run { parent { id blueprintName } } }").await;
        assert_eq!(json["run"]["parent"]["id"], "root");

        // A run nobody started has no parent, and a parent that is not in the
        // index reads the same way: null rather than an error, because a run
        // whose parent was deleted is an ordinary record.
        let json = data(parent, "{ run { parent { id } } }").await;
        assert!(json["run"]["parent"].is_null());

        let workdir = tempfile::tempdir().expect("a workdir");
        let mut orphan = meta_in(workdir.path());
        orphan.run_id = "orphan".to_string();
        orphan.parent_run_id = Some("long-gone".to_string());
        let json = data(orphan, "{ run { parent { id } } }").await;
        assert!(json["run"]["parent"].is_null());
    })
    .await;
}

/// What a run is parked on comes from the daemon, and an unreachable daemon is a
/// refusal rather than "nothing is waiting".
#[tokio::test]
async fn what_a_run_waits_on_comes_from_the_daemon() {
    let workdir = tempfile::tempdir().expect("a workdir");
    let answer = ask(
        meta_in(workdir.path()),
        "{ run { openInteraction { prompt } } }",
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
        Some("\"DAEMON_UNAVAILABLE\"".to_string()),
        "silence from the daemon is not an empty inbox"
    );
}

/// A file read past the end reports how far the file goes, and a window that
/// ends mid-character is trimmed so the windows line up.
#[tokio::test]
async fn a_window_is_trimmed_to_character_boundaries() {
    let workdir = tempfile::tempdir().expect("a workdir");
    // Two-byte characters, so a window boundary can land inside one.
    std::fs::write(workdir.path().join("text.md"), "éé".repeat(8)).expect("a file");

    let json = data(
        meta_in(workdir.path()),
        r#"{ run { fileContent(path: "text.md", offset: 3) { offset content truncated } } }"#,
    )
    .await;
    let window = &json["run"]["fileContent"];
    // The offset moved forward to the next character, so the text reads as text
    // rather than starting with half a character.
    assert_eq!(window["offset"], 4, "{window}");
    assert!(
        window["content"]
            .as_str()
            .is_some_and(|text| text.starts_with('é')),
        "{window}"
    );
}

/// A listing of a directory outside the run's working directory is refused, the
/// same way a read of a file outside it is.
#[tokio::test]
async fn a_listing_outside_the_workdir_is_refused() {
    let workdir = tempfile::tempdir().expect("a workdir");
    let answer = ask(
        meta_in(workdir.path()),
        r#"{ run { files(source: WORKDIR, path: "../elsewhere") { path } } }"#,
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
        Some("\"FORBIDDEN\"".to_string())
    );
}

/// A stage record carries its per-region peaks, which is what a stage is judged
/// on rather than the window as it stands now.
#[tokio::test]
async fn a_stage_record_carries_its_region_peaks() {
    crate::runstate::with_isolated_runs_dir_async("graphql-peaks", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let meta = meta_in(workdir.path());
        crate::runstate::create_run(&meta).expect("run written");
        let mut record = leviath_core::run_meta::StageRecord::new("review".to_string(), 0);
        record.region_tokens = std::collections::BTreeMap::from([("plan".to_string(), 120usize)]);
        record.visits = vec![leviath_core::run_meta::StageVisitRecord::opened_at(
            100,
            "v-one".to_string(),
        )];
        crate::runstate::write_stages_index(&meta.run_id, &[record]).expect("the ledger");

        let json = data(
            meta,
            "{ run { stages { results { name regionPeaks { region tokens }
                 visits { enteredAt leftAt inProgress } } } } }",
        )
        .await;
        let stage = &json["run"]["stages"]["results"][0];
        assert_eq!(stage["name"], "review");
        assert_eq!(stage["regionPeaks"][0]["region"], "plan");
        assert_eq!(stage["regionPeaks"][0]["tokens"], 120);
        // The visit in progress says so, which is the same fact as `leftAt`
        // being null said the way a list is filtered on.
        assert_eq!(stage["visits"][0]["inProgress"], true);
        assert!(stage["visits"][0]["leftAt"].is_null());
    })
    .await;
}

/// `stages` takes the same filter, `orderBy` and cursor every listing in this
/// schema takes, not just the bare list it used to be.
#[tokio::test]
async fn stages_are_filtered_ordered_and_paged_with_a_cursor() {
    crate::runstate::with_isolated_runs_dir_async("graphql-stages-listing", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let meta = meta_in(workdir.path());
        crate::runstate::create_run(&meta).expect("run written");
        crate::runstate::write_stages_index(
            &meta.run_id,
            &[
                leviath_core::run_meta::StageRecord::new("plan".to_string(), 0),
                leviath_core::run_meta::StageRecord::new("build".to_string(), 1),
            ],
        )
        .expect("the ledger");

        // Declared order by default.
        let json = data(meta.clone(), "{ run { stages(first: 1) { total cursor results { name } } } }").await;
        let page = &json["run"]["stages"];
        assert_eq!(page["total"], 2);
        assert_eq!(page["results"][0]["name"], "plan");
        let cursor = page["cursor"].as_str().expect("more to come").to_string();

        // The cursor resumes the rest.
        let json = data(
            meta.clone(),
            &format!(r#"{{ run {{ stages(first: 10, after: "{cursor}") {{ cursor results {{ name }} }} }} }}"#),
        )
        .await;
        let page = &json["run"]["stages"];
        assert!(page["cursor"].is_null(), "that was the rest");
        assert_eq!(page["results"][0]["name"], "build");

        // Explicit order reverses declared order.
        let json = data(
            meta.clone(),
            "{ run { stages(orderBy: [{ field: INDEX, direction: DESC }]) { results { name } } } }",
        )
        .await;
        let names: Vec<&str> = json["run"]["stages"]["results"]
            .as_array()
            .expect("results")
            .iter()
            .filter_map(|node| node["name"].as_str())
            .collect();
        assert_eq!(names, vec!["build", "plan"]);

        // A filter narrows the ledger to the one stage named.
        let json = data(
            meta.clone(),
            r#"{ run { stages(filter: { name: { eq: "build" } }) { total results { name } } } }"#,
        )
        .await;
        assert_eq!(json["run"]["stages"]["total"], 1);
        assert_eq!(json["run"]["stages"]["results"][0]["name"], "build");

        // A page over the cap is refused, saying what the cap is.
        let answer = ask(meta.clone(), "{ run { stages(first: 5000) { total } } }").await;
        assert!(
            answer.errors.first().expect("a refusal").message.contains("at most 200"),
            "{:?}",
            answer.errors
        );

        // A filter nested past the depth limit is refused rather than walked.
        let mut filter = r#"{ name: { eq: "plan" } }"#.to_string();
        for _ in 0..20 {
            filter = format!("{{ not: {filter} }}");
        }
        let answer = ask(
            meta,
            &format!("{{ run {{ stages(filter: {filter}) {{ total }} }} }}"),
        )
        .await;
        assert!(
            answer.errors.first().expect("a refusal").message.contains("flatten it"),
            "{:?}",
            answer.errors
        );
    })
    .await;
}

/// `blobs` and `artifacts` take an explicit `orderBy` the same way `stages`
/// does.
#[tokio::test]
async fn blobs_and_artifacts_can_be_ordered_explicitly() {
    crate::runstate::with_isolated_runs_dir_async("graphql-blobs-artifacts-order", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let mut meta = meta_in(workdir.path());
        meta.final_output = Some(leviath_core::output::FinalOutputDescriptor {
            format: None,
            stage: "review".to_string(),
            submitted_at: 1_788_924_600,
            bytes: 4,
            truncated: false,
            artifacts: vec![
                leviath_core::output::Artifact {
                    name: "b.txt".to_string(),
                    path: "b.txt".to_string(),
                    mime_type: leviath_core::mime::MimeType::parse("text/plain").expect("a type"),
                    size: 1,
                    sha256: String::new(),
                },
                leviath_core::output::Artifact {
                    name: "a.txt".to_string(),
                    path: "a.txt".to_string(),
                    mime_type: leviath_core::mime::MimeType::parse("text/plain").expect("a type"),
                    size: 1,
                    sha256: String::new(),
                },
            ],
        });
        crate::runstate::create_run(&meta).expect("run written");

        let json = data(
            meta.clone(),
            "{ run { blobs(orderBy: [{ field: SHA_256, direction: DESC }]) { total } } }",
        )
        .await;
        assert_eq!(json["run"]["blobs"]["total"], 0, "no parts stored");

        let json = data(
            meta,
            "{ run { artifacts(orderBy: [{ field: NAME, direction: ASC }]) { results { name } } } }",
        )
        .await;
        let names: Vec<&str> = json["run"]["artifacts"]["results"]
            .as_array()
            .expect("results")
            .iter()
            .filter_map(|node| node["name"].as_str())
            .collect();
        assert_eq!(names, vec!["a.txt", "b.txt"], "ascending, by name");
    })
    .await;
}

/// `logs` reads one stage by index, every stage in order, or the stage the
/// run is on now when `stage` is omitted, from whichever stream is asked for.
#[tokio::test]
async fn logs_read_one_stage_every_stage_or_the_current_one() {
    crate::runstate::with_isolated_runs_dir_async("graphql-logs", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let mut meta = meta_in(workdir.path());
        meta.current_stage = "build".to_string();
        meta.stage_index = 1;
        crate::runstate::create_run(&meta).expect("run written");
        crate::runstate::write_stages_index(
            &meta.run_id,
            &[
                leviath_core::run_meta::StageRecord::new("plan".to_string(), 0),
                leviath_core::run_meta::StageRecord::new("build".to_string(), 1),
            ],
        )
        .expect("the ledger");
        crate::runstate::append_stage_output("reader", 0, "plan output");
        crate::runstate::append_stage_output("reader", 1, "build output");
        crate::runstate::append_stage_log("reader", 0, "[tool] plan");
        crate::runstate::append_stage_log("reader", 1, "[tool] build");

        // Omitted `stage` means the stage the run is on now: the last one in
        // the ledger.
        let json = data(meta.clone(), "{ run { logs } }").await;
        assert_eq!(json["run"]["logs"], "build output\n");

        // One stage by index.
        let json = data(meta.clone(), "{ run { logs(stage: { index: 0 }) } }").await;
        assert_eq!(json["run"]["logs"], "plan output\n");

        // Every stage, in order.
        let json = data(meta.clone(), "{ run { logs(stage: { all: true }) } }").await;
        assert_eq!(
            json["run"]["logs"],
            "===== stage 0: plan =====\nplan output\n\n===== stage 1: build =====\nbuild output\n"
        );

        // The operational stream instead of the output one.
        let json = data(
            meta.clone(),
            "{ run { logs(stage: { index: 1 }, stream: OPERATIONAL) } }",
        )
        .await;
        assert_eq!(json["run"]["logs"], "[tool] build\n");

        // A negative index is refused.
        let answer = ask(meta.clone(), "{ run { logs(stage: { index: -1 }) } }").await;
        assert!(
            answer
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("cannot be negative")
        );

        // A negative tailBytes is refused.
        let answer = ask(meta, "{ run { logs(tailBytes: -1) } }").await;
        assert!(
            answer
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("cannot be negative")
        );
    })
    .await;
}

/// A listing of an absolute path inside the run's working directory works, and
/// the entries come back relative to it.
#[tokio::test]
async fn an_absolute_path_inside_the_workdir_lists() {
    let workdir = tempfile::tempdir().expect("a workdir");
    std::fs::create_dir(workdir.path().join("src")).expect("a directory");
    std::fs::write(workdir.path().join("src/main.rs"), "fn main() {}").expect("a file");
    let absolute = workdir.path().join("src").to_string_lossy().into_owned();

    let json = data(
        meta_in(workdir.path()),
        &format!(
            r#"{{ run {{ files(source: WORKDIR, path: "{}") {{ results {{ name path }} }} }} }}"#,
            absolute.replace('\\', "\\\\")
        ),
    )
    .await;
    let entry = &json["run"]["files"]["results"][0];
    assert_eq!(entry["name"], "main.rs");
    // The host's own separator: this path goes back to this host, and a Windows
    // server answers `src\main.rs`.
    let expected = std::path::Path::new("src")
        .join("main.rs")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        entry["path"], expected,
        "relative to the working directory, so it can be passed back"
    );
}

/// A part's dimensions and duration come through where the record has them.
#[tokio::test]
async fn a_parts_dimensions_come_through() {
    use leviath_core::mime::{MimeType, Part};
    use leviath_core::region::EntryContent;
    use leviath_core::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot};

    crate::runstate::with_isolated_runs_dir_async("graphql-blob-dims", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let meta = meta_in(workdir.path());
        crate::runstate::create_run(&meta).expect("run written");
        let picture = Part::stored(leviath_core::mime::BlobRef {
            sha256: "a".repeat(64),
            mime_type: MimeType::parse("image/png").expect("a type"),
            size: 2_048,
            width: Some(1_024),
            height: Some(768),
            duration_ms: Some(0),
            tokens: 400,
            stand_in: "[image/png 1024x768, 2 KB] hero.png".to_string(),
        })
        .named("hero.png");
        crate::runstate::write_context_snapshot(
            &meta.run_id,
            &ContextSnapshot {
                stage_name: "review".to_string(),
                total_tokens: 400,
                max_tokens: 1_000,
                regions: vec![RegionSnapshot {
                    name: "task".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 400,
                    max_tokens: 1_000,
                    entries: vec![RegionEntrySnapshot {
                        content: EntryContent::from_parts(vec![picture]),
                        tokens: 400,
                        kind: Default::default(),
                        metadata: None,
                        key: None,
                        taint: leviath_core::taint::TaintLevel::Public,
                        reasoning: None,
                    }],
                    description: None,
                }],
            },
        )
        .expect("a snapshot");

        let json = data(
            meta,
            "{ run { blobs { results { width height durationMs tokens } } } }",
        )
        .await;
        let blob = &json["run"]["blobs"]["results"][0];
        assert_eq!(blob["width"], 1_024);
        assert_eq!(blob["height"], 768);
        assert_eq!(blob["durationMs"], 0);
        // The listing counts the tokens from the type and the size rather than
        // echoing what the record happened to store, so this is a number rather
        // than the one above.
        assert!(blob["tokens"].as_i64().is_some_and(|n| n > 0), "{blob}");
    })
    .await;
}

/// A file read that reaches the end of a larger file says where the next window
/// starts, and the window that follows it finishes the file.
#[tokio::test]
async fn a_truncated_window_says_where_to_continue() {
    let workdir = tempfile::tempdir().expect("a workdir");
    // Larger than one window, so the first read is truncated.
    let size = crate::commands::serve::core::files::MAX_FILE_READ_BYTES as usize + 16;
    std::fs::write(workdir.path().join("big.txt"), "a".repeat(size)).expect("a file");

    let json = data(
        meta_in(workdir.path()),
        r#"{ run { fileContent(path: "big.txt") { size nextOffset truncated } } }"#,
    )
    .await;
    let window = &json["run"]["fileContent"];
    assert_eq!(window["truncated"], true);
    assert_eq!(
        window["nextOffset"],
        crate::commands::serve::core::files::MAX_FILE_READ_BYTES as i64,
        "{window}"
    );

    let rest = data(
        meta_in(workdir.path()),
        &format!(
            r#"{{ run {{ fileContent(path: "big.txt", offset: {}) {{ truncated nextOffset }} }} }}"#,
            crate::commands::serve::core::files::MAX_FILE_READ_BYTES
        ),
    )
    .await;
    assert_eq!(rest["run"]["fileContent"]["truncated"], false);
    assert!(rest["run"]["fileContent"]["nextOffset"].is_null());
}

/// Before the first stage is entered, the answer is the entry stage's.
///
/// That is the stage a message would arrive in, so reading the blueprint's entry
/// rather than answering unknown is what makes the field useful on a run that has
/// only just been queued.
#[tokio::test]
async fn a_run_yet_to_enter_a_stage_answers_from_its_entry_stage() {
    crate::runstate::with_isolated_runs_dir_async("graphql-accepts-entry", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let mut meta = meta_in(workdir.path());
        meta.current_stage = String::new();
        crate::runstate::create_run(&meta).expect("run written");
        std::fs::write(
            crate::runstate::run_dir(&meta.run_id)
                .join(leviath_core::files::BLUEPRINT_SNAPSHOT_FILE),
            "[agent]\nname = \"coder\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
             \n[context.regions.work]\nkind = \"temporary\"\nmax_tokens = 100\n\
             \n[stages.review]\nmode = \"autonomous\"\naccepts_messages = false\n\
             \n[stages.build]\nmode = \"autonomous\"\n",
        )
        .expect("a snapshot");

        let json = data(meta, "{ run { acceptsMessages } }").await;
        assert_eq!(
            json["run"]["acceptsMessages"], false,
            "the entry stage is `review`, which takes no messages"
        );
    })
    .await;
}

/// A page of children larger than the cap is refused.
///
/// The cap is what stops one query walking a whole sub-agent tree, so it has to
/// hold on the nested field as well as on the root listing.
#[tokio::test]
async fn a_child_page_over_the_cap_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-children-cap", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let meta = meta_in(workdir.path());
        crate::runstate::create_run(&meta).expect("run written");

        let answer = ask(meta.clone(), "{ run { children(first: 5000) { total } } }").await;
        let message = &answer.errors.first().expect("a refusal").message;
        assert!(message.contains("at most"), "{message}");

        let answer = ask(meta, "{ run { children(first: 0) { total } } }").await;
        let message = &answer.errors.first().expect("a refusal").message;
        assert!(message.contains("at least 1"), "{message}");
    })
    .await;
}

/// A context-history cursor from somewhere else is refused rather than followed.
#[tokio::test]
async fn a_history_cursor_from_elsewhere_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-history-cursor", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let meta = meta_in(workdir.path());
        crate::runstate::create_run(&meta).expect("run written");
        write_journal(&meta, &[10, 20]);

        let answer = ask(
            meta.clone(),
            r#"{ run { contextHistory(first: 1, after: "not-from-here") { total } } }"#,
        )
        .await;
        assert!(
            !answer.errors.is_empty(),
            "a cursor this listing did not mint is not followed"
        );

        // The filtered walk reads the same cursor and refuses it the same way.
        let filtered = ask(
            meta,
            r#"{ run { contextHistory(first: 1, after: "not-from-here",
                        filter: { stage: { eq: "review" } }) { total } } }"#,
        )
        .await;
        assert!(!filtered.errors.is_empty(), "{:?}", filtered.errors);
    })
    .await;
}

/// A revision taken from the history reads back as exactly that window, and goes
/// on meaning it however far the run moves afterwards.
///
/// This is the guarantee the whole debugger rests on. A revision is derived from
/// the window's contents, so the read is immutable: it can only ever answer with
/// the content the revision was minted from, never with what the run holds now.
#[tokio::test]
async fn a_revision_reads_back_the_window_it_names() {
    crate::runstate::with_isolated_runs_dir_async("graphql-revision", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let meta = meta_in(workdir.path());
        create_run(&meta).expect("run written");
        write_journal(&meta, &[10, 20, 30]);

        // Every point carries its own revision, and no two of these windows
        // share one.
        let json = data(
            meta_in(workdir.path()),
            "{ run { contextHistory(first: 3) { results { window { revision \
             totalTokens } } } } }",
        )
        .await;
        let results = json["run"]["contextHistory"]["results"]
            .as_array()
            .expect("results")
            .clone();
        let revisions: Vec<&str> = results
            .iter()
            .map(|node| node["window"]["revision"].as_str().expect("a revision"))
            .collect();
        assert_eq!(revisions.len(), 3);
        assert!(
            revisions.iter().all(|r| r.starts_with("cw1-")),
            "{revisions:?}"
        );
        assert_eq!(
            revisions
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3,
            "three windows, three names"
        );

        // The middle one resolves to the middle window, not to the run's latest.
        let json = data(
            meta_in(workdir.path()),
            &format!(
                r#"{{ run {{ contextSnapshot(revision: "{}") {{
                     at stage window {{ revision totalTokens }}
                   }} }} }}"#,
                revisions[1]
            ),
        )
        .await;
        let point = &json["run"]["contextSnapshot"];
        assert_eq!(point["window"]["totalTokens"], 20);
        assert_eq!(point["window"]["revision"], revisions[1]);
        assert_eq!(point["stage"], "review");
        assert_eq!(point["at"], 2);

        // And a revision this run never held is null rather than the nearest
        // thing to it.
        let json = data(
            meta_in(workdir.path()),
            r#"{ run { contextSnapshot(
                 revision: "cw1-00000000000000000000000000000000"
               ) { at } } }"#,
        )
        .await;
        assert!(json["run"]["contextSnapshot"].is_null());
    })
    .await;
}

/// A run with no journal has no window to name, and says so rather than failing.
#[tokio::test]
async fn a_run_with_no_journal_has_no_named_window() {
    crate::runstate::with_isolated_runs_dir_async("graphql-revision-empty", |_dir| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        let meta = meta_in(workdir.path());
        create_run(&meta).expect("run written");
        let json = data(
            meta_in(workdir.path()),
            r#"{ run { contextSnapshot(revision: "cw1-anything") { at } } }"#,
        )
        .await;
        assert!(json["run"]["contextSnapshot"].is_null());
    })
    .await;
}

/// Every function `#[mirror]` wrote for this module's types runs at least once.
///
/// The mirrors are straight lines of delegation, so running each of them once
/// is enough to measure all of them.
#[tokio::test]
async fn every_mirrored_function_runs() {
    use super::run_files::{FileEntry, FileSource, FileWindow};

    exercise_enum(&[FileSource::Modified, FileSource::Workdir]).await;

    let entry = FileEntry {
        name: "main.rs".to_string(),
        path: "src/main.rs".to_string(),
        is_dir: false,
        size: Some(BigInt(42)),
        exists: true,
        is_outside_workdir: false,
        mime_type: "text/x-rust".to_string(),
    };
    exercise(std::slice::from_ref(&entry)).await;
    exercise_list(std::slice::from_ref(&entry)).await;

    exercise(&[FileWindow {
        path: "/work/src/main.rs".to_string(),
        size: BigInt(100),
        offset: BigInt(0),
        next_offset: Some(BigInt(50)),
        content: "fn main() {}".to_string(),
        truncated: true,
    }])
    .await;
}
