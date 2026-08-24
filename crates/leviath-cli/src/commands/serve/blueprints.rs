//! Blueprint discovery, CRUD, and validation endpoints.

use std::path::{Path, PathBuf};

use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::types::*;
use leviath_core::manifest::parse_manifest;

/// Resolve the installed agents directory.
///
/// Goes through the shared home resolver so `LEVIATH_HOME` applies here too.
/// Calling `dirs::home_dir()` directly would have these handlers read and
/// write a *different* directory from the one `lev add` installs into
/// whenever that override is set.
pub(super) fn agents_dir() -> PathBuf {
    leviath_core::paths::agents_dir().unwrap_or_default()
}

/// Resolve `<agents_dir>/<name>`, refusing a name that is not a single safe path
/// component.
///
/// `Path::join` neither normalizes `..` nor resists an absolute path, so an
/// unvalidated `name` from a REST body or URL segment reached anywhere on the
/// filesystem: `POST /api/blueprints` with `name = "../../../../tmp/x"` created
/// a directory and wrote attacker-controlled TOML into it, and
/// `DELETE /api/blueprints/{name}` recursively deleted whatever it landed on.
fn blueprint_dir(name: &str) -> Result<PathBuf, (StatusCode, Json<ErrorResponse>)> {
    if !leviath_core::is_safe_path_component(name) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!(
                    "Invalid blueprint name '{name}': names may contain only letters, \
                     digits, '.', '_' and '-'"
                ),
            }),
        ));
    }
    Ok(agents_dir().join(name))
}

/// Collapse a raw discovery scan into the canonical catalog: one blueprint per
/// name, in a stable order.
///
/// Split out of [`discover_blueprints`] because it is the part with the rules,
/// and it can be tested over a hand-built `Vec` instead of a directory tree.
///
/// **Dedup by name, first wins.** `get_blueprint` and `spawn_agent` both resolve
/// a blueprint with `.find(|b| b.name == name)` over this list, so a name
/// reachable from two roots (the installed agents dir and a `config.agent_paths`
/// entry, say) made *which agent actually ran* depend on `read_dir` order, which
/// is a filesystem detail that can differ between two calls on one machine. The
/// dedup happens in scan order, before the sort, so the winner is the one the
/// existing `.find()` already meant to pick - the installed catalog first - and
/// this only makes that choice deterministic rather than changing it.
///
/// **Then sort by name**, which needs no tie-break precisely because the dedup
/// above ran first: after it, no two entries share a name, so name alone is a
/// total order. A list that is merely "whatever the filesystem said" cannot be
/// paginated - a cursor over an unstable order skips and repeats entries.
fn canonicalize(found: Vec<BlueprintInfo>) -> Vec<BlueprintInfo> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut kept: Vec<BlueprintInfo> = Vec::with_capacity(found.len());
    for info in found {
        if seen.insert(info.name.clone()) {
            kept.push(info);
        } else {
            tracing::debug!(
                name = %info.name,
                shadowed = %info.path,
                "duplicate blueprint name; keeping the earlier one"
            );
        }
    }
    kept.sort_by(|a, b| a.name.cmp(&b.name));
    kept
}

/// Scan for blueprints from installed agents dir and configured agent_paths.
///
/// The result is deduplicated by name and name-sorted - see [`canonicalize`].
/// Every consumer goes through here (`list_blueprints`, `get_blueprint`,
/// `spawn_agent`), so they all share one answer to "which blueprint is `x`".
pub(super) fn discover_blueprints(config: &crate::config::Config) -> Vec<BlueprintInfo> {
    let mut results = Vec::new();
    let agents = agents_dir();

    let mut dirs_to_scan: Vec<PathBuf> = vec![agents];
    dirs_to_scan.extend(config.agent_paths.iter().cloned());

    for dir in dirs_to_scan {
        if !dir.exists() {
            continue;
        }
        // Check dir itself
        let manifest = dir.join("agent.leviath");
        if manifest.exists() {
            results.extend(read_blueprint_info(&manifest, &dir));
        }
        // Check subdirs. Sorted, because `canonicalize` resolves a duplicate
        // name by taking the first one scanned - and two subdirectories of the
        // *same* root can declare the same name, so without this the winner
        // would still come down to `read_dir` order.
        let mut subdirs: Vec<PathBuf> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|p| p.is_dir())
            .collect();
        subdirs.sort();
        for p in subdirs {
            let m = p.join("agent.leviath");
            if m.exists() {
                results.extend(read_blueprint_info(&m, &p));
            }
        }
    }

    canonicalize(results)
}

pub(super) fn read_blueprint_info(manifest_path: &Path, dir: &Path) -> Option<BlueprintInfo> {
    let content = std::fs::read_to_string(manifest_path).ok()?;
    let bp = parse_manifest(&content).ok()?;
    Some(BlueprintInfo {
        name: bp.name,
        version: bp.version,
        description: bp.description,
        path: dir.to_string_lossy().to_string(),
        stages: bp.stages.iter().map(|s| s.name.clone()).collect(),
        manifest: content,
    })
}

/// Default page size. Comfortably more blueprints than anyone installs, so the
/// common case is one request.
const DEFAULT_LIMIT: usize = 50;
/// Largest page served.
const MAX_LIMIT: usize = 200;

/// `GET /api/blueprints`: the installed agent catalog, paginated and filterable.
///
/// **Breaking change**, taken deliberately in the same release as the rest of
/// this work: the response is now the envelope every paginated route here
/// returns, rather than a bare array. Announced through the `capabilities` list
/// on `GET /api/config` so a client can tell before it asks.
///
/// Worth being plain about the tradeoff: **pagination buys nothing here.**
/// `discover_blueprints` scans every configured directory and TOML-parses every
/// manifest on every request regardless of page size, so a page of ten costs
/// what the whole list costs. The saving is wire bytes on a handful of small
/// objects. The envelope is worth taking for one consistent shape across the
/// API, and `q` is the part with real value - but the catalog is bounded by
/// what a person installs, and this is not what makes it scale.
pub(super) async fn list_blueprints(
    State(state): State<AppState>,
    Query(query): Query<BlueprintsQuery>,
) -> Result<Json<Page<BlueprintInfo>>, (StatusCode, Json<ErrorResponse>)> {
    let descending = match query.order.as_deref() {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(other) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("Unknown order '{other}': expected asc or desc"),
            ));
        }
    };
    let sort_name = match query.sort.as_deref() {
        None | Some("name") => "name",
        Some("version") => "version",
        Some(other) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("Unknown sort '{other}': expected name or version"),
            ));
        }
    };
    let limit = match query.limit {
        None => DEFAULT_LIMIT,
        Some(0) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "`limit` must be at least 1; omit it for the default".to_string(),
            ));
        }
        Some(n) => n.min(MAX_LIMIT),
    };

    let digest = super::cursor::filter_digest(&[query.q.as_deref().unwrap_or("")]);
    let order_name = if descending { "desc" } else { "asc" };
    let cursor = match query.cursor.as_deref() {
        None => None,
        Some(raw) => Some(
            super::cursor::decode(raw, sort_name, order_name, &digest)
                .map_err(|e| err(StatusCode::BAD_REQUEST, e.message()))?,
        ),
    };

    let mut found = discover_blueprints(&state.current_config());

    // `q` shares the search primitive but not the framework: three in-memory
    // string fields do not need sources, phases or highlights.
    if let Some(needle) = query.q.as_deref().filter(|s| !s.is_empty()) {
        found.retain(|bp| {
            super::search::find_ignore_ascii_case(&bp.name, needle).is_some()
                || super::search::find_ignore_ascii_case(&bp.description, needle).is_some()
                || bp
                    .stages
                    .iter()
                    .any(|stage| super::search::find_ignore_ascii_case(stage, needle).is_some())
        });
    }

    // `discover_blueprints` already sorts by (name, path); re-sort only when
    // something other than that default was asked for.
    // Sorting by version needs the name to break ties; sorting by name needs
    // nothing, because `canonicalize` already made names unique.
    let key = |bp: &BlueprintInfo| match sort_name {
        "version" => (bp.version.clone(), bp.name.clone()),
        _ => (bp.name.clone(), String::new()),
    };
    found.sort_by(|a, b| {
        if descending {
            key(b).cmp(&key(a))
        } else {
            key(a).cmp(&key(b))
        }
    });

    let total = found.len();
    let mut remaining: Vec<BlueprintInfo> = match cursor {
        None => found,
        Some(ref cursor) => found
            .into_iter()
            .filter(|bp| {
                cursor.precedes(
                    &super::cursor::CursorKey::Text(key(bp).0),
                    &key(bp).1,
                    descending,
                )
            })
            .collect(),
    };

    let has_more = remaining.len() > limit;
    remaining.truncate(limit);
    let next_cursor = has_more.then(|| remaining.last()).flatten().map(|last| {
        let (primary, tiebreak) = key(last);
        super::cursor::encode(
            sort_name,
            order_name,
            &digest,
            super::cursor::CursorKey::Text(primary),
            &tiebreak,
        )
    });

    Ok(Json(Page::new(
        remaining,
        next_cursor,
        Some(total),
        now_secs(),
    )))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `GET /api/blueprints/{name}`: one blueprint, with its manifest text.
///
/// The manifest is read here rather than listed, because a listing that
/// carried every manifest would send the contents of every agent on the
/// machine to answer a question about their names.
///
/// A blueprint the catalog found but whose manifest cannot be read is a 500
/// rather than a detail response without one: the caller asked for the file,
/// and answering with everything except the file is how a console ends up
/// showing a manifest it invented.
pub(super) async fn get_blueprint(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<BlueprintDetail>, StatusCode> {
    let blueprints = discover_blueprints(&state.current_config());
    let mut info = blueprints
        .into_iter()
        .find(|b| b.name == name)
        .ok_or(StatusCode::NOT_FOUND)?;
    // Taken out of the info rather than read again: discovery already read
    // this file to build everything else in the response.
    let manifest = std::mem::take(&mut info.manifest);
    // Parsed from the text already in hand rather than re-read: the same
    // reason the manifest itself is carried through from discovery.
    let parsed = parse_manifest(&manifest).ok();
    let regions = parsed
        .as_ref()
        .map(|bp| {
            bp.context_layout
                .regions
                .iter()
                .map(|r| RegionInfo {
                    name: r.name.clone(),
                    kind: leviath_runtime::persistence::region_kind_str(&r.kind).to_string(),
                    description: r.description.clone(),
                    describe_in_prompt: r.describe_in_prompt,
                    max_tokens: r.max_tokens,
                })
                .collect()
        })
        .unwrap_or_default();
    let fan_outs = parsed.as_ref().map(fan_out_infos).unwrap_or_default();
    Ok(Json(BlueprintDetail {
        info,
        regions,
        fan_outs,
        manifest,
    }))
}

/// The fan-out stages of a blueprint, with their limits resolved.
///
/// The manifest text is on the same response, so a client *could* work these
/// out itself, but only by re-implementing the parser's defaults and its
/// reading of `0`. A console that got the default wrong would show a stage
/// as capped at four workers when the daemon runs thirty; one that missed the
/// zero rule would show "0 workers" for a stage that is unlimited. Resolving
/// the numbers here means what the API says is what the run does.
fn fan_out_infos(bp: &leviath_core::blueprint::Blueprint) -> Vec<FanOutInfo> {
    bp.stages
        .iter()
        .filter_map(|stage| match &stage.mode {
            leviath_core::blueprint::StageMode::FanOut { config } => Some(FanOutInfo {
                stage: stage.name.clone(),
                worker_agent: config.worker_agent.clone(),
                worker_stage: config.worker_stage.clone(),
                worker_query: config.worker_query.clone(),
                merge_stage: config.merge_stage.clone(),
                max_workers: config.worker_cap(),
                max_items: config.max_items,
                on_worker_failure: match config.on_worker_failure {
                    leviath_core::blueprint::WorkerFailurePolicy::Continue => "continue",
                    leviath_core::blueprint::WorkerFailurePolicy::FailAll => "fail_all",
                }
                .to_string(),
                results_region: config.results_region.clone(),
            }),
            _ => None,
        })
        .collect()
}

pub(super) async fn create_blueprint(
    Json(body): Json<CreateBlueprintReq>,
) -> Result<Json<BlueprintInfo>, (StatusCode, Json<ErrorResponse>)> {
    // Validate manifest first, keeping the parsed Blueprint so the response
    // can be built from it directly below instead of re-reading the file we
    // just wrote (re-reading would make the re-read's error arm a TOCTOU-only,
    // untestable dead branch).
    let bp = parse_manifest(&body.manifest).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid manifest: {}", e),
            }),
        )
    })?;

    let dir = blueprint_dir(&body.name)?;
    std::fs::create_dir_all(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to create directory: {}", e),
            }),
        )
    })?;

    let manifest_path = dir.join("agent.leviath");
    std::fs::write(&manifest_path, &body.manifest).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to write manifest: {}", e),
            }),
        )
    })?;

    Ok(Json(BlueprintInfo {
        name: bp.name,
        version: bp.version,
        description: bp.description,
        path: dir.to_string_lossy().to_string(),
        stages: bp.stages.iter().map(|s| s.name.clone()).collect(),
        // The text just written. Not serialized on this route, which returns
        // the catalog shape, but carried so the value is never a lie.
        manifest: body.manifest,
    }))
}

pub(super) async fn update_blueprint(
    AxumPath(name): AxumPath<String>,
    Json(body): Json<UpdateBlueprintReq>,
) -> Result<Json<BlueprintInfo>, (StatusCode, Json<ErrorResponse>)> {
    let bp = parse_manifest(&body.manifest).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid manifest: {}", e),
            }),
        )
    })?;

    let dir = blueprint_dir(&name)?;
    let manifest_path = dir.join("agent.leviath");
    if !manifest_path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Blueprint '{}' not found", name),
            }),
        ));
    }

    std::fs::write(&manifest_path, &body.manifest).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to write manifest: {}", e),
            }),
        )
    })?;

    Ok(Json(BlueprintInfo {
        name: bp.name,
        version: bp.version,
        description: bp.description,
        path: dir.to_string_lossy().to_string(),
        stages: bp.stages.iter().map(|s| s.name.clone()).collect(),
        // The text just written. Not serialized on this route, which returns
        // the catalog shape, but carried so the value is never a lie.
        manifest: body.manifest,
    }))
}

pub(super) async fn delete_blueprint(
    AxumPath(name): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let dir = blueprint_dir(&name)?;
    if !dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Blueprint '{}' not found", name),
            }),
        ));
    }

    std::fs::remove_dir_all(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to delete blueprint: {}", e),
            }),
        )
    })?;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/blueprints/validate`: parse, validate and lint a manifest.
///
/// An optional `name` says which existing blueprint this manifest is an edit
/// of, which is what lets the lint find that agent's own `tools/*.rhai`. An
/// unusable name (one that is not a safe path component) is treated as no name
/// rather than a 400: the caller asked for a verdict on a manifest, and the
/// manifest can still be judged against the built-in tools.
pub(super) async fn validate_blueprint(
    Json(body): Json<ValidateBlueprintReq>,
) -> Json<ValidateResponse> {
    let dir = body
        .name
        .as_deref()
        .and_then(|name| blueprint_dir(name).ok())
        .unwrap_or_else(|| PathBuf::from("."));
    Json(validate_manifest_text(&body.manifest, &dir))
}

/// Parse, validate and lint a manifest posted as text.
///
/// A lint error is a real defect (a tool name that resolves to nothing, a
/// permission for a tool the stage never granted), so it makes the response
/// invalid alongside the structural errors. Warnings and notes are reported
/// separately and do not.
///
/// `dir` is the agent's own directory when the request named one, and that is
/// what makes the tool check meaningful: the lint resolves `tools/*.rhai`
/// relative to it, so validating an edit of an existing agent without it
/// reported every tool that agent defines as unknown and refused the save.
/// A manifest typed from nothing has no directory to offer, and then the lint
/// runs against the built-in tool set alone, which is the most that can be
/// said about it.
fn validate_manifest_text(manifest: &str, dir: &Path) -> ValidateResponse {
    let bp = match parse_manifest(manifest) {
        Ok(bp) => bp,
        Err(e) => return ValidateResponse::invalid(vec![e.to_string()]),
    };
    if let Err(e) = bp.validate() {
        return ValidateResponse::invalid(vec![e.to_string()]);
    }

    let env = crate::lint::LintEnv::offline(dir);
    let findings = crate::lint::lint_manifest(manifest, &bp, &env);
    let (errors, warnings): (Vec<_>, Vec<_>) = findings
        .iter()
        .partition(|f| f.severity == crate::lint::LintSeverity::Error);
    let render = |f: &&crate::lint::LintFinding| format!("{} [{}]", f.one_line(), f.code);

    ValidateResponse {
        valid: errors.is_empty(),
        errors: (!errors.is_empty()).then(|| errors.iter().map(render).collect()),
        warnings: (!warnings.is_empty()).then(|| warnings.iter().map(render).collect()),
    }
}

#[cfg(test)]
mod listing_tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    use crate::config::Config;

    fn manifest(name: &str, description: &str) -> String {
        format!(
            r#"
[agent]
name = "{name}"
version = "1.0.0"
description = "{description}"

[stages.zzstage-work]
system_prompt = "do it"
"#
        )
    }

    /// A catalog directory holding the named blueprints, each name prefixed so
    /// it cannot be confused with an installed one.
    fn catalog(entries: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, description) in entries {
            let name = format!("{FIXTURE_PREFIX}{name}");
            let sub = dir.path().join(&name);
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("agent.leviath"), manifest(&name, description)).unwrap();
        }
        dir
    }

    /// A fixture's full name.
    fn fx(name: &str) -> String {
        format!("{FIXTURE_PREFIX}{name}")
    }

    /// Fixture names all start with this, so assertions can pick them out of a
    /// catalog that also contains whatever the developer running the tests has
    /// installed.
    ///
    /// `discover_blueprints` always scans the installed agents dir on top of
    /// `agent_paths`, and redirecting `LEVIATH_HOME` to hide it would mutate
    /// process-global env and break every concurrently-running test that
    /// resolves an agents path. So these assert invariants over the discovered
    /// catalog instead of pinning its exact contents - which is the house rule
    /// for agent tests anyway.
    const FIXTURE_PREFIX: &str = "zzfixture-";

    async fn page(dir: &tempfile::TempDir, extra: &str) -> (StatusCode, serde_json::Value) {
        let (tx, _) = broadcast::channel(64);
        let state = AppState {
            update_check: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config {
                agent_paths: vec![dir.path().to_path_buf()],
                ..Default::default()
            }),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Arc::new(crate::commands::serve::types::ServeLimits::default()),
        };
        let app = Router::new()
            .route("/api/blueprints", get(list_blueprints))
            .with_state(state);
        let req = Request::builder()
            .uri(format!("/api/blueprints{extra}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
        )
    }

    /// Just this test's fixtures, in the order the page returned them.
    fn fixture_names(page: &serde_json::Value) -> Vec<String> {
        names(page)
            .into_iter()
            .filter(|n| n.starts_with(FIXTURE_PREFIX))
            .collect()
    }

    fn names(page: &serde_json::Value) -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn the_catalog_pages_through_every_blueprint_exactly_once() {
        let dir = catalog(&[
            ("alpha", "first"),
            ("bravo", "second"),
            ("charlie", "third"),
            ("delta", "fourth"),
        ]);

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let extra = match cursor {
                None => "?limit=2".to_string(),
                Some(ref c) => format!("?limit=2&cursor={c}"),
            };
            let (status, body) = page(&dir, &extra).await;
            assert_eq!(status, StatusCode::OK);
            seen.extend(names(&body));
            match body["next_cursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }
        // Every fixture, once, in order - regardless of what else the catalog
        // holds, and regardless of which page each landed on.
        let got: Vec<String> = seen
            .iter()
            .filter(|n| n.starts_with(FIXTURE_PREFIX))
            .cloned()
            .collect();
        assert_eq!(
            got,
            vec![fx("alpha"), fx("bravo"), fx("charlie"), fx("delta")]
        );
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), seen.len(), "a blueprint was returned twice");
    }

    /// The part of this change with real value: the catalog is small, so
    /// filtering is what a person actually wants from it.
    #[tokio::test]
    async fn q_matches_name_description_and_stage_names() {
        let dir = catalog(&[
            ("researcher", "digs through papers"),
            ("coder", "writes zzrust"),
        ]);

        // The prefix makes the needle unique to this test's fixtures.
        let (_, by_name) = page(&dir, "?q=ZZFIXTURE-RESEARCH").await;
        assert_eq!(names(&by_name), vec![fx("researcher")]);

        let (_, by_description) = page(&dir, "?q=writes+zzrust").await;
        assert_eq!(names(&by_description), vec![fx("coder")]);

        // Both fixtures declare a stage named after the prefix.
        let (_, by_stage) = page(&dir, "?q=zzstage").await;
        assert_eq!(fixture_names(&by_stage).len(), 2);

        let (_, nothing) = page(&dir, "?q=nothing-like-this-at-all").await;
        assert!(names(&nothing).is_empty());
        assert_eq!(nothing["total"], 0);
    }

    /// Versions collide freely, so this is the sort where the name tie-break
    /// actually does work.
    #[tokio::test]
    async fn the_catalog_can_be_sorted_by_version() {
        let dir = catalog(&[("alpha", "a"), ("bravo", "b")]);
        let (status, body) = page(&dir, "?sort=version&limit=200").await;
        assert_eq!(status, StatusCode::OK);
        // Both fixtures declare 1.0.0, so the shared version leaves the name
        // to order them.
        assert_eq!(fixture_names(&body), vec![fx("alpha"), fx("bravo")]);
    }

    #[tokio::test]
    async fn the_catalog_can_be_ordered_backwards() {
        let dir = catalog(&[("alpha", "a"), ("bravo", "b")]);
        let (_, body) = page(&dir, "?order=desc&limit=200").await;
        assert_eq!(fixture_names(&body), vec![fx("bravo"), fx("alpha")]);
    }

    #[tokio::test]
    async fn a_bad_sort_order_or_limit_is_refused() {
        let dir = catalog(&[("alpha", "a")]);
        for extra in [
            "?sort=whenever",
            "?order=sideways",
            "?limit=0",
            "?cursor=zz",
        ] {
            let (status, _) = page(&dir, extra).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "expected {extra} to be rejected"
            );
        }
    }

    /// A cursor names a position in one filtered list; changing the filter
    /// under it cannot produce a meaningful continuation.
    #[tokio::test]
    async fn a_cursor_is_bound_to_the_query_that_minted_it() {
        let dir = catalog(&[("alpha", "a"), ("bravo", "b"), ("charlie", "c")]);
        let (_, first) = page(&dir, "?limit=1").await;
        let cursor = first["next_cursor"].as_str().unwrap().to_string();

        let (ok, _) = page(&dir, &format!("?limit=1&cursor={cursor}")).await;
        assert_eq!(ok, StatusCode::OK);

        let (changed, _) = page(&dir, &format!("?limit=1&q=alpha&cursor={cursor}")).await;
        assert_eq!(changed, StatusCode::BAD_REQUEST);
    }
}

#[cfg(test)]
mod canonicalize_tests {
    use super::*;

    fn info(name: &str, path: &str) -> BlueprintInfo {
        BlueprintInfo {
            name: name.to_string(),
            version: "1".to_string(),
            description: String::new(),
            path: path.to_string(),
            stages: vec![],
            manifest: String::new(),
        }
    }

    /// The scan order decides the winner, so the *earlier* root keeps the name
    /// even though its path sorts later. Getting this backwards would silently
    /// change which agent a spawn runs.
    #[test]
    fn duplicate_names_keep_the_first_scanned_not_the_first_sorted() {
        let out = canonicalize(vec![
            info("coder", "/zzz/installed"),
            info("coder", "/aaa/custom"),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "/zzz/installed");
    }

    #[test]
    fn output_is_name_sorted_with_path_as_the_tie_break() {
        let out = canonicalize(vec![
            info("zebra", "/b"),
            info("alpha", "/z"),
            info("alpha2", "/a"),
        ]);
        let names: Vec<&str> = out.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "alpha2", "zebra"]);
    }

    /// Distinct names from the same root all survive - the dedup keys on name,
    /// not on the directory a blueprint was found in.
    #[test]
    fn distinct_names_are_all_kept() {
        let out = canonicalize(vec![info("a", "/1"), info("b", "/2"), info("c", "/3")]);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn an_empty_scan_canonicalizes_to_an_empty_catalog() {
        assert!(canonicalize(vec![]).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_test_agent;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{get, post};
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    use crate::config::Config;

    fn test_state_with_path(path: PathBuf) -> AppState {
        let (tx, _) = broadcast::channel(64);
        AppState {
            update_check: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config {
                agent_paths: vec![path],
                ..Default::default()
            }),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        }
    }

    fn test_manifest() -> &'static str {
        r#"
[agent]
name = "test-bp"
version = "1.0.0"
description = "A test blueprint"

[stages.plan]
system_prompt = "Plan the work"
"#
    }

    // ─── list_blueprints ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_blueprints_empty_path_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints", get(list_blueprints))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        // The envelope, not a bare array - the breaking change this release
        // takes deliberately.
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(page["items"].is_array());
        assert!(page["total"].is_number());
        assert!(page["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn list_blueprints_with_agent_returns_it() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.leviath"), test_manifest()).unwrap();

        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints", get(list_blueprints))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let blueprints = page["items"].as_array().unwrap().clone();
        assert_test_bp_listed(&blueprints);
    }

    fn assert_test_bp_listed(blueprints: &[serde_json::Value]) {
        assert!(
            blueprints
                .iter()
                .any(|b| b["name"].as_str() == Some("test-bp")),
            "test-bp should be listed"
        );
    }

    #[test]
    #[should_panic(expected = "test-bp should be listed")]
    fn assert_test_bp_listed_panics_when_missing() {
        assert_test_bp_listed(&[]);
    }

    // ─── get_blueprint ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_blueprint_existing_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("test-bp");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.leviath"), test_manifest()).unwrap();

        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints/{name}", get(get_blueprint))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints/test-bp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let bp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(bp["name"].as_str().unwrap(), "test-bp");
        assert_eq!(bp["version"].as_str().unwrap(), "1.0.0");
        // The manifest text comes with it. Without this a console has to guess
        // what it is editing, and the guesses available to a browser are a
        // local draft or a copy bundled at build time - neither of which is
        // the file the daemon runs.
        assert_eq!(bp["manifest"].as_str().unwrap(), test_manifest());
    }

    /// A console showed a blueprint's stages and nothing about its memory, so a
    /// person editing an agent could see what it *does* and not what it
    /// *keeps* - which is the half that decides whether it can do the job on a
    /// small window.
    #[tokio::test]
    async fn the_detail_route_reports_the_blueprints_context_regions() {
        let manifest = r#"
[agent]
name = "curator"

[context.regions]
sources = { kind = "pinned", max_tokens = 400, describe_in_prompt = true, description = "One line per source." }
chat = { kind = "sliding_window", max_tokens = 900 }

[stages.plan]
system_prompt = "Plan the work"
"#;
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("curator");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.leviath"), manifest).unwrap();

        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints/{name}", get(get_blueprint))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints/curator")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let bp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Compared whole, so a field that stops being serialized fails here
        // rather than passing a check that only looks at the fields it knows.
        assert_eq!(
            bp["regions"],
            serde_json::json!([
                {
                    "name": "sources",
                    "kind": "pinned",
                    "description": "One line per source.",
                    "describe_in_prompt": true,
                    "max_tokens": 400,
                },
                {
                    "name": "chat",
                    // The blueprint's own word, and the one a context snapshot
                    // writes: a console reading both sees one kind, not two.
                    "kind": "sliding_window",
                    "describe_in_prompt": false,
                    "max_tokens": 900,
                },
            ])
        );
    }

    /// The detail route resolves each fan-out stage's limits the way the
    /// daemon does: the default filled in for a stage that names none, `null`
    /// for a cap that is not there (`0` in the manifest, or no `max_items`),
    /// and the number itself otherwise. Stages that do not fan out are not
    /// listed, and a blueprint with no fan-out has an empty list rather than
    /// no key.
    #[tokio::test]
    async fn the_detail_route_reports_the_blueprints_fan_out_limits() {
        let manifest = r#"
[agent]
name = "spreader"

[stages.plan]
system_prompt = "Plan the work"

[stages.spread]
mode = "fan_out"
worker_stage = "worker"
merge_stage = "gather"
split_prompt = "split it"
results_region = "findings"
on_worker_failure = "fail_all"
max_workers = 0
max_items = 12

[stages.wide]
mode = "fan_out"
worker_agent = "researcher"
split_prompt = "split it"

[stages.worker]
system_prompt = "Do one part"
allow_as_worker = true

[stages.gather]
system_prompt = "Gather"

[context.regions]
findings = { kind = "clearable", max_tokens = 4000 }
"#;
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("spreader");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.leviath"), manifest).unwrap();

        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints/{name}", get(get_blueprint))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints/spreader")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let bp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            bp["fan_outs"],
            serde_json::json!([
                {
                    "stage": "spread",
                    "worker_stage": "worker",
                    "merge_stage": "gather",
                    "max_workers": null,
                    "max_items": 12,
                    "on_worker_failure": "fail_all",
                    "results_region": "findings",
                },
                {
                    "stage": "wide",
                    "worker_agent": "researcher",
                    "max_workers": leviath_core::blueprint::DEFAULT_MAX_WORKERS,
                    "max_items": null,
                    "on_worker_failure": "continue",
                },
            ])
        );
    }

    /// A blueprint that never fans out reports an empty list, so a client can
    /// tell "no fan-out here" from a daemon too old to say.
    #[tokio::test]
    async fn a_blueprint_without_fan_out_reports_an_empty_fan_outs_list() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("test-bp");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.leviath"), test_manifest()).unwrap();

        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints/{name}", get(get_blueprint))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints/test-bp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let bp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(bp["fan_outs"], serde_json::json!([]));
    }

    /// The listing must not carry manifests: it answers "what agents are
    /// there", and one manifest per agent would make that question cost the
    /// contents of every agent on the machine.
    #[tokio::test]
    async fn the_listing_does_not_carry_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("test-bp");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.leviath"), test_manifest()).unwrap();

        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints", get(list_blueprints))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = page["items"].as_array().expect("an items array");
        assert!(!items.is_empty(), "the agent is listed");
        assert!(
            items.iter().all(|b| b.get("manifest").is_none()),
            "the listing stays a catalog: {items:?}"
        );
    }

    /// A manifest that cannot be read is not discovered at all, so the detail
    /// route answers 404 rather than inventing a blueprint with no text. This
    /// is why the route needs no "could not read it" arm: there is no state in
    /// which discovery succeeds and the manifest is missing.
    #[tokio::test]
    async fn a_blueprint_whose_manifest_cannot_be_read_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("test-bp");
        std::fs::create_dir_all(&agent_dir).unwrap();
        // A directory where the manifest should be: it exists, and no platform
        // will read it as text.
        std::fs::create_dir_all(agent_dir.join("agent.leviath")).unwrap();

        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints/{name}", get(get_blueprint))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints/test-bp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_blueprint_not_found_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints/{name}", get(get_blueprint))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints/does-not-exist-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    /// Unique blueprint name so tests operating against the real
    /// `~/.leviath/agents` dir (create/update/delete have no path DI seam)
    /// don't collide with each other or with a developer's real agents.
    fn unique_bp_name(prefix: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!("test-bp-{}-{}-{}", prefix, std::process::id(), nanos)
    }

    // ─── create_blueprint ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_blueprint_valid_manifest_returns_ok() {
        let name = unique_bp_name("create");
        let manifest = format!(
            r#"
[agent]
name = "{name}"
version = "1.0.0"
description = "Created via API"

[stages.plan]
system_prompt = "Plan the work"
"#
        );

        let app = Router::new().route("/api/blueprints", post(create_blueprint));
        let body = serde_json::json!({ "name": name, "manifest": manifest });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(info["name"].as_str().unwrap(), name);
        assert_eq!(info["stages"].as_array().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(agents_dir().join(&name));
    }

    /// `POST /api/blueprints` with a traversing name created a directory and
    /// wrote attacker-controlled TOML wherever it pointed. `Path::join` neither
    /// normalizes `..` nor resists an absolute path, so the name had to be
    /// validated rather than trusted.
    #[tokio::test]
    async fn create_blueprint_rejects_traversing_names() {
        let manifest = r#"
[agent]
name = "x"
version = "1.0.0"
description = "d"

[stages.plan]
system_prompt = "p"
"#;
        for name in [
            "../../../../tmp/leviath-traversal-probe",
            "/tmp/leviath-traversal-probe",
            "..",
            "a/b",
        ] {
            let app = Router::new().route("/api/blueprints", post(create_blueprint));
            let body = serde_json::json!({ "name": name, "manifest": manifest });
            let req = Request::builder()
                .method("POST")
                .uri("/api/blueprints")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                axum::http::StatusCode::BAD_REQUEST,
                "name {name:?} should be refused"
            );
        }
        assert!(
            !std::path::Path::new("/tmp/leviath-traversal-probe").exists(),
            "nothing may be created outside the agents directory"
        );
    }

    /// `DELETE /api/blueprints/{name}` reached `fs::remove_dir_all` on the same
    /// unvalidated join - arbitrary recursive deletion for any token holder.
    /// A percent-encoded `..%2f` decodes *after* segment matching, so the
    /// decoded form is what has to be rejected.
    #[tokio::test]
    async fn delete_blueprint_rejects_traversing_names() {
        let victim = std::env::temp_dir().join("leviath-delete-probe");
        std::fs::create_dir_all(&victim).unwrap();

        let app = Router::new().route(
            "/api/blueprints/{name}",
            axum::routing::delete(delete_blueprint),
        );
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/blueprints/..%2f..%2f..%2f..%2ftmp%2fleviath-delete-probe")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        assert!(victim.exists(), "the directory must not have been deleted");
        let _ = std::fs::remove_dir_all(&victim);
    }

    #[tokio::test]
    async fn create_blueprint_dir_creation_failure_returns_500() {
        // Force `create_dir_all` to fail deterministically by pre-creating a
        // regular *file* at the target path - a directory can't be created
        // where a non-directory entry already exists. This is cross-platform:
        // both Unix (ENOTDIR/EEXIST) and Windows (ERROR_ALREADY_EXISTS) refuse
        // to create a directory at a path that's already occupied by a file.
        let name = unique_bp_name("create-fail");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(agents_dir()).unwrap();
        std::fs::write(&dir, b"blocking file").unwrap();

        let app = Router::new().route("/api/blueprints", post(create_blueprint));
        let manifest = format!(
            "\n[agent]\nname = \"{name}\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\n[stages.plan]\nsystem_prompt = \"p\"\n"
        );
        let body = serde_json::json!({ "name": name, "manifest": manifest });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        let _ = std::fs::remove_file(&dir);
    }

    #[tokio::test]
    async fn create_blueprint_invalid_manifest_returns_400() {
        let app = Router::new().route("/api/blueprints", post(create_blueprint));
        let body = serde_json::json!({
            "name": "bad-agent",
            "manifest": "not valid toml [[[{"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_blueprint_manifest_write_failure_returns_500() {
        // Distinct from `create_blueprint_dir_creation_failure_returns_500`:
        // here `create_dir_all` succeeds (the blueprint dir doesn't already
        // exist as a blocking file), but the manifest *file* write fails --
        // forced by pre-creating a directory at the exact path
        // `<dir>/agent.leviath`, so `std::fs::write` hits EISDIR.
        let name = unique_bp_name("create-manifest-write-fail");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(dir.join("agent.leviath")).unwrap();

        let app = Router::new().route("/api/blueprints", post(create_blueprint));
        let manifest = format!(
            "\n[agent]\nname = \"{name}\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\n[stages.plan]\nsystem_prompt = \"p\"\n"
        );
        let body = serde_json::json!({ "name": name, "manifest": manifest });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── update_blueprint ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn update_blueprint_write_failure_returns_500() {
        use axum::routing::put;

        // Force `std::fs::write` to fail deterministically: the manifest
        // file exists (so the not-found check passes) but is read-only, so
        // overwriting it fails. `set_readonly` is cross-platform (Unix
        // clears/sets the owner-write bit; Windows toggles the FILE_ATTRIBUTE
        // _READONLY flag), and both platforms' `std::fs::write` honor it.
        let name = unique_bp_name("update-fail");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest_path = dir.join("agent.leviath");
        std::fs::write(&manifest_path, test_manifest()).unwrap();
        let original = std::fs::metadata(&manifest_path).unwrap().permissions();
        let mut perms = original.clone();
        perms.set_readonly(true);
        std::fs::set_permissions(&manifest_path, perms).unwrap();

        let app = Router::new().route("/api/blueprints/{name}", put(update_blueprint));
        let body = serde_json::json!({ "manifest": test_manifest() });
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/api/blueprints/{}", name))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        // Put the original permissions back so the directory can be removed on
        // Windows, where a read-only file cannot be deleted. Restoring what was
        // there beats `set_readonly(false)`, which on Unix sets *every* write
        // bit and would hand back a mode the file never had.
        let _ = std::fs::set_permissions(&manifest_path, original);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_blueprint_existing_returns_ok() {
        use axum::routing::put;

        let name = unique_bp_name("update");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            format!(
                r#"
[agent]
name = "{name}"
version = "1.0.0"
description = "Original"

[stages.plan]
system_prompt = "Plan"
"#
            ),
        )
        .unwrap();

        let app = Router::new().route("/api/blueprints/{name}", put(update_blueprint));
        let updated_manifest = format!(
            r#"
[agent]
name = "{name}"
version = "2.0.0"
description = "Updated"

[stages.plan]
system_prompt = "Plan"

[stages.implement]
system_prompt = "Implement"
"#
        );
        let body = serde_json::json!({ "manifest": updated_manifest });
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/api/blueprints/{}", name))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(info["version"].as_str().unwrap(), "2.0.0");
        assert_eq!(info["stages"].as_array().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_blueprint_invalid_manifest_returns_400() {
        use axum::routing::put;

        let app = Router::new().route("/api/blueprints/{name}", put(update_blueprint));
        let body = serde_json::json!({
            "manifest": "not valid toml {{{"
        });
        let req = Request::builder()
            .method("PUT")
            .uri("/api/blueprints/my-agent")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    /// `PUT` runs the same unvalidated join as `POST` and `DELETE` did, and it
    /// ends in `fs::write` of caller-supplied TOML. A valid manifest with a
    /// traversing *name* must be refused before the path is built, not after.
    #[tokio::test]
    async fn update_blueprint_rejects_traversing_names() {
        use axum::routing::put;

        let manifest = r#"
[agent]
name = "x"
version = "1.0.0"
description = "d"

[stages.plan]
system_prompt = "p"
"#;
        for name in ["..", "%2e%2e", "."] {
            let app = Router::new().route("/api/blueprints/{name}", put(update_blueprint));
            let body = serde_json::json!({ "manifest": manifest });
            let req = Request::builder()
                .method("PUT")
                .uri(format!("/api/blueprints/{name}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                axum::http::StatusCode::BAD_REQUEST,
                "name {name:?} should be refused"
            );
        }
    }

    #[tokio::test]
    async fn update_blueprint_not_found_returns_404() {
        use axum::routing::put;

        let app = Router::new().route("/api/blueprints/{name}", put(update_blueprint));
        let body = serde_json::json!({
            "manifest": r#"
[agent]
name = "no-such-agent"
version = "1.0.0"
description = "Missing"

[stages.run]
system_prompt = "Run"
"#
        });
        let req = Request::builder()
            .method("PUT")
            .uri("/api/blueprints/no-such-agent-xyz-99999")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // ─── delete_blueprint ─────────────────────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_blueprint_removal_failure_returns_500() {
        use axum::routing::delete;
        use std::os::unix::fs::PermissionsExt;

        // Force `remove_dir_all` to fail deterministically: the blueprint
        // dir exists (so the not-found check passes) but is made read-only
        // and non-executable, so unlinking its contents fails with EACCES.
        let name = unique_bp_name("delete-fail");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.leviath"), test_manifest()).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let app = Router::new().route("/api/blueprints/{name}", delete(delete_blueprint));
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/blueprints/{}", name))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        // Restore perms so cleanup (and any subsequent test) can remove it.
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows twin of `delete_blueprint_removal_failure_returns_500`.
    ///
    /// On Unix, directory *write* permission (not the file's own
    /// permission) governs whether an entry can be unlinked from a
    /// directory, so making the directory `0o555` is what forces
    /// `remove_dir_all` to fail there. Windows has no equivalent
    /// "directory write permission" concept via `std::fs::Permissions`, and
    /// marking a file inside the directory read-only does NOT make
    /// `remove_dir_all` fail on Windows: it clears the read-only attribute
    /// before deleting, the same way it silently succeeds through other
    /// removable-but-`readonly` obstacles. A real sharing violation does
    /// still block deletion, though: holding an exclusive (no-share) file
    /// handle open on a file inside the directory for the duration of the
    /// request - the same technique
    /// `session.rs`'s `resolve_task_unreadable_file_returns_error` Windows
    /// twin uses - reliably makes `remove_dir_all` fail there.
    #[cfg(windows)]
    #[tokio::test]
    async fn delete_blueprint_removal_failure_returns_500_windows() {
        use axum::routing::delete;
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        let name = unique_bp_name("delete-fail-win");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest_path = dir.join("agent.leviath");

        // Create the manifest THROUGH an exclusive (no-share) handle and hold
        // it open for the duration of the delete attempt below, so
        // `remove_dir_all` hits a sharing violation trying to unlink
        // `manifest_path`. Writing the file first and reopening it exclusively
        // was a CI flake: Windows Defender / the indexer briefly opens a
        // just-written file, and then it is OUR exclusive open that gets the
        // sharing violation. Creating it exclusively from the start leaves no
        // closed-file window for a scanner to grab.
        let mut locked = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .share_mode(0)
            .open(&manifest_path)
            .unwrap();
        std::io::Write::write_all(&mut locked, test_manifest().as_bytes()).unwrap();
        let _locked = locked;

        let app = Router::new().route("/api/blueprints/{name}", delete(delete_blueprint));
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/blueprints/{}", name))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        drop(_locked);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_blueprint_existing_returns_no_content() {
        use axum::routing::delete;

        let name = unique_bp_name("delete");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.leviath"), test_manifest()).unwrap();
        assert!(dir.exists());

        let app = Router::new().route("/api/blueprints/{name}", delete(delete_blueprint));
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/blueprints/{}", name))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NO_CONTENT);
        assert_dir_removed(&dir);
    }

    fn assert_dir_removed(dir: &std::path::Path) {
        assert!(!dir.exists(), "directory should be removed");
    }

    #[test]
    #[should_panic(expected = "directory should be removed")]
    fn assert_dir_removed_panics_when_still_present() {
        assert_dir_removed(std::path::Path::new("."));
    }

    #[tokio::test]
    async fn delete_blueprint_not_found_returns_404() {
        use axum::routing::delete;

        let app = Router::new().route("/api/blueprints/{name}", delete(delete_blueprint));
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/blueprints/nonexistent-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // ─── validate_blueprint ───────────────────────────────────────────────────

    #[tokio::test]
    async fn validate_blueprint_valid_manifest_returns_ok_valid_true() {
        let app = Router::new().route("/api/blueprints/validate", post(validate_blueprint));
        let body = serde_json::json!({"manifest": test_manifest()});
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: ValidateResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(result.valid);
        assert!(result.errors.is_none());
    }

    /// A blueprint the lint objects to but `Blueprint::validate` does not.
    /// `valid` follows the lint errors, and the warnings ride alongside without
    /// affecting it.
    #[tokio::test]
    async fn validate_blueprint_reports_lint_errors_and_warnings_separately() {
        // `raed_file` is an error (it resolves to nothing); the missing
        // `max_iterations` and the unattended `ask_user_text` are warnings.
        let manifest = r#"
[agent]
name = "linty"
version = "0.1.0"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
available_tools = ["read_file", "raed_file", "ask_user_text"]

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        let result = validate_manifest_text(manifest, Path::new("."));
        assert!(!result.valid);
        let errors = result.errors.expect("the typo is an error");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("unknown-tool"), "{errors:?}");
        let warnings = result.warnings.expect("the defaults are warnings");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("stage-missing-max-iterations")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("blocking-tool-in-autonomous-stage")),
            "{warnings:?}"
        );
    }

    /// An agent's own `tools/*.rhai` resolve when the request says which agent
    /// this is.
    ///
    /// This is the fault that stopped any tool-bearing agent being saved from
    /// the console: the lint was rooted at the daemon's working directory, so
    /// every tool the agent itself defines came back as unknown, at error
    /// severity, and the pre-flight refused the save. The pair matters more
    /// than either half - rooted at the agent the grant is fine, rooted
    /// anywhere else it is an error - so both are asserted against the same
    /// manifest.
    #[tokio::test]
    async fn a_manifest_naming_its_agent_resolves_that_agents_own_tools() {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(
            tools.join("web_search.rhai"),
            "// @tool web_search\n// @description searches\n\"found\"",
        )
        .unwrap();
        let manifest = r#"
[agent]
name = "toolful"
version = "0.1.0"
description = "d"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main"
max_iterations = 5
available_tools = ["web_search"]

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;

        let rooted = validate_manifest_text(manifest, dir.path());
        assert!(
            rooted.valid,
            "the agent's own tool resolves: {:?}",
            rooted.errors
        );

        // The same manifest judged from somewhere that has no such tools/ is
        // exactly the failure users hit.
        let elsewhere = tempfile::tempdir().unwrap();
        let unrooted = validate_manifest_text(manifest, elsewhere.path());
        assert!(!unrooted.valid);
        let errors = unrooted.errors.expect("the grant resolves to nothing");
        assert!(errors[0].contains("unknown-tool"), "{errors:?}");
    }

    /// The handler turns the request's `name` into that agent's directory, and
    /// tolerates a name it cannot use rather than failing the whole request.
    #[tokio::test]
    async fn validate_accepts_a_blueprint_name_and_ignores_an_unusable_one() {
        let app = Router::new().route("/api/blueprints/validate", post(validate_blueprint));
        let manifest = r#"
[agent]
name = "plain"
version = "0.1.0"
description = "d"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        // A traversal attempt is not a 400 here: the manifest can still be
        // judged, just without a directory behind it.
        for name in [
            serde_json::json!("../../etc"),
            serde_json::json!("no-such-agent"),
            serde_json::Value::Null,
        ] {
            let body = serde_json::json!({ "manifest": manifest, "name": name }).to_string();
            let req = Request::builder()
                .method("POST")
                .uri("/api/blueprints/validate")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK, "{name}");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let out: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(out["valid"], serde_json::json!(true), "{name}");
        }
    }

    /// Warnings alone leave the blueprint valid.
    #[tokio::test]
    async fn validate_blueprint_with_only_warnings_stays_valid() {
        let manifest = r#"
[agent]
name = "warny"
version = "0.1.0"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        let result = validate_manifest_text(manifest, Path::new("."));
        assert!(result.valid);
        assert!(result.errors.is_none());
        assert_eq!(result.warnings.expect("no max_iterations").len(), 1);
    }

    #[tokio::test]
    async fn validate_blueprint_invalid_manifest_returns_ok_valid_false() {
        let app = Router::new().route("/api/blueprints/validate", post(validate_blueprint));
        let body = serde_json::json!({"manifest": "not toml at all [[[{"});
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: ValidateResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(!result.valid);
        assert!(result.errors.is_some());
    }

    #[tokio::test]
    async fn validate_blueprint_parses_but_fails_structural_validation_returns_ok_valid_false() {
        // Distinct from the manifest above: this one parses fine as TOML/a
        // Blueprint (Ok(bp) from parse_manifest), but bp.validate()
        // itself rejects it - an entry_stage that doesn't match any defined
        // stage. Exercises the `Ok(bp) => match bp.validate() { Err(e) => .. }`
        // arm, which `validate_blueprint_invalid_manifest_returns_ok_valid_false`
        // (a parse failure) never reaches.
        let app = Router::new().route("/api/blueprints/validate", post(validate_blueprint));
        let manifest = r#"
[agent]
name = "bad-entry-stage"
version = "1.0.0"
description = "Entry stage doesn't exist"
entry_stage = "does-not-exist"

[stages.plan]
system_prompt = "Plan"
"#;
        let body = serde_json::json!({"manifest": manifest});
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: ValidateResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(!result.valid);
        assert!(
            result
                .errors
                .unwrap()
                .iter()
                .any(|e| e.contains("entry_stage"))
        );
    }

    #[test]
    fn agents_dir_is_under_home() {
        let dir = agents_dir();
        let path_str = dir.to_string_lossy();
        assert!(path_str.contains(".leviath"));
        assert!(path_str.ends_with("agents"));
    }

    #[test]
    fn read_blueprint_info_from_valid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("agent.leviath");
        let content = r#"
[agent]
name = "test-bp"
version = "1.0.0"
description = "A test blueprint"

[stages.plan]
system_prompt = "Plan the work"
"#;
        std::fs::write(&manifest_path, content).unwrap();

        let info = read_blueprint_info(&manifest_path, dir.path()).unwrap();
        assert_eq!(info.name, "test-bp");
        assert_eq!(info.version, "1.0.0");
        assert_eq!(info.description, "A test blueprint");
        assert_eq!(info.stages, vec!["plan"]);
        assert_eq!(info.path, dir.path().to_string_lossy());
    }

    #[test]
    fn read_blueprint_info_nonexistent_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("nonexistent.leviath");
        let result = read_blueprint_info(&manifest_path, dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn read_blueprint_info_invalid_toml_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("agent.leviath");
        std::fs::write(&manifest_path, "not valid toml [[[").unwrap();
        let result = read_blueprint_info(&manifest_path, dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn read_blueprint_info_multiple_stages() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("agent.leviath");
        let content = r#"
[agent]
name = "multi-stage"
version = "0.2.0"
description = "Multi-stage"

[stages.plan]
system_prompt = "Plan"

[stages.implement]
system_prompt = "Implement"

[stages.review]
system_prompt = "Review"
"#;
        std::fs::write(&manifest_path, content).unwrap();

        let info = read_blueprint_info(&manifest_path, dir.path()).unwrap();
        assert_eq!(info.name, "multi-stage");
        assert_eq!(info.stages.len(), 3);
    }

    #[test]
    fn discover_blueprints_with_custom_path() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        let content = r#"
[agent]
name = "discovered"
version = "1.0.0"
description = "Should be discovered"

[stages.work]
system_prompt = "Do work"
"#;
        write_test_agent(agent_dir, content);

        let config = crate::config::Config {
            agent_paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };

        let blueprints = discover_blueprints(&config);
        let found = blueprints.iter().find(|b| b.name == "discovered");
        assert_discovered_in_custom_path(found.is_some());
    }

    fn assert_discovered_in_custom_path(found: bool) {
        assert!(found, "should discover agent in custom path");
    }

    #[test]
    #[should_panic(expected = "should discover agent in custom path")]
    fn assert_discovered_in_custom_path_panics_when_not_found() {
        assert_discovered_in_custom_path(false);
    }

    #[test]
    fn discover_blueprints_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            agent_paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        // Should not panic even with empty dirs
        let blueprints = discover_blueprints(&config);
        // May include blueprints from ~/.leviath/agents, but no crash
        let _ = blueprints;
    }

    #[test]
    fn discover_blueprints_nonexistent_path_is_skipped() {
        let config = crate::config::Config {
            agent_paths: vec![PathBuf::from("/nonexistent/path/unlikely_to_exist_12345")],
            ..Default::default()
        };
        // Should not panic
        let _ = discover_blueprints(&config);
    }

    #[test]
    fn discover_blueprints_direct_manifest_in_dir() {
        let dir = tempfile::tempdir().unwrap();
        let content = r#"
[agent]
name = "direct"
version = "0.1.0"
description = "Directly in scan dir"

[stages.run]
system_prompt = "Run"
"#;
        write_test_agent(dir.path(), content);

        let config = crate::config::Config {
            agent_paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };

        let blueprints = discover_blueprints(&config);
        let found = blueprints.iter().find(|b| b.name == "direct");
        assert_discovered_directly_in_scan_dir(found.is_some());
    }

    fn assert_discovered_directly_in_scan_dir(found: bool) {
        assert!(found, "should discover agent.leviath directly in scan dir");
    }

    #[test]
    #[should_panic(expected = "should discover agent.leviath directly in scan dir")]
    fn assert_discovered_directly_in_scan_dir_panics_when_not_found() {
        assert_discovered_directly_in_scan_dir(false);
    }
}
