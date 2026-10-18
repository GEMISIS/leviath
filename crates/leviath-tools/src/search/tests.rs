use super::*;
use std::fs;

fn make_tools(dir: &Path) -> BuiltinTools {
    BuiltinTools::new(ToolContext::new(dir.to_path_buf()))
}

/// A workdir with a little tree in it: two Rust files, a text file, a
/// nested directory.
fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "fn main() {\n    spawn_run();\n}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("src/nested/lib.rs"),
        "pub fn spawn_run() {}\n",
    )
    .unwrap();
    fs::write(dir.path().join("notes.txt"), "remember spawn_run\n").unwrap();
    dir
}

async fn search(tools: &BuiltinTools, args: Value) -> String {
    tools.execute("search_files", args).await.into_string()
}

async fn glob(tools: &BuiltinTools, args: Value) -> String {
    tools.execute("glob", args).await.into_string()
}

#[tokio::test]
async fn search_returns_matching_lines_relative_to_the_workdir_in_path_order() {
    let dir = tree();
    let out = search(&make_tools(dir.path()), json!({"pattern": "spawn_run"})).await;
    assert_eq!(
        out,
        "notes.txt:1: remember spawn_run\n\
         src/main.rs:2:     spawn_run();\n\
         src/nested/lib.rs:1: pub fn spawn_run() {}"
    );
}

#[tokio::test]
async fn search_reads_the_pattern_as_a_regex_unless_told_otherwise() {
    let dir = tree();
    let tools = make_tools(dir.path());
    let out = search(&tools, json!({"pattern": "fn \\w+\\(\\)"})).await;
    assert!(out.contains("src/main.rs:1: fn main() {"), "{out}");
    assert!(out.contains("src/nested/lib.rs:1:"), "{out}");

    let literal = search(&tools, json!({"pattern": "spawn_run();", "regex": false})).await;
    assert_eq!(literal, "src/main.rs:2:     spawn_run();");

    let bad = search(&tools, json!({"pattern": "("})).await;
    assert!(bad.starts_with("[error] 'pattern' is not a valid regular expression"));
    assert!(bad.contains("\"regex\": false"), "{bad}");
}

#[tokio::test]
async fn search_can_ignore_case() {
    let dir = tree();
    let tools = make_tools(dir.path());
    let missed = search(&tools, json!({"pattern": "REMEMBER"})).await;
    assert_eq!(missed, "No matches for \"REMEMBER\" under ''.");
    let found = search(
        &tools,
        json!({"pattern": "REMEMBER", "case_insensitive": true}),
    )
    .await;
    assert_eq!(found, "notes.txt:1: remember spawn_run");
}

#[tokio::test]
async fn the_glob_argument_narrows_by_name_or_by_path() {
    let dir = tree();
    let tools = make_tools(dir.path());
    let by_name = search(&tools, json!({"pattern": "spawn_run", "glob": "*.rs"})).await;
    assert!(!by_name.contains("notes.txt"), "{by_name}");
    assert!(by_name.contains("src/nested/lib.rs"), "{by_name}");

    let by_path = search(
        &tools,
        json!({"pattern": "spawn_run", "path": "src", "glob": "nested/**"}),
    )
    .await;
    assert_eq!(by_path, "src/nested/lib.rs:1: pub fn spawn_run() {}");

    let bad = search(&tools, json!({"pattern": "x", "glob": "a[b"})).await;
    assert!(
        bad.starts_with("[error] 'glob' is not a valid glob"),
        "{bad}"
    );
}

#[tokio::test]
async fn search_refuses_a_root_outside_the_workdir() {
    let dir = tree();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "spawn_run").unwrap();
    let tools = make_tools(dir.path());
    let out = search(
        &tools,
        json!({"pattern": "spawn_run", "path": outside.path().to_str().unwrap()}),
    )
    .await;
    assert!(out.contains("would escape the working directory"), "{out}");
    let up = search(&tools, json!({"pattern": "x", "path": "../"})).await;
    assert!(up.starts_with("[error]"), "{up}");
    let globbed = glob(
        &tools,
        json!({"pattern": "*", "path": outside.path().to_str().unwrap()}),
    )
    .await;
    assert!(
        globbed.contains("would escape the working directory"),
        "{globbed}"
    );
}

#[tokio::test]
async fn a_missing_root_or_pattern_is_an_error() {
    let dir = tree();
    let tools = make_tools(dir.path());
    assert_eq!(
        search(&tools, json!({"pattern": "x", "path": "nope"})).await,
        "[error] path 'nope' does not exist"
    );
    assert_eq!(
        search(&tools, json!({})).await,
        "[error] missing 'pattern' argument"
    );
    assert_eq!(
        glob(&tools, json!({})).await,
        "[error] missing 'pattern' argument"
    );
    assert_eq!(
        glob(&tools, json!({"pattern": "*", "path": "nope"})).await,
        "[error] path 'nope' does not exist"
    );
    let bad = glob(&tools, json!({"pattern": "a[b"})).await;
    assert!(
        bad.starts_with("[error] 'pattern' is not a valid glob"),
        "{bad}"
    );
}

/// What `.gitignore`, `.ignore` and a leading dot hide stays hidden unless
/// the call asks, and `.git` itself is never walked.
#[tokio::test]
async fn ignored_and_hidden_files_are_skipped_unless_asked_for() {
    let dir = tree();
    fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
    fs::write(dir.path().join(".ignore"), "*.log\n").unwrap();
    fs::create_dir_all(dir.path().join("target")).unwrap();
    fs::write(dir.path().join("target/out.rs"), "spawn_run").unwrap();
    fs::write(dir.path().join("build.log"), "spawn_run").unwrap();
    fs::write(dir.path().join(".hidden"), "spawn_run").unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git/HEAD"), "spawn_run").unwrap();
    let tools = make_tools(dir.path());

    let plain = search(&tools, json!({"pattern": "spawn_run"})).await;
    for hidden in ["target/out.rs", "build.log", ".hidden", ".git"] {
        assert!(!plain.contains(hidden), "{hidden} leaked: {plain}");
    }
    let all = search(
        &tools,
        json!({"pattern": "spawn_run", "include_ignored": true}),
    )
    .await;
    for shown in ["target/out.rs", "build.log", ".hidden"] {
        assert!(all.contains(shown), "{shown} missing: {all}");
    }
    assert!(!all.contains(".git/"), "{all}");

    let paths = glob(&tools, json!({"pattern": "**"})).await;
    assert!(!paths.contains("target"), "{paths}");
    let every = glob(&tools, json!({"pattern": "**", "include_ignored": true})).await;
    assert!(every.contains("target/out.rs"), "{every}");
    assert!(!every.contains(".git/"), "{every}");
}

#[tokio::test]
async fn binary_files_are_skipped() {
    let dir = tree();
    fs::write(dir.path().join("blob.bin"), b"spawn_run\0\x01\x02").unwrap();
    let out = search(&make_tools(dir.path()), json!({"pattern": "spawn_run"})).await;
    assert!(!out.contains("blob.bin"), "{out}");
}

#[tokio::test]
async fn context_lines_surround_each_match_and_groups_are_separated() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=12).map(|n| format!("line {n}\n")).collect();
    fs::write(dir.path().join("a.txt"), &body).unwrap();
    fs::write(dir.path().join("b.txt"), "line 1\n").unwrap();
    let out = search(
        &make_tools(dir.path()),
        json!({"pattern": "^line (2|3|10|1)$", "context_lines": 1}),
    )
    .await;
    assert_eq!(
        out,
        "a.txt:1: line 1\n\
         a.txt:2: line 2\n\
         a.txt:3: line 3\n\
         a.txt-4- line 4\n\
         --\n\
         a.txt-9- line 9\n\
         a.txt:10: line 10\n\
         a.txt-11- line 11\n\
         --\n\
         b.txt:1: line 1"
    );
}

#[tokio::test]
async fn max_results_stops_the_search_with_a_note() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hit\nhit\nhit\n").unwrap();
    fs::write(dir.path().join("b.txt"), "hit\n").unwrap();
    let out = search(
        &make_tools(dir.path()),
        json!({"pattern": "hit", "max_results": 2}),
    )
    .await;
    assert!(out.starts_with("a.txt:1: hit\na.txt:2: hit\n\n"), "{out}");
    assert!(out.contains("stopped after 2 matching lines"), "{out}");
    assert!(!out.contains("b.txt"), "{out}");
}

/// The caps, reached with a few bytes through the seam rather than a megabyte.
#[test]
fn the_output_and_file_caps_say_what_they_cut() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hit one\nhit two\nhit three\n").unwrap();
    let tools = make_tools(dir.path());
    let scope = tools.read_scope();
    let root = scope.workdir.clone();
    let matcher = regex::Regex::new("hit").unwrap();
    let opts = SearchOptions {
        context: 0,
        max_results: 100,
        include_ignored: false,
        filter: None,
    };
    let out = search_at(
        &scope,
        &root,
        &matcher,
        &opts,
        Caps {
            file_bytes: 1024,
            output_bytes: 20,
        },
    );
    assert!(out.starts_with("a.txt:1: hit one\n\n"), "{out}");
    assert!(out.contains("reached its 20-byte cap"), "{out}");

    let out = search_at(
        &scope,
        &root,
        &matcher,
        &opts,
        Caps {
            file_bytes: 10,
            output_bytes: 1024,
        },
    );
    assert!(out.starts_with("a.txt:1: hit one\n\n"), "{out}");
    assert!(out.contains("1 file(s) longer than 10 bytes"), "{out}");

    // A separator that does not fit stops the output the same way a line does.
    let with_context = SearchOptions { context: 1, ..opts };
    fs::write(dir.path().join("b.txt"), "hit\n").unwrap();
    let out = search_at(
        &scope,
        &root,
        &matcher,
        &with_context,
        Caps {
            file_bytes: 1024,
            output_bytes: 54,
        },
    );
    assert!(out.ends_with("byte cap. Narrow the pattern, path or glob to see the rest.]"));
    assert!(!out.contains("b.txt"), "{out}");
}

#[tokio::test]
async fn a_long_line_is_clipped() {
    let dir = tempfile::tempdir().unwrap();
    let long = format!("hit {}", "é".repeat(1000));
    fs::write(dir.path().join("a.txt"), &long).unwrap();
    let out = search(&make_tools(dir.path()), json!({"pattern": "hit"})).await;
    assert!(out.ends_with("..."), "{out}");
    assert!(out.chars().count() < 450, "{}", out.chars().count());
}

/// A `[read_paths]` grant opens a root outside the workdir to both tools,
/// exactly as it does for `read_file`, and those paths are shown in full.
#[tokio::test]
async fn a_granted_read_path_is_searchable() {
    let dir = tree();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("doc.md"), "spawn_run docs\n").unwrap();
    let entry = outside.path().to_str().unwrap().to_string();
    let compiled =
        leviath_core::ReadPathSet::compile(std::slice::from_ref(&entry), dir.path(), None, false)
            .unwrap();
    let tools = make_tools(dir.path());
    tools.set_read_paths(leviath_core::ReadPathPolicy {
        agent: "tester".into(),
        blueprint: compiled.clone(),
        grants: compiled,
        allow_blueprint: false,
    });
    let out = search(&tools, json!({"pattern": "spawn_run", "path": &entry})).await;
    let real = fs::canonicalize(outside.path()).unwrap().join("doc.md");
    assert_eq!(out, format!("{}:1: spawn_run docs", real.display()));
    let paths = glob(&tools, json!({"pattern": "*.md", "path": &entry})).await;
    assert_eq!(paths, real.display().to_string());
}

/// A path the fence refuses is never searched or listed, whatever the walk
/// met. Driven through the seam so it holds on every platform; the Unix test
/// below makes the same point with a real symlink.
#[test]
fn an_entry_the_fence_refuses_is_neither_searched_nor_listed() {
    let dir = tree();
    fs::write(dir.path().join("secret.txt"), "spawn_run").unwrap();
    fn refuses_secrets(path: &Path, _: &Path) -> bool {
        !path.to_string_lossy().contains("secret")
    }
    let tools = make_tools(dir.path());
    let scope = ReadScope {
        within: refuses_secrets,
        ..tools.read_scope()
    };
    let root = scope.workdir.clone();
    let opts = SearchOptions {
        context: 0,
        max_results: 100,
        include_ignored: false,
        filter: None,
    };
    let matcher = regex::Regex::new("spawn_run").unwrap();
    let out = search_at(&scope, &root, &matcher, &opts, Caps::REAL);
    assert!(!out.contains("secret"), "{out}");
    assert!(out.contains("notes.txt"), "{out}");
    let all = globset::Glob::new("**").unwrap().compile_matcher();
    let paths = glob_at(&scope, &root, &all, 100, false);
    assert!(!paths.contains("secret"), "{paths}");
    assert!(paths.contains("notes.txt"), "{paths}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_workdir_is_never_followed() {
    let dir = tree();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "spawn_run secret").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("leak.txt"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("leakdir")).unwrap();
    let tools = make_tools(dir.path());
    let out = search(&tools, json!({"pattern": "secret"})).await;
    assert_eq!(out, "No matches for \"secret\" under ''.");
    let paths = glob(&tools, json!({"pattern": "**"})).await;
    assert!(!paths.contains("leak"), "{paths}");
    let named = search(&tools, json!({"pattern": "secret", "path": "leakdir"})).await;
    assert!(named.contains("through a symlink"), "{named}");
}

#[cfg(windows)]
#[tokio::test]
async fn a_symlink_out_of_the_workdir_is_never_followed() {
    // Creating a symlink needs a privilege Windows CI does not have, so this
    // twin holds the refusal through the path a link would resolve to; the
    // seam test above covers the walk itself.
    let dir = tree();
    let tools = make_tools(dir.path());
    let named = search(&tools, json!({"pattern": "secret", "path": "..\\"})).await;
    assert!(named.starts_with("[error]"), "{named}");
}

#[tokio::test]
async fn glob_lists_matching_paths_sorted_with_directories_marked() {
    let dir = tree();
    let tools = make_tools(dir.path());
    assert_eq!(
        glob(&tools, json!({"pattern": "src/**/*.rs"})).await,
        "src/main.rs\nsrc/nested/lib.rs"
    );
    assert_eq!(
        glob(&tools, json!({"pattern": "*"})).await,
        "notes.txt\nsrc/"
    );
    assert_eq!(
        glob(&tools, json!({"pattern": "*.rs", "path": "src"})).await,
        "src/main.rs"
    );
    assert_eq!(
        glob(&tools, json!({"pattern": "*.py"})).await,
        "No paths match \"*.py\" under ''."
    );
    let capped = glob(&tools, json!({"pattern": "**", "max_results": 1})).await;
    assert!(
        capped.starts_with("notes.txt\n\n[truncated: 4 more paths"),
        "{capped}"
    );
}

/// `may_read` answers what `read_file` would: inside yes, outside no.
#[test]
fn may_read_is_the_read_fence() {
    let dir = tree();
    let tools = make_tools(dir.path());
    assert!(tools.may_read(&tools.workdir().join("src/main.rs")));
    assert!(!tools.may_read(&tools.workdir().join("../elsewhere")));
}

#[test]
fn windows_merge_when_they_touch_and_clamp_to_the_file() {
    assert_eq!(windows(&[0, 2, 9], 1, 10), vec![(0, 3), (8, 9)]);
    assert_eq!(windows(&[4], 0, 10), vec![(4, 4)]);
}
