//! Gathering what the bundle holds.
//!
//! Every file is named here, one by one. Nothing globs the data root, because
//! the root also holds `control.token`, `mcp-auth.json` and `provider-auth.json`,
//! and a bundle that copied whatever it found would ship a live credential the
//! day a new file appeared beside them. What is missing is recorded as
//! missing, so a reader can tell "there was no daemon log" from "the bundle
//! forgot it".

use std::path::{Path, PathBuf};

use leviath_core::files::{
    ARCHIVE_FILE, BLOBS_DIR, CONTEXT_FILE, FANOUT_FILE, INTERACTIONS_FILE, MANIFEST_FILENAME,
    META_FILE, STAGES_FILE,
};
use leviath_core::run_meta::RunMeta;
use leviath_core::secrets::is_sensitive_env_name;

use super::scrub::{self, Scrubber};
use super::{About, RageEnv, report};
use crate::commands::doctor::{DaemonTarget, DoctorArgs, run_checks};
use crate::commands::run::session::build_provider_registry_from_config;

/// The most a script, a blueprint file or a config file may weigh. Anything
/// larger is not the kind of file these are.
pub(crate) const SMALL_CAP: u64 = 256 * 1024;
/// How much of a log's tail is kept.
pub(crate) const LOG_TAIL: u64 = 2 * 1024 * 1024;
/// The most a log file is read to take that tail from. Past this the file is
/// left out: no log of Leviath's grows this large, and one that did is a
/// finding in its own right.
pub(crate) const LOG_READ_CAP: u64 = 64 * 1024 * 1024;
/// The most a run's context snapshot or stage file may weigh before it is
/// left out.
pub(crate) const RUN_FILE_CAP: u64 = 8 * 1024 * 1024;
/// The most a run journal may weigh before it is left out. A mature run's
/// journal is tens of megabytes, which is exactly what a reader needs.
pub(crate) const ARCHIVE_CAP: u64 = 64 * 1024 * 1024;
/// The most one stored part may weigh, and the most all of a bundle's parts
/// may weigh together.
pub(crate) const BLOB_CAP: u64 = 8 * 1024 * 1024;
pub(crate) const BLOBS_TOTAL_CAP: u64 = 32 * 1024 * 1024;

/// What counts as a text file when a directory of blueprint or script files
/// is copied. Anything else is left out by name.
const TEXT_EXTENSIONS: &[&str] = &[
    "leviath", "rhai", "toml", "md", "txt", "json", "yaml", "yml",
];

/// How deep a copied directory tree goes.
const MAX_TREE_DEPTH: usize = 4;

/// What the user chose, from the TUI or the flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    pub about: About,
    pub run_id: Option<String>,
    pub agent: Option<PathBuf>,
    pub note: String,
    pub include_blobs: bool,
}

/// One file in the bundle.
pub(crate) struct Member {
    pub path: String,
    pub bytes: Vec<u8>,
    pub redactions: usize,
    pub truncated: bool,
}

/// A file that was not copied, and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SkippedEntry {
    /// Where it would have been in the bundle.
    pub path: String,
    /// Why it is not: absent, over a cap, unreadable, or left out on purpose.
    pub reason: String,
}

/// One top-level directory of the bundle, summed up.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Section {
    /// The directory, with its trailing slash, or the file at the top level.
    pub name: String,
    /// How many members it holds.
    pub files: usize,
    /// Their total size before compression.
    pub bytes: u64,
    /// How many secrets were replaced across them.
    pub redactions: usize,
}

/// A member as `manifest.json` lists it.
#[derive(serde::Serialize)]
struct MemberEntry<'a> {
    path: &'a str,
    bytes: usize,
    redactions: usize,
    truncated: bool,
}

/// Everything gathered, before it is zipped.
#[derive(Default)]
pub(crate) struct Bundle {
    pub members: Vec<Member>,
    pub skipped: Vec<SkippedEntry>,
    /// Things worth knowing that belong to no one file.
    pub notes: Vec<String>,
}

impl Bundle {
    fn text(&mut self, path: impl Into<String>, text: String, redactions: usize, truncated: bool) {
        self.members.push(Member {
            path: path.into(),
            bytes: text.into_bytes(),
            redactions,
            truncated,
        });
    }

    fn bytes(&mut self, path: impl Into<String>, bytes: Vec<u8>, redactions: usize) {
        self.members.push(Member {
            path: path.into(),
            bytes,
            redactions,
            truncated: false,
        });
    }

    fn skip(&mut self, path: impl Into<String>, reason: impl Into<String>) {
        self.skipped.push(SkippedEntry {
            path: path.into(),
            reason: reason.into(),
        });
    }

    /// A JSON member, pretty-printed, with its strings scrubbed.
    fn json(&mut self, path: impl Into<String>, scrubber: &Scrubber, mut value: serde_json::Value) {
        let redactions = scrubber.scrub_json(&mut value);
        // A `Value` always serializes.
        let text = serde_json::to_string_pretty(&value).expect("a JSON value serializes");
        self.text(path, text, redactions, false);
    }

    /// How many replacements were made across every member.
    pub(crate) fn redactions(&self) -> usize {
        self.members.iter().map(|m| m.redactions).sum()
    }

    /// The members grouped by their first path component, in first-seen order.
    pub(crate) fn sections(&self) -> Vec<Section> {
        let mut sections: Vec<Section> = Vec::new();
        for member in &self.members {
            let name = match member.path.split_once('/') {
                Some((dir, _)) => format!("{dir}/"),
                None => member.path.clone(),
            };
            match sections.iter_mut().find(|s| s.name == name) {
                Some(section) => {
                    section.files += 1;
                    section.bytes += member.bytes.len() as u64;
                    section.redactions += member.redactions;
                }
                None => sections.push(Section {
                    name,
                    files: 1,
                    bytes: member.bytes.len() as u64,
                    redactions: member.redactions,
                }),
            }
        }
        sections
    }

    fn manifest(&self, sel: &Selection, created_at: &str) -> serde_json::Value {
        let members: Vec<MemberEntry<'_>> = self
            .members
            .iter()
            .map(|m| MemberEntry {
                path: &m.path,
                bytes: m.bytes.len(),
                redactions: m.redactions,
                truncated: m.truncated,
            })
            .collect();
        serde_json::json!({
            "created_at": created_at,
            "leviath_version": env!("CARGO_PKG_VERSION"),
            "build": crate::daemon::setup::CURRENT_BUILD,
            "about": sel.about,
            "run_id": sel.run_id,
            "agent": sel.agent.as_ref().map(|p| p.display().to_string()),
            "members": members,
            "skipped": self.skipped,
            "notes": self.notes,
        })
    }
}

/// Gather everything the selection calls for. Never fails: a source that
/// cannot be read is recorded as skipped, and the bundle says so.
pub(crate) async fn collect(env: &RageEnv, sel: &Selection, created_at: &str) -> Bundle {
    let mut bundle = Bundle::default();
    let scrubber = scrubber_for(env, &mut bundle);

    environment(env, &scrubber, &mut bundle);
    doctor(&scrubber, &mut bundle).await;
    bundle.json(
        "daemon.json",
        &scrubber,
        serde_json::to_value((env.daemon)()).unwrap_or_default(),
    );
    config_files(env, &scrubber, &mut bundle);
    blueprints(env, &scrubber, &mut bundle);
    copy_text_tree(
        &env.data_dir.join("tools"),
        "tools",
        1,
        &scrubber,
        &mut bundle,
    );
    copy_text_tree(
        &env.data_dir.join("providers"),
        "providers",
        1,
        &scrubber,
        &mut bundle,
    );
    logs(env, &scrubber, &mut bundle);

    match sel.about {
        About::Run => {
            if let Some(id) = &sel.run_id {
                run_family(env, &scrubber, &mut bundle, id, sel.include_blobs);
            }
        }
        About::Agent => {
            if let Some(path) = &sel.agent {
                blueprint_under_test(path, &scrubber, &mut bundle);
            }
        }
        About::Setup => setup_imports(env, &mut bundle),
        About::Other => {}
    }

    let readme = report::readme(
        sel.about,
        &sel.note,
        sel.run_id.as_deref(),
        env!("CARGO_PKG_VERSION"),
        crate::daemon::setup::CURRENT_BUILD,
        created_at,
        &bundle,
    );
    let (readme, redactions) = scrubber.scrub(&readme);
    bundle.text("README.md", readme, redactions, false);
    let manifest = bundle.manifest(sel, created_at);
    bundle.text(
        "manifest.json",
        serde_json::to_string_pretty(&manifest).expect("a JSON value serializes"),
        0,
        false,
    );
    bundle
}

/// A scrubber that knows every secret this machine holds: what the loaded
/// config carries (including keys that come from the environment and are in
/// no file), every credential-shaped environment variable, and the tokens in
/// the two auth stores. The stores are read for this alone and never copied.
fn scrubber_for(env: &RageEnv, bundle: &mut Bundle) -> Scrubber {
    let mut known = Vec::new();
    match crate::config::Config::load() {
        Ok(config) => known.extend(scrub::config_secrets(&config)),
        Err(e) => bundle.notes.push(format!(
            "the config did not load, so only its file text is scrubbed: {e}"
        )),
    }
    let names = (env.env_names)();
    known.extend(scrub::env_secrets(&names, &*env.env_lookup));
    for store in ["mcp-auth.json", "provider-auth.json"] {
        if let Ok(text) = std::fs::read_to_string(env.data_dir.join(store))
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
        {
            scrub::secret_strings_in(&value, &mut known);
        }
    }
    Scrubber::new(known)
}

/// `environment.json`: the machine and the install, with no values from the
/// environment, only which credential-shaped names are set.
fn environment(env: &RageEnv, scrubber: &Scrubber, bundle: &mut Bundle) {
    let names = (env.env_names)();
    let relevant: Vec<String> = names
        .into_iter()
        .filter(|name| {
            name.starts_with("LEVIATH_")
                || name.ends_with("_BASE_URL")
                || is_sensitive_env_name(name)
        })
        .collect();
    let present = scrub::present_names(&relevant, &*env.env_lookup);
    let value = serde_json::json!({
        "leviath_version": env!("CARGO_PKG_VERSION"),
        "build": crate::daemon::setup::CURRENT_BUILD,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
        "os_version": leviath_sys::osinfo::current_version(),
        "cpus": std::thread::available_parallelism().map(usize::from).ok(),
        "disk_free_bytes_at_data_dir": leviath_sys::disk::available_bytes(&env.data_dir),
        "locale": leviath_sys::locale::current_tag(),
        "install": (env.install)(),
        "data_dir": env.data_dir.display().to_string(),
        "config_path": env.config_path.display().to_string(),
        "runs_dir": env.runs_dir.display().to_string(),
        "env_vars_set_values_withheld": present,
    });
    bundle.json("environment.json", scrubber, value);
}

/// `doctor.json`: the offline checks, which bill nothing and start nothing.
async fn doctor(scrubber: &Scrubber, bundle: &mut Bundle) {
    let args = DoctorArgs {
        offline: true,
        ..DoctorArgs::default()
    };
    let checks = run_checks(
        &args,
        &build_provider_registry_from_config,
        DaemonTarget::Skip,
    )
    .await;
    bundle.json(
        "doctor.json",
        scrubber,
        serde_json::json!({ "offline": true, "checks": checks }),
    );
}

/// `config/`: the config file and its siblings, the policy file and its
/// rules, and the two state files beside them.
fn config_files(env: &RageEnv, scrubber: &Scrubber, bundle: &mut Bundle) {
    copy_toml(&env.config_path, "config/config.toml", scrubber, bundle);
    for name in ["yolo.toml", "mime_types.toml"] {
        copy_toml(
            &env.config_path.with_file_name(name),
            &format!("config/{name}"),
            scrubber,
            bundle,
        );
    }
    copy_toml(
        &env.policy_dir.join("policy.toml"),
        "config/policy.toml",
        scrubber,
        bundle,
    );
    copy_text_tree(
        &env.policy_dir.join("rules"),
        "config/rules",
        1,
        scrubber,
        bundle,
    );
    for name in ["ui-state.json", "model_capabilities.json"] {
        copy_json(
            &env.data_dir.join(name),
            &format!("config/{name}"),
            SMALL_CAP,
            scrubber,
            bundle,
        );
    }
}

/// `agents/`: every installed blueprint, text files only.
fn blueprints(env: &RageEnv, scrubber: &Scrubber, bundle: &mut Bundle) {
    let installed = installed_blueprints(&env.agents_dir);
    if installed.is_empty() {
        bundle.skip("agents/", "no installed blueprints");
    }
    for (name, dir) in installed {
        copy_text_tree(
            &dir,
            &format!("agents/{name}"),
            MAX_TREE_DEPTH,
            scrubber,
            bundle,
        );
    }
}

/// `logs/`: the daemon's log and the supervisor's capture, and the
/// dashboard's activity log, each with its rolled copy.
fn logs(env: &RageEnv, scrubber: &Scrubber, bundle: &mut Bundle) {
    let rolled = |path: &Path| {
        let mut name = path.as_os_str().to_owned();
        name.push(".1");
        PathBuf::from(name)
    };
    let daemon_log = env.data_dir.join("daemon.log");
    let sources = [
        (daemon_log.clone(), "logs/daemon.log"),
        (rolled(&daemon_log), "logs/daemon.log.1"),
        (
            env.data_dir.join("daemon.stdio.log"),
            "logs/daemon.stdio.log",
        ),
        (env.dashboard_log.clone(), "logs/dashboard.log"),
        (rolled(&env.dashboard_log), "logs/dashboard.log.1"),
    ];
    for (path, dest) in sources {
        tail_text(&path, dest, LOG_TAIL, scrubber, bundle);
    }
    // One log per `lev serve`, named for the server: `serve-<name>.log` and
    // its rolled copy. Found by name, never by copying whatever sits in the
    // directory beside them.
    for path in sorted_entries(&env.data_dir) {
        let name = file_name(&path);
        let is_serve_log =
            name.starts_with("serve-") && (name.ends_with(".log") || name.ends_with(".log.1"));
        if is_serve_log && path.is_file() {
            tail_text(&path, &format!("logs/{name}"), LOG_TAIL, scrubber, bundle);
        }
    }
}

/// `runs/<id>/` for the run and everything it spawned.
fn run_family(
    env: &RageEnv,
    scrubber: &Scrubber,
    bundle: &mut Bundle,
    root: &str,
    include_blobs: bool,
) {
    let metas = list_metas(&env.runs_dir);
    let mut blob_budget = BLOBS_TOTAL_CAP;
    for id in family(&metas, &env.runs_dir, root) {
        copy_run(env, scrubber, bundle, &id, include_blobs, &mut blob_budget);
    }
}

/// One run directory, file by file.
fn copy_run(
    env: &RageEnv,
    scrubber: &Scrubber,
    bundle: &mut Bundle,
    id: &str,
    include_blobs: bool,
    blob_budget: &mut u64,
) {
    let dir = env.runs_dir.join(id);
    let dest = format!("runs/{id}");
    if !dir.is_dir() {
        bundle.skip(format!("{dest}/"), "no such run directory");
        return;
    }

    match crate::runstate::read_meta_from(&dir) {
        Ok(meta) => {
            let value = serde_json::to_value(meta.redacted()).unwrap_or_default();
            bundle.json(format!("{dest}/{META_FILE}"), scrubber, value);
            let blueprint = blueprint_dir_of(&meta.agent_path);
            if blueprint.is_dir() {
                copy_text_tree(
                    &blueprint,
                    &format!("{dest}/blueprint"),
                    MAX_TREE_DEPTH,
                    scrubber,
                    bundle,
                );
            } else {
                bundle.skip(
                    format!("{dest}/blueprint/"),
                    format!(
                        "the blueprint directory {} is not on disk",
                        blueprint.display()
                    ),
                );
            }
        }
        // A meta that will not parse is still worth reading as text.
        Err(_) => copy_text(
            &dir.join(META_FILE),
            &format!("{dest}/{META_FILE}"),
            RUN_FILE_CAP,
            scrubber,
            bundle,
        ),
    }

    for name in [STAGES_FILE, FANOUT_FILE, INTERACTIONS_FILE, CONTEXT_FILE] {
        let path = dir.join(name);
        if path.is_file() {
            copy_json(
                &path,
                &format!("{dest}/{name}"),
                RUN_FILE_CAP,
                scrubber,
                bundle,
            );
        }
    }
    let final_output = dir.join(leviath_core::FINAL_OUTPUT_FILE);
    if final_output.is_file() {
        copy_text(
            &final_output,
            &format!("{dest}/{}", leviath_core::FINAL_OUTPUT_FILE),
            RUN_FILE_CAP,
            scrubber,
            bundle,
        );
    }
    copy_stages(&dir, &dest, scrubber, bundle);
    copy_archive(&dir, &dest, scrubber, bundle);
    copy_blobs(&dir, &dest, include_blobs, BLOB_CAP, blob_budget, bundle);
}

/// The directory a run's `agent_path` names. The daemon records the
/// manifest file itself, so a file's parent is the blueprint; a directory is
/// taken as it is.
fn blueprint_dir_of(agent_path: &str) -> PathBuf {
    let path = Path::new(agent_path);
    if path.is_file() {
        path.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        path.to_path_buf()
    }
}

/// `stages/<n>/`: the per-stage logs, context and taint audit.
fn copy_stages(dir: &Path, dest: &str, scrubber: &Scrubber, bundle: &mut Bundle) {
    for stage in sorted_entries(&dir.join("stages")) {
        let index = file_name(&stage);
        for name in ["output.log", "logs.log"] {
            let path = stage.join(name);
            if path.is_file() {
                tail_text(
                    &path,
                    &format!("{dest}/stages/{index}/{name}"),
                    RUN_FILE_CAP,
                    scrubber,
                    bundle,
                );
            }
        }
        for name in [CONTEXT_FILE, "taint_audit.json"] {
            let path = stage.join(name);
            if path.is_file() {
                copy_json(
                    &path,
                    &format!("{dest}/stages/{index}/{name}"),
                    RUN_FILE_CAP,
                    scrubber,
                    bundle,
                );
            }
        }
    }
}

/// `run.lvr`, re-encoded with its secrets out.
fn copy_archive(dir: &Path, dest: &str, scrubber: &Scrubber, bundle: &mut Bundle) {
    let member = format!("{dest}/{ARCHIVE_FILE}");
    let bytes = match read_capped(&dir.join(ARCHIVE_FILE), ARCHIVE_CAP) {
        Ok(bytes) => bytes,
        Err(reason) => {
            bundle.skip(member, reason);
            return;
        }
    };
    match scrubber.scrub_run_archive(&bytes) {
        Ok(scrubbed) => {
            if scrubbed.skipped > 0 {
                bundle.notes.push(format!(
                    "{member}: {} frame(s) this build could not read were left out",
                    scrubbed.skipped
                ));
            }
            bundle.bytes(member, scrubbed.bytes, scrubbed.redactions);
        }
        Err(e) => bundle.skip(member, format!("the journal could not be re-encoded: {e}")),
    }
}

/// `blobs/`: the run's stored parts, each within `per_part`, all within
/// what is left of `budget`.
pub(super) fn copy_blobs(
    dir: &Path,
    dest: &str,
    include: bool,
    per_part: u64,
    budget: &mut u64,
    bundle: &mut Bundle,
) {
    let blobs = dir.join(BLOBS_DIR);
    if !blobs.is_dir() {
        return;
    }
    if !include {
        bundle.skip(format!("{dest}/{BLOBS_DIR}/"), "left out (--no-blobs)");
        return;
    }
    for path in sorted_entries(&blobs) {
        let member = format!("{dest}/{BLOBS_DIR}/{}", file_name(&path));
        let cap = per_part.min(*budget);
        match read_capped(&path, cap) {
            Ok(bytes) => {
                *budget = budget.saturating_sub(bytes.len() as u64);
                bundle.bytes(member, bytes, 0);
            }
            Err(reason) => bundle.skip(member, reason),
        }
    }
}

/// `blueprint/` and `blueprint-check.json` for `--agent`.
fn blueprint_under_test(path: &Path, scrubber: &Scrubber, bundle: &mut Bundle) {
    let manifest = if path.is_file() {
        path.to_path_buf()
    } else {
        path.join(MANIFEST_FILENAME)
    };
    let dir = manifest.parent().map(Path::to_path_buf).unwrap_or_default();
    copy_text_tree(&dir, "blueprint", MAX_TREE_DEPTH, scrubber, bundle);

    let check = match std::fs::read_to_string(&manifest) {
        Ok(content) => match leviath_core::manifest::parse_manifest(&content) {
            Ok(blueprint) => {
                let validation = blueprint.validate();
                serde_json::json!({
                    "path": manifest.display().to_string(),
                    "parses": true,
                    "name": blueprint.name,
                    "version": blueprint.version,
                    "stages": blueprint.stages.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
                    "validates": validation.is_ok(),
                    "validation_error": validation.err().map(|e| e.to_string()),
                })
            }
            Err(e) => serde_json::json!({
                "path": manifest.display().to_string(),
                "parses": false,
                "error": e.to_string(),
            }),
        },
        Err(e) => serde_json::json!({
            "path": manifest.display().to_string(),
            "parses": false,
            "error": format!("cannot read: {e}"),
        }),
    };
    bundle.json("blueprint-check.json", scrubber, check);
}

/// `setup/imports.json`: which other tools' config files exist, by path.
/// Their contents are theirs, and never copied.
fn setup_imports(env: &RageEnv, bundle: &mut Bundle) {
    let sources: Vec<serde_json::Value> =
        crate::commands::setup::import::known_sources(&env.import_roots)
            .into_iter()
            .map(|source| {
                serde_json::json!({
                    "tool": source.display,
                    "path": source.path.display().to_string(),
                    "exists": source.path.is_file(),
                })
            })
            .collect();
    bundle.text(
        "setup/imports.json",
        serde_json::to_string_pretty(&sources).expect("a JSON value serializes"),
        0,
        false,
    );
}

// ─── Readers ────────────────────────────────────────────────────────────────

/// Read a whole file, or say why not: absent, over `cap`, or unreadable.
pub(super) fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(describe_io)?;
    if meta.len() > cap {
        return Err(format!(
            "left out: {} bytes is over the {} byte cap",
            meta.len(),
            cap
        ));
    }
    std::fs::read(path).map_err(describe_io)
}

/// An I/O failure as the manifest words it: a missing file is the common
/// case and reads as such, anything else carries the OS's reason.
pub(super) fn describe_io(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "not present".to_string()
    } else {
        format!("unreadable: {e}")
    }
}

/// A path's final component, lossily. Every path here came from a
/// directory listing, so there always is one.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A TOML file with the structural scrub. Absent is recorded, since every
/// TOML file here is one a reader would look for.
fn copy_toml(path: &Path, dest: &str, scrubber: &Scrubber, bundle: &mut Bundle) {
    match read_capped(path, SMALL_CAP * 4) {
        Ok(bytes) => {
            let (text, redactions) = scrubber.scrub_toml(&String::from_utf8_lossy(&bytes));
            bundle.text(dest, text, redactions, false);
        }
        Err(reason) => bundle.skip(dest, reason),
    }
}

/// A text file with the textual scrub.
fn copy_text(path: &Path, dest: &str, cap: u64, scrubber: &Scrubber, bundle: &mut Bundle) {
    match read_capped(path, cap) {
        Ok(bytes) => {
            let (text, redactions) = scrubber.scrub(&String::from_utf8_lossy(&bytes));
            bundle.text(dest, text, redactions, false);
        }
        Err(reason) => bundle.skip(dest, reason),
    }
}

/// A JSON file, scrubbed as a document when it parses and as text when it
/// does not. Absent is recorded.
fn copy_json(path: &Path, dest: &str, cap: u64, scrubber: &Scrubber, bundle: &mut Bundle) {
    match read_capped(path, cap) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) => bundle.json(dest, scrubber, value),
            Err(_) => {
                let (text, redactions) = scrubber.scrub(&String::from_utf8_lossy(&bytes));
                bundle.text(dest, text, redactions, false);
            }
        },
        Err(reason) => bundle.skip(dest, reason),
    }
}

/// The last `cap` bytes of a text file, marked truncated when that is not
/// all of it. Absent is recorded.
pub(super) fn tail_text(
    path: &Path,
    dest: &str,
    cap: u64,
    scrubber: &Scrubber,
    bundle: &mut Bundle,
) {
    let bytes = match read_capped(path, LOG_READ_CAP) {
        Ok(bytes) => bytes,
        Err(reason) => {
            bundle.skip(dest, reason);
            return;
        }
    };
    let truncated = bytes.len() as u64 > cap;
    let start = bytes.len().saturating_sub(cap as usize);
    let mut text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if truncated {
        text.insert_str(0, "[... earlier lines left out by lev rage ...]\n");
    }
    let (text, redactions) = scrubber.scrub(&text);
    bundle.text(dest, text, redactions, truncated);
}

/// Copy the text files under `dir` to `dest/`, `depth` levels down. A
/// directory that is not there is recorded once; a file that is not text is
/// recorded by name.
pub(super) fn copy_text_tree(
    dir: &Path,
    dest: &str,
    depth: usize,
    scrubber: &Scrubber,
    bundle: &mut Bundle,
) {
    if !dir.is_dir() {
        bundle.skip(format!("{dest}/"), "not present");
        return;
    }
    copy_tree_level(dir, dest, depth, scrubber, bundle);
}

fn copy_tree_level(dir: &Path, dest: &str, depth: usize, scrubber: &Scrubber, bundle: &mut Bundle) {
    for path in sorted_entries(dir) {
        let member = format!("{dest}/{}", file_name(&path));
        if path.is_dir() {
            if depth > 1 {
                copy_tree_level(&path, &member, depth - 1, scrubber, bundle);
            } else {
                bundle.skip(format!("{member}/"), "deeper than the bundle copies");
            }
            continue;
        }
        let is_text = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| TEXT_EXTENSIONS.contains(&e));
        if is_text {
            copy_text(&path, &member, SMALL_CAP, scrubber, bundle);
        } else {
            bundle.skip(member, "not a text file");
        }
    }
}

/// A directory's entries sorted by name, or nothing when it cannot be read.
fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    entries.sort();
    entries
}

// ─── Runs and blueprints, for the pickers ───────────────────────────────────

/// Every run under `runs_dir` whose `meta.json` parses, newest first.
pub(crate) fn list_metas(runs_dir: &Path) -> Vec<RunMeta> {
    let mut runs: Vec<RunMeta> = sorted_entries(runs_dir)
        .iter()
        .filter_map(|dir| crate::runstate::read_meta_from(dir).ok())
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    runs
}

/// `root` and everything it spawned, root first, each only once. The tree is
/// read the way `runstate::descendant_run_ids` reads it: a child's
/// `parent_run_id` and a parent's `children`, unioned, and only ids whose
/// directory is really there.
pub(super) fn family(metas: &[RunMeta], runs_dir: &Path, root: &str) -> Vec<String> {
    use std::collections::{HashMap, HashSet};
    let mut by_parent: HashMap<&str, Vec<&str>> = HashMap::new();
    for meta in metas {
        if let Some(parent) = meta.parent_run_id.as_deref() {
            by_parent.entry(parent).or_default().push(&meta.run_id);
        }
    }
    for meta in metas {
        let known = by_parent.entry(&meta.run_id).or_default();
        for child in &meta.children {
            if !known.contains(&child.as_str()) {
                known.push(child);
            }
        }
    }
    let mut seen: HashSet<&str> = HashSet::from([root]);
    let mut out = vec![root.to_string()];
    let mut frontier = vec![root];
    while let Some(id) = frontier.pop() {
        for child in by_parent.get(id).map(Vec::as_slice).unwrap_or_default() {
            if seen.insert(child) && runs_dir.join(child).is_dir() {
                out.push((*child).to_string());
                frontier.push(child);
            }
        }
    }
    out
}

/// The run `given` names: an exact id, or a prefix only one run starts
/// with. Anything else says what was found.
pub(crate) fn resolve_run_id(metas: &[RunMeta], given: &str) -> Result<String, String> {
    if metas.iter().any(|m| m.run_id == given) {
        return Ok(given.to_string());
    }
    let matches: Vec<&str> = metas
        .iter()
        .filter(|m| m.run_id.starts_with(given))
        .map(|m| m.run_id.as_str())
        .collect();
    match matches.as_slice() {
        [] => Err(format!("no run has the id or prefix `{given}`")),
        [one] => Ok((*one).to_string()),
        many => Err(format!(
            "`{given}` matches {} runs: {}",
            many.len(),
            many.join(", ")
        )),
    }
}

/// The installed blueprints: every directory under `agents_dir` holding a
/// manifest, by name.
pub(crate) fn installed_blueprints(agents_dir: &Path) -> Vec<(String, PathBuf)> {
    sorted_entries(agents_dir)
        .into_iter()
        .filter(|dir| dir.join(MANIFEST_FILENAME).is_file())
        .map(|dir| (file_name(&dir), dir))
        .collect()
}
