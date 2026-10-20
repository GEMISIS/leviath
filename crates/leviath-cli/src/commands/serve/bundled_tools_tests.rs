use super::*;

use crate::commands::serve::testutil::{state_with_agent_paths, with_home};

/// The directories a default config names.
fn dirs() -> AgentDirs {
    AgentDirs::of(&crate::config::Config::default())
}

/// The global copy of `name` under `home`.
fn global_copy(home: &std::path::Path, name: &str) -> std::path::PathBuf {
    home.join(".leviath")
        .join("tools")
        .join(format!("{name}.rhai"))
}

/// A bundled tool lands in the global tools directory with this build's
/// bytes, and the outcome says what the install did each time.
#[tokio::test]
async fn a_bundled_tool_installs_globally_and_says_what_it_did() {
    with_home(|home| async move {
        let shipped = crate::bundled::bundled_tool("web_search").expect("web_search ships");

        let first = install_one(&dirs(), "web_search").expect("installs");
        assert_eq!(first.name, "web_search");
        assert_eq!(first.outcome, InstallOutcome::Installed);
        assert!(first.written.compiles, "{:?}", first.written.error);
        let path = global_copy(&home, "web_search");
        assert_eq!(first.written.path, path.display().to_string());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), shipped.source);

        // A copy that already matches is not rewritten: a run loading it at
        // that moment must not race a replacement. Backdating the file and
        // finding the date unmoved shows nothing touched it.
        let long_ago = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let again = install_one(&dirs(), "web_search").expect("installs again");
        assert_eq!(again.outcome, InstallOutcome::Unchanged);
        assert_eq!(again.written.path, path.display().to_string());
        assert!(again.written.compiles);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            long_ago
        );

        std::fs::write(&path, "// @tool web_search\n\"edited\"").unwrap();
        let replaced = install_one(&dirs(), "web_search").expect("replaces");
        assert_eq!(replaced.outcome, InstallOutcome::Replaced);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), shipped.source);
    })
    .await;
}

/// A name this build ships no tool under is a miss that names the ones it
/// does, and nothing is written.
#[tokio::test]
async fn an_unknown_bundled_tool_is_a_miss_naming_the_real_ones() {
    with_home(|home| async move {
        let refused = install_one(&dirs(), "web_serch").expect_err("no such tool");
        let message = refused.to_string();
        assert!(matches!(refused, ServeError::NotFound(_)), "{message}");
        assert!(message.contains("web_serch"), "{message}");
        assert!(message.contains("web_search"), "{message}");
        assert!(message.contains("web_fetch"), "{message}");
        assert!(!home.join(".leviath").join("tools").exists());
    })
    .await;
}

/// Something at the global path that is not a plain file is refused by the
/// write path's own containment check, not written through or replaced.
#[tokio::test]
async fn a_global_path_that_is_not_a_file_is_refused() {
    with_home(|home| async move {
        let path = global_copy(&home, "web_search");
        std::fs::create_dir_all(&path).unwrap();
        let refused = install_one(&dirs(), "web_search").expect_err("not a plain file");
        assert!(matches!(refused, ServeError::Forbidden(_)), "{refused}");
        assert!(path.is_dir(), "the directory is left alone");
    })
    .await;
}

/// The listing names every bundled tool, and follows the global copy from
/// missing, to current, to edited.
#[tokio::test]
async fn the_listing_follows_the_global_copy() {
    with_home(|home| async move {
        let row = |name: &str| {
            list(&dirs())
                .into_iter()
                .find(|row| row.name == name)
                .expect("listed")
        };
        let names: Vec<String> = list(&dirs()).into_iter().map(|row| row.name).collect();
        let shipped: Vec<String> = crate::bundled::bundled_tools()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(names, shipped);

        let missing = row("web_search");
        assert!(!missing.installed && !missing.current);
        assert!(
            missing.description.contains("Search the web"),
            "{}",
            missing.description
        );

        install_one(&dirs(), "web_search").expect("installs");
        let current = row("web_search");
        assert!(current.installed && current.current);

        std::fs::write(global_copy(&home, "web_search"), "// @tool web_search\n1").unwrap();
        let edited = row("web_search");
        assert!(edited.installed && !edited.current);
    })
    .await;
}

/// The read route answers with the same rows.
#[tokio::test]
async fn the_read_route_lists_the_bundled_tools() {
    with_home(|_home| async move {
        let Json(listed) = list_bundled_tools(State(state_with_agent_paths(Vec::new()))).await;
        let body = serde_json::to_value(&listed).expect("serializes");
        let search = body["tools"]
            .as_array()
            .expect("a list")
            .iter()
            .find(|row| row["name"] == "web_search")
            .expect("web_search is listed")
            .clone();
        assert_eq!(search["installed"], false);
        assert_eq!(search["current"], false);
    })
    .await;
}

/// The route answers with the install, and with the miss as a 404.
#[tokio::test]
async fn the_route_installs_and_refuses_through_the_api_error() {
    with_home(|_home| async move {
        let state = state_with_agent_paths(Vec::new());
        let Json(installed) =
            install_bundled_tool(State(state.clone()), AxumPath("web_fetch".to_string()))
                .await
                .expect("installs");
        let body = serde_json::to_value(&installed).expect("serializes");
        assert_eq!(body["name"], "web_fetch");
        assert_eq!(body["outcome"], "installed");
        assert_eq!(body["compiles"], true);
        assert!(
            body["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("web_fetch.rhai"))
        );

        let refused = install_bundled_tool(State(state), AxumPath("nope".to_string()))
            .await
            .expect_err("no such tool");
        assert_eq!(refused.0, axum::http::StatusCode::NOT_FOUND);
    })
    .await;
}
