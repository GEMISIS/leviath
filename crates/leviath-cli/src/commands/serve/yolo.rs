//! `/api/yolo`: the profiles behind `--yolo=<name>`, over HTTP.
//!
//! The same three questions `lev yolo` answers, for a console: what profiles
//! are there, what does one say, and what would it decide for a call. The
//! write half is mounted only under `--allow-admin`, because a profile is a
//! grant of permissions and writing one is the same category of act as writing
//! `config.toml`.
//!
//! Two shapes of write, for two callers. `PUT /api/yolo` replaces the file's
//! text, which is what an editor over a text area wants. [`upsert_profile`]
//! and [`remove_profile`] change one table and leave the rest of the document
//! alone, which is what a form over one profile wants: the comments around it
//! survive. Both check the whole document before anything reaches the disk.
//!
//! Every handler reads the file as it stands through
//! [`crate::yolo::yolo_path`], a plain function over the environment rather
//! than anything reachable from a request, for the reason `AdminPaths` gives:
//! a file location that is request data is a path-injection finding.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

use super::types::{ApiError, AppState};
use crate::commands::yolo::TestArgs;
use crate::yolo::{ProfileSummary, YoloError, YoloFile, yolo_path};

/// `GET /api/yolo`: where the file is, whether it loads, and each profile's
/// shape.
#[derive(Debug, Serialize)]
pub(super) struct YoloListing {
    /// The file the profiles are read from.
    pub path: String,
    /// Whether it exists. `false` with no error means `--yolo=<name>` has
    /// nothing to name yet.
    pub exists: bool,
    /// Why the file does not load, when it does not. The profiles are then
    /// empty: a spawn naming one would be refused with this same message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub profiles: Vec<ProfileSummary>,
}

/// The listing for the file as it stands.
pub(super) fn listing() -> YoloListing {
    let path = yolo_path();
    match YoloFile::load_from(&path) {
        Ok(file) => YoloListing {
            path: path.display().to_string(),
            exists: file.exists(),
            error: None,
            profiles: file.profiles().map(|p| p.summary()).collect(),
        },
        Err(e) => YoloListing {
            path: path.display().to_string(),
            exists: true,
            error: Some(e.to_string()),
            profiles: Vec::new(),
        },
    }
}

/// Every profile the file holds, or none at all when it does not load.
///
/// The "why it does not load" half is `config.yoloFile.error`, which is where
/// a file's own status lives. This answers the other question, which is what a
/// listing asks: what profiles are there to name.
pub(super) fn profiles() -> Vec<std::sync::Arc<crate::yolo::YoloProfile>> {
    YoloFile::load_from(&yolo_path())
        .map(|file| file.profiles().cloned().collect())
        .unwrap_or_default()
}

/// The status a profile lookup failure answers with: the name is the
/// caller's (404), the file is the operator's (422).
fn lookup_error(e: YoloError) -> super::core::error::ServeError {
    use super::core::error::ServeError;

    match e {
        YoloError::UnknownProfile { .. } | YoloError::NoFile { .. } => {
            ServeError::NotFound(e.to_string())
        }
        // The file is there and will not load. Sending the request differently
        // does not help, and nothing is missing, so it is neither of those.
        _ => ServeError::Unprocessable(e.to_string()),
    }
}

/// Load the file and resolve `name` in it.
fn profile_named(
    name: &str,
) -> Result<std::sync::Arc<crate::yolo::YoloProfile>, super::core::error::ServeError> {
    let path = yolo_path();
    let file = YoloFile::load_from(&path).map_err(lookup_error)?;
    file.resolve(Some(name), &path).map_err(lookup_error)
}

/// `GET /api/yolo`.
pub(super) async fn list_profiles(State(_state): State<AppState>) -> Json<YoloListing> {
    Json(listing())
}

/// `GET /api/yolo/{name}`: one profile in full, as its parsed spec, with what
/// it keeps for a person.
pub(super) async fn get_profile(
    State(_state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let profile = profile_named(&name).map_err(|e| super::core::error::as_api_error(&e))?;
    Ok(Json(serde_json::json!({
        "name": profile.name,
        "spec": profile.spec,
        "holds": profile.holds(),
    })))
}

/// `POST /api/yolo/test`: what `profile` would decide for one call.
#[derive(Debug, Deserialize)]
pub(super) struct TestReq {
    /// The profile to test.
    pub profile: String,
    /// The tool the model would call.
    pub tool: String,
    /// For the shell: the command line.
    #[serde(default)]
    pub command: Option<String>,
    /// The call's arguments, for a tool that is not the shell.
    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
    /// The workdir the run would have. Defaults to the server's.
    #[serde(default)]
    pub workdir: Option<String>,
    /// What the config layers resolve the tool to (`allow`, `ask`, `deny`).
    /// Left off, it is read from the config in force.
    #[serde(default)]
    pub configured: Option<String>,
    /// `builtin`, `subagent`, `script` or `mcp`; guessed from the name when
    /// left off.
    #[serde(default)]
    pub kind: Option<String>,
    /// Decide as if `--allow <tool>` had been passed.
    #[serde(default)]
    pub allowed: bool,
}

/// `POST /api/yolo/test`. The same code path as `lev yolo test`, so the two
/// cannot disagree about a call.
pub(super) async fn test_profile(
    State(state): State<AppState>,
    Json(req): Json<TestReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    decided(&state, req)
        .map(Json)
        .map_err(|e| super::core::error::as_api_error(&e))
}

/// What one profile would do with one call, for whichever surface asked.
///
/// The same code path `lev yolo test` runs, so the command and the API cannot
/// disagree about a call.
pub(super) fn decided(
    state: &AppState,
    req: TestReq,
) -> Result<serde_json::Value, super::core::error::ServeError> {
    use super::core::error::ServeError;

    let profile = profile_named(&req.profile)?;
    let args = TestArgs {
        name: req.profile,
        tool: req.tool,
        command: req.command,
        args: req.arguments.map(|v| v.to_string()),
        workdir: req.workdir.map(std::path::PathBuf::from),
        configured: req.configured,
        kind: req.kind,
        allowed: req.allowed,
        json: true,
    };
    let config = state.config.current();
    let configured = crate::commands::yolo::configured_policy(&args, Some(&config))
        .map_err(|e| ServeError::BadRequest(e.to_string()))?;
    crate::commands::yolo::decision_json(&profile, &args, configured)
        .map_err(|e| ServeError::BadRequest(e.to_string()))
}

/// `PUT /api/yolo` (admin-only): replace the file's text.
#[derive(Debug, Deserialize)]
pub(super) struct WriteYoloReq {
    /// The whole file, as TOML.
    pub text: String,
}

/// `PUT /api/yolo`. Parse-checked first, so a save that would not load is
/// refused with the same message a spawn would give, and the file on disk is
/// left as it was.
pub(super) async fn put_profiles(
    State(_state): State<AppState>,
    Json(req): Json<WriteYoloReq>,
) -> Result<Json<YoloListing>, ApiError> {
    write_profiles(&req.text)
        .map(|()| Json(listing()))
        .map_err(|e| super::core::error::as_api_error(&e))
}

/// Replace the profiles file, for whichever surface asked.
///
/// The file is the unit: `--yolo=<name>` names a profile inside it, and the
/// profiles refer to each other, so writing one at a time would let a save leave
/// the set inconsistent. Parsed before it is written, so a file that would not
/// load is refused rather than saved and discovered at the next spawn.
pub(super) fn write_profiles(text: &str) -> Result<(), super::core::error::ServeError> {
    use super::core::error::ServeError;

    YoloFile::from_toml(text).map_err(|e| ServeError::BadRequest(e.to_string()))?;
    let path = yolo_path();
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)
        .and_then(|()| std::fs::write(&path, text))
        .map_err(|e| ServeError::Internal(format!("failed to write {}: {e}", path.display())))
}

/// The file as a document that can be edited a table at a time, or an empty
/// one when there is no file yet.
fn read_document() -> Result<DocumentMut, super::core::error::ServeError> {
    use super::core::error::ServeError;

    let path = yolo_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(ServeError::Internal(format!(
                "failed to read {}: {e}",
                path.display()
            )));
        }
    };
    text.parse::<DocumentMut>().map_err(|e| {
        ServeError::Unprocessable(format!(
            "{} does not parse: {}",
            path.display(),
            e.message()
        ))
    })
}

/// Check that the file, as it stands before anything is edited, loads.
///
/// Asked first and answered as [`ServeError::Unprocessable`], because a file
/// that was already broken is not this request's doing: nothing the caller
/// sends differently fixes it, and an operator told "bad request" would go
/// looking at the query rather than at the file. It is the same thing a TOML
/// file that will not even parse answers, and for the same reason.
fn loads(doc: &DocumentMut) -> Result<YoloFile, super::core::error::ServeError> {
    use super::core::error::ServeError;

    YoloFile::from_toml(&doc.to_string()).map_err(|e| {
        ServeError::Unprocessable(format!("{} does not load: {e}", yolo_path().display()))
    })
}

/// Read the file, refuse a file that will not load, apply one edit, write it.
///
/// Both one-table writes are this shape, and they share it rather than each
/// spelling it out, because the two checks around the edit are what say whose
/// problem a failure is. A file that will not load before the edit is the
/// operator's, and `UNPROCESSABLE` says so. A file that will not load after it
/// is the caller's, and that is a bad request. An edit with nothing to do
/// answers `false` and the file on disk is never opened for writing.
///
/// The edit arrives as a trait object rather than by type, so the two callers
/// share one copy of this and the "nothing to do" arm is the same code for
/// both. Written generically, the copy the upsert gets has an arm no request
/// can reach, because an upsert always has a table to write.
fn edit_document(
    edit: &mut dyn FnMut(&mut DocumentMut) -> bool,
) -> Result<Option<YoloFile>, super::core::error::ServeError> {
    let mut doc = read_document()?;
    loads(&doc)?;
    match edit(&mut doc) {
        false => Ok(None),
        true => Ok(Some(save_document(&doc)?)),
    }
}

/// Check that the document loads as a whole, then write it.
///
/// The whole document rather than the one table that changed: the profiles are
/// read as a set, and a save that left the file unloadable would be discovered
/// at the next spawn rather than here. The file on disk is untouched until the
/// check passes.
///
/// A refusal here is a bad request, because by this point the file was known to
/// load and the edit is the only thing that changed: a reserved profile name, a
/// shell rule that will not compile.
fn save_document(doc: &DocumentMut) -> Result<YoloFile, super::core::error::ServeError> {
    use super::core::error::ServeError;

    let text = doc.to_string();
    let file = YoloFile::from_toml(&text).map_err(|e| ServeError::BadRequest(e.to_string()))?;
    let path = yolo_path();
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)
        .and_then(|()| leviath_sys::write_atomic(&path, text.as_bytes(), None))
        .map_err(|e| ServeError::Internal(format!("failed to write {}: {e}", path.display())))?;
    Ok(file)
}

/// One profile's table, as the file spells it.
///
/// Written whole rather than key by key, because a profile is a grant of
/// permissions: an edit that left half of a previous list behind would
/// describe a set of rules nobody wrote.
fn profile_table(spec: &crate::yolo::rules::ProfileSpec) -> Table {
    /// One list of tool entries.
    fn entries(list: &[String]) -> Item {
        let mut array = Array::new();
        for entry in list {
            array.push(entry.as_str());
        }
        Item::Value(Value::Array(array))
    }

    /// One verdict's shell rules, as a table each.
    fn shell_rules(rules: &[crate::yolo::rules::ShellRuleSpec]) -> Item {
        let mut tables = ArrayOfTables::new();
        for rule in rules {
            let mut table = Table::new();
            table.insert("command", Item::Value(Value::from(rule.command.as_str())));
            if let Some(args) = &rule.args {
                table.insert("args", entries(args));
            }
            tables.push(table);
        }
        Item::ArrayOfTables(tables)
    }

    let mut tools = Table::new();
    tools.set_implicit(true);
    tools.insert("allow", entries(&spec.tools.allow));
    tools.insert("ask", entries(&spec.tools.ask));
    tools.insert("deny", entries(&spec.tools.deny));

    // Implicit, and holding nothing but arrays of tables: a profile with no
    // shell rules writes no `[<name>.shell]` header at all.
    let mut shell = Table::new();
    shell.set_implicit(true);
    shell.insert("allow", shell_rules(&spec.shell.allow));
    shell.insert("ask", shell_rules(&spec.shell.ask));
    shell.insert("deny", shell_rules(&spec.shell.deny));

    let mut table = Table::new();
    table.set_implicit(false);
    table.insert("default", word(waiver_word(spec.default)));
    table.insert("questions", word(human_word(spec.questions)));
    table.insert("checkpoints", word(human_word(spec.checkpoints)));
    table.insert("gate", word(human_word(spec.gate)));
    table.insert("tools", Item::Table(tools));
    table.insert("shell", Item::Table(shell));
    table
}

/// One setting's word, as a TOML string.
fn word(text: &str) -> Item {
    Item::Value(Value::from(text))
}

/// What a profile's default does, in the word the file uses.
fn waiver_word(waiver: crate::yolo::rules::Waiver) -> &'static str {
    match waiver {
        crate::yolo::rules::Waiver::Allow => "allow",
        crate::yolo::rules::Waiver::Ask => "ask",
    }
}

/// Whether a human-in-the-loop mechanism reaches a person, in the same words.
fn human_word(human: crate::yolo::rules::Human) -> &'static str {
    match human {
        crate::yolo::rules::Human::Ask => "ask",
        crate::yolo::rules::Human::Auto => "auto",
    }
}

/// One profile as the file holds it after a write, and whether it is new.
#[derive(Debug)]
pub(super) struct Written {
    /// Whether the profile was added rather than replaced.
    pub(super) is_new: bool,
    /// The profile, read back through the file the save produced.
    pub(super) profile: std::sync::Arc<crate::yolo::YoloProfile>,
}

/// What sits above one table's `[header]` line: the blank lines and comments
/// written before it, which is where the file's own header lives when the
/// table is the first one.
fn prefix_of(item: Option<&Item>) -> String {
    item.and_then(Item::as_table)
        .and_then(|table| table.decor().prefix())
        .and_then(toml_edit::RawString::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The table that comes first in the file as it now stands.
///
/// By written position rather than by the order the tables are held in, which
/// is the order they were added in and not the order they are printed in.
fn first_table(doc: &mut DocumentMut) -> Option<&mut Table> {
    doc.as_table_mut()
        .iter_mut()
        .filter_map(|(_, item)| item.as_table_mut())
        .min_by_key(|table| table.position().unwrap_or(isize::MAX))
}

/// Put trivia back above whichever table now stands where it was written.
///
/// `toml_edit` files the blank lines and comments before a `[header]` under
/// that header's own table, so a file's leading comment belongs to whichever
/// table happens to come first. Replacing or removing that table would take the
/// comment with it, and the file would lose the line explaining what it is.
fn keep_prefix(doc: &mut DocumentMut, prefix: &str) {
    if prefix.is_empty() {
        return;
    }
    let Some(table) = first_table(doc) else {
        return;
    };
    let decor = table.decor_mut();
    let below = decor
        .prefix()
        .and_then(toml_edit::RawString::as_str)
        .unwrap_or_default()
        .to_string();
    decor.set_prefix(format!("{prefix}{below}"));
}

/// Write one profile's table, leaving every other table in the file as it is.
///
/// Comments and formatting elsewhere in the file survive, because only the one
/// table is replaced, and what sat above the replaced table's own header stays
/// above it: that is where the file's own leading comment is kept when this is
/// the first profile in it. The comments inside the table being written do not
/// survive: it is rewritten from what the request said, and a comment about the
/// old rules would then describe rules that are gone.
pub(super) fn upsert_profile(
    name: &str,
    spec: &crate::yolo::rules::ProfileSpec,
) -> Result<Written, super::core::error::ServeError> {
    let mut is_new = false;
    let written = edit_document(&mut |doc| {
        let existing = doc.get(name);
        is_new = existing.is_none();
        let kept = prefix_of(existing);
        let mut table = profile_table(spec);
        table.decor_mut().set_prefix(kept);
        doc.insert(name, Item::Table(table));
        true
    })?;
    let file = written.expect("a profile write always has a table to write");
    Ok(Written {
        is_new,
        profile: file
            .get(name)
            .expect("the document that loaded holds the table it was given"),
    })
}

/// Take one profile out of the file, and say whether there was one.
///
/// What sat above the removed table moves down to the table that is first now,
/// so deleting the first profile does not delete the file's own header with it.
/// A file that will not load is refused before the miss is answered: it cannot
/// be read as "there is no profile by that name", and a miss would blame the
/// name when what is wrong is the file.
pub(super) fn remove_profile(name: &str) -> Result<bool, super::core::error::ServeError> {
    let removed = edit_document(&mut |doc| match doc.remove(name) {
        None => false,
        Some(removed) => {
            keep_prefix(doc, &prefix_of(Some(&removed)));
            true
        }
    })?;
    Ok(removed.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::yolo::EXAMPLE_TOML;
    use axum::http::StatusCode;

    fn state() -> AppState {
        super::super::testutil::state_with_agent_paths(Vec::new())
    }

    fn test_req(profile: &str, tool: &str, command: Option<&str>) -> TestReq {
        TestReq {
            profile: profile.to_string(),
            tool: tool.to_string(),
            command: command.map(str::to_string),
            arguments: None,
            workdir: Some(std::env::temp_dir().display().to_string()),
            configured: None,
            kind: None,
            allowed: false,
        }
    }

    #[tokio::test]
    async fn the_listing_follows_the_file_through_its_states() {
        crate::config::with_isolated_config_path_async("api-yolo-list", |cfg| async move {
            let none = list_profiles(State(state())).await.0;
            assert!(!none.exists);
            assert!(none.error.is_none());
            assert!(none.profiles.is_empty());
            assert_eq!(none.path, cfg.join("yolo.toml").display().to_string());

            let written = put_profiles(
                State(state()),
                Json(WriteYoloReq {
                    text: EXAMPLE_TOML.to_string(),
                }),
            )
            .await
            .expect("a good file is written")
            .0;
            assert!(written.exists);
            assert_eq!(written.profiles.len(), 2);
            assert_eq!(written.profiles[1].name, "careful");
            assert_eq!(
                std::fs::read_to_string(cfg.join("yolo.toml")).unwrap(),
                EXAMPLE_TOML
            );

            // A broken save is refused and leaves the file alone.
            let refused = put_profiles(
                State(state()),
                Json(WriteYoloReq {
                    text: "[p]\nblock = 1\n".to_string(),
                }),
            )
            .await
            .expect_err("a file that does not load is refused");
            assert_eq!(refused.0, StatusCode::BAD_REQUEST);
            assert!(
                refused.1.0.error.contains("does not load"),
                "{}",
                refused.1.0.error
            );
            assert_eq!(
                std::fs::read_to_string(cfg.join("yolo.toml")).unwrap(),
                EXAMPLE_TOML
            );

            // A write that fails is a 500 naming the path.
            std::fs::remove_file(cfg.join("yolo.toml")).unwrap();
            std::fs::create_dir(cfg.join("yolo.toml")).unwrap();
            let failed = put_profiles(
                State(state()),
                Json(WriteYoloReq {
                    text: EXAMPLE_TOML.to_string(),
                }),
            )
            .await
            .expect_err("a directory in the way");
            assert_eq!(failed.0, StatusCode::INTERNAL_SERVER_ERROR);
            assert!(
                failed.1.0.error.contains("failed to write"),
                "{}",
                failed.1.0.error
            );
            std::fs::remove_dir(cfg.join("yolo.toml")).unwrap();

            // A file broken by hand is reported, with no profiles.
            std::fs::write(cfg.join("yolo.toml"), "[").unwrap();
            let broken = list_profiles(State(state())).await.0;
            assert!(broken.exists);
            assert!(broken.error.is_some());
            assert!(broken.profiles.is_empty());
            let json = serde_json::to_value(&broken).unwrap();
            assert!(json["error"].is_string());
        })
        .await;
    }

    #[tokio::test]
    async fn a_profile_is_read_by_name_or_answers_404() {
        crate::config::with_isolated_config_path_async("api-yolo-get", |cfg| async move {
            let missing = get_profile(State(state()), Path("careful".to_string()))
                .await
                .expect_err("no file");
            assert_eq!(missing.0, StatusCode::NOT_FOUND);
            std::fs::write(cfg.join("yolo.toml"), EXAMPLE_TOML).unwrap();
            let got = get_profile(State(state()), Path("careful".to_string()))
                .await
                .expect("found")
                .0;
            assert_eq!(got["name"], "careful");
            assert_eq!(got["spec"]["default"], "ask");
            assert!(got["holds"].as_array().unwrap().len() >= 2);
            let unknown = get_profile(State(state()), Path("nope".to_string()))
                .await
                .expect_err("unknown");
            assert_eq!(unknown.0, StatusCode::NOT_FOUND);
            assert!(unknown.1.0.error.contains("build-only, careful"));
            std::fs::write(cfg.join("yolo.toml"), "[").unwrap();
            let broken = get_profile(State(state()), Path("careful".to_string()))
                .await
                .expect_err("broken file");
            assert_eq!(broken.0, StatusCode::UNPROCESSABLE_ENTITY);
        })
        .await;
    }

    /// A profile the file does not load has none to list, and a document that
    /// cannot be read at all is not editable a table at a time.
    #[tokio::test]
    async fn an_unreadable_file_lists_nothing_and_refuses_a_table_write() {
        crate::config::with_isolated_config_path_async("api-yolo-doc", |cfg| async move {
            let path = cfg.join("yolo.toml");
            std::fs::write(&path, "[").expect("a broken file");
            assert!(
                profiles().is_empty(),
                "a file that does not load holds none"
            );

            // A directory where the file goes: read_to_string fails with
            // something that is not "not found", which is the operator's
            // problem rather than the caller's.
            std::fs::remove_file(&path).expect("the broken file");
            std::fs::create_dir(&path).expect("a directory in the way");
            let spec = crate::yolo::rules::ProfileSpec::builtin_default();
            let failed = upsert_profile("builder", &spec).expect_err("a directory in the way");
            assert!(failed.to_string().contains("failed to read"), "{failed:?}");
            let failed = remove_profile("builder").expect_err("a directory in the way");
            assert!(failed.to_string().contains("failed to read"), "{failed:?}");
            std::fs::remove_dir(&path).expect("the directory");

            // A write that cannot land is a 500 naming the path.
            std::fs::create_dir(&path).expect("a directory in the way");
            let doc = "[builder]\ndefault = \"allow\"\n"
                .parse::<DocumentMut>()
                .expect("a document");
            let failed = save_document(&doc).expect_err("a directory in the way");
            assert!(failed.to_string().contains("failed to write"), "{failed:?}");
            std::fs::remove_dir(&path).expect("the directory");
        })
        .await;
    }

    /// A profile written into a file that is not there yet makes the file, and
    /// comes back compiled.
    #[tokio::test]
    async fn a_table_write_makes_the_file_when_there_is_none() {
        crate::config::with_isolated_config_path_async("api-yolo-new", |cfg| async move {
            let mut spec = crate::yolo::rules::ProfileSpec::builtin_default();
            spec.tools.ask = vec!["web_fetch".to_string()];
            spec.shell.deny = vec![crate::yolo::rules::ShellRuleSpec {
                command: "curl".to_string(),
                args: Some(vec!["--*".to_string()]),
            }];
            let written = upsert_profile("builder", &spec).expect("a new file");
            assert!(written.is_new);
            assert_eq!(written.profile.name, "builder");
            assert_eq!(
                written.profile.spec.tools.ask,
                vec!["web_fetch".to_string()]
            );
            assert_eq!(written.profile.spec.shell.deny[0].command, "curl");
            let text = std::fs::read_to_string(cfg.join("yolo.toml")).expect("the file");
            assert!(text.contains("[builder]"), "{text}");
            assert!(text.contains("[[builder.shell.deny]]"), "{text}");
            assert!(
                !text.contains("[builder.shell]\n\n"),
                "an empty shell table writes no header: {text}"
            );

            assert!(remove_profile("builder").expect("the file loads"));
            assert!(!remove_profile("builder").expect("the file loads"));
        })
        .await;
    }

    #[tokio::test]
    async fn a_call_is_decided_the_way_the_cli_decides_it() {
        crate::config::with_isolated_config_path_async("api-yolo-test", |cfg| async move {
            std::fs::write(cfg.join("yolo.toml"), EXAMPLE_TOML).unwrap();
            let denied = test_profile(
                State(state()),
                Json(test_req("careful", "shell", Some("curl https://x"))),
            )
            .await
            .expect("decided")
            .0;
            assert_eq!(denied["policy"], "deny");
            assert_eq!(denied["reason"], "shell deny rule \"curl\"");
            assert_eq!(denied["configured"], "ask");

            let mut req = test_req("careful", "web_fetch", None);
            req.arguments = Some(serde_json::json!({"url": "https://x"}));
            req.configured = Some("allow".to_string());
            let asked = test_profile(State(state()), Json(req))
                .await
                .expect("decided")
                .0;
            assert_eq!(asked["policy"], "ask");

            let mut req = test_req("careful", "acme__thing", None);
            req.kind = Some("robot".to_string());
            let bad = test_profile(State(state()), Json(req))
                .await
                .expect_err("a bad kind");
            assert_eq!(bad.0, StatusCode::BAD_REQUEST);
            let mut req = test_req("careful", "shell", Some("ls"));
            req.configured = Some("maybe".to_string());
            let bad = test_profile(State(state()), Json(req))
                .await
                .expect_err("a bad policy word");
            assert_eq!(bad.0, StatusCode::BAD_REQUEST);
            let unknown = test_profile(State(state()), Json(test_req("nope", "shell", Some("ls"))))
                .await
                .expect_err("unknown profile");
            assert_eq!(unknown.0, StatusCode::NOT_FOUND);
        })
        .await;
    }
}
