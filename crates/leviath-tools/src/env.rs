//! The environment tools: what time it is, what machine this is, what locale
//! the user works in, where the interesting directories are, and whether a
//! given program is installed.
//!
//! These exist because a model has no way to know any of it. Its training data
//! has a cutoff and the run does not, so an agent asked about anything current
//! will otherwise reason from whenever it was trained. Everything here is
//! read-only and side-effect free, which is why the tools default to `allow`
//! and are classified inbound.
//!
//! # How the platform differences are handled
//!
//! Every branch that depends on the operating system is a *pure function taking
//! the platform as an argument*, following
//! [`BuiltinTools::detect_shell_for`](crate::BuiltinTools) and
//! `leviath_sys::editor::default_editors_for`. Nothing here is `#[cfg]`-gated,
//! so the Windows answer is reachable under test from macOS and the Linux
//! answer from Windows. The impure edges - reading the clock, the environment,
//! the filesystem - are thin wrappers that pass what they found into those
//! functions.
//!
//! # Why these are synchronous
//!
//! None of them does I/O worth awaiting. [`BuiltinTools::execute`] is `async`
//! because of the shell and the file tools, so its arms delegate here; but the
//! region-seeding path resolves tool calls outside that lane and would
//! otherwise have to build a runtime to await a function that never yields.

use super::*;
use chrono::{DateTime, Datelike, FixedOffset, Local, Timelike, Utc};

// ── current_time ──────────────────────────────────────────────────────────

/// Render one instant as the object `current_time` returns.
///
/// Takes both renderings of the same moment plus the zone name, rather than
/// reading the clock itself, so every field can be asserted against a fixed
/// instant. A function that called `Utc::now()` internally could only be tested
/// against itself.
///
/// `local` is a [`FixedOffset`] rather than a `Local`: it carries the offset as
/// data, so a test can build any zone without the host being in it.
pub(crate) fn describe_instant(
    utc: DateTime<Utc>,
    local: DateTime<FixedOffset>,
    zone: Option<&str>,
) -> Value {
    let iso = local.iso_week();
    json!({
        "utc": utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "local": local.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        "timezone": zone,
        "utc_offset": local.offset().to_string(),
        "unix": utc.timestamp(),
        "date": local.format("%Y-%m-%d").to_string(),
        "time": local.format("%H:%M:%S").to_string(),
        "weekday": local.format("%A").to_string(),
        "iso_week": format!("{}-W{:02}", iso.year(), iso.week()),
        "day_of_year": local.ordinal(),
        "hour_24": local.hour(),
    })
}

// ── system_info ───────────────────────────────────────────────────────────

/// What a filesystem has room for, as reported to the model.
///
/// A pair rather than two arguments so the "neither is known" case is one
/// `None` rather than two that could disagree.
pub(crate) struct DiskSpace {
    /// Free bytes on the filesystem holding the working directory.
    pub available: Option<u64>,
    /// Total bytes of that filesystem.
    pub total: Option<u64>,
}

/// The facts `system_info` reports, over values the caller looked up.
///
/// `os_version` is `None` on any platform that does not publish its release in
/// a readable file (Windows, today). It is reported as `null` rather than
/// omitted, so the model can tell "this machine did not say" from "the tool
/// forgot to look".
pub(crate) fn describe_system(
    os: &str,
    arch: &str,
    family: &str,
    version: Option<&str>,
    host: Option<&str>,
    cpus: usize,
    disk: &DiskSpace,
) -> Value {
    json!({
        "os": os,
        "os_name": leviath_sys::osinfo::display_name_for(os),
        "os_version": version,
        "os_family": family,
        "arch": arch,
        "cpu_count": cpus,
        "hostname": host,
        "path_separator": path_separator_for(family),
        "line_ending": line_ending_name_for(family),
        "executable_suffix": std::env::consts::EXE_SUFFIX,
        "disk": {
            "available_bytes": disk.available,
            "total_bytes": disk.total,
        },
    })
}

/// The character that separates entries in `PATH` on a platform of this family.
///
/// Keyed on the *family* rather than the OS, because that is the axis the
/// answer actually turns on: every Unix uses `:` and Windows uses `;`.
pub(crate) fn path_separator_for(family: &str) -> &'static str {
    match family {
        "windows" => ";",
        _ => ":",
    }
}

/// How a platform of this family ends a line, named rather than written.
///
/// The name, because the literal would arrive at the model as an actual newline
/// inside a JSON string, which reads as formatting rather than as an answer.
pub(crate) fn line_ending_name_for(family: &str) -> &'static str {
    match family {
        "windows" => "crlf",
        _ => "lf",
    }
}

// ── locale_info ───────────────────────────────────────────────────────────

/// Split a platform locale tag into `(normalized, language, region)`.
///
/// Handles the shapes that actually turn up: `en_US.UTF-8` from a Unix
/// environment variable, `en-US` from Windows and macOS, a bare `en`, and the
/// `C`/`POSIX` sentinels that mean "no locale configured" rather than naming
/// one. The normalized form uses `-`, per BCP-47, whichever separator arrived.
///
/// The charset suffix and any `@modifier` are dropped: they describe encoding
/// and collation, and nothing downstream asks about either.
pub(crate) fn split_locale_tag(tag: &str) -> Option<(String, String, Option<String>)> {
    let head = tag.trim().split(['.', '@']).next().unwrap_or("").trim();
    // `C` and `POSIX` are the absence of a locale spelled as a value. Reporting
    // them as a language would have the model believe the user works in a
    // language called "C".
    if head.is_empty() || head.eq_ignore_ascii_case("c") || head.eq_ignore_ascii_case("posix") {
        return None;
    }
    let mut parts = head.split(['_', '-']).filter(|p| !p.is_empty());
    let language = parts.next()?.to_ascii_lowercase();
    let region = parts.next().map(|r| r.to_ascii_uppercase());
    let normalized = match &region {
        Some(r) => format!("{language}-{r}"),
        None => language.clone(),
    };
    Some((normalized, language, region))
}

/// The object `locale_info` returns for a platform-reported tag.
///
/// An unreadable or unset locale is reported as nulls with the raw tag kept, so
/// the model can see *what* the platform said even when it could not be split.
pub(crate) fn describe_locale(tag: Option<&str>) -> Value {
    let parsed = tag.and_then(split_locale_tag);
    match parsed {
        Some((normalized, language, region)) => json!({
            "locale": normalized,
            "language": language,
            "region": region,
            "raw": tag,
        }),
        None => json!({
            "locale": Value::Null,
            "language": Value::Null,
            "region": Value::Null,
            "raw": tag,
        }),
    }
}

// ── environment_info ──────────────────────────────────────────────────────

/// The directories `environment_info` reports, as the caller resolved them.
///
/// Every field is optional because every one of them can be absent: a daemon
/// with no home directory, a platform with no notion of a config directory.
#[derive(Default)]
pub(crate) struct WellKnownDirs {
    /// The user's home directory.
    pub home: Option<PathBuf>,
    /// The system temporary directory.
    pub temp: Option<PathBuf>,
    /// The per-user configuration directory.
    pub config: Option<PathBuf>,
    /// The per-user data directory.
    pub data: Option<PathBuf>,
}

/// Partition environment variables into those an agent may see and the names it
/// may not.
///
/// The rule is [`leviath_core::script_env_allowed`] - the same one a Rhai
/// `env_var` read answers to - so there is a single answer to "may
/// agent-supplied code see this variable" rather than one per surface.
///
/// Withheld names are *listed*, not dropped. An agent that can see
/// `ANTHROPIC_API_KEY` exists but not its value can reason about the machine's
/// configuration without the secret; silently omitting it would instead have it
/// conclude the variable is unset and suggest setting it.
pub(crate) fn partition_env<'a>(
    vars: impl Iterator<Item = (&'a str, &'a str)>,
    allowlist: &[String],
) -> (serde_json::Map<String, Value>, Vec<String>) {
    let mut visible = serde_json::Map::new();
    let mut withheld = Vec::new();
    for (name, value) in vars {
        match leviath_core::script_env_allowed(name, allowlist) {
            true => {
                visible.insert(name.to_string(), Value::String(value.to_string()));
            }
            false => withheld.push(name.to_string()),
        }
    }
    withheld.sort();
    (visible, withheld)
}

/// The object `environment_info` returns.
///
/// `path_entries` is split here rather than handed over as one string, because
/// the model would otherwise have to know the platform's separator to read it -
/// and the separator is the very thing this tool exists to report.
pub(crate) fn describe_environment(
    workdir: &Path,
    dirs: &WellKnownDirs,
    family: &str,
    path_var: Option<&str>,
    env: (serde_json::Map<String, Value>, Vec<String>),
) -> Value {
    let separator = path_separator_for(family);
    let entries: Vec<&str> = path_var
        .map(|p| p.split(separator).filter(|e| !e.is_empty()).collect())
        .unwrap_or_default();
    let (visible, withheld) = env;
    json!({
        "working_directory": workdir.display().to_string(),
        "home_directory": dirs.home.as_ref().map(|p| p.display().to_string()),
        "temp_directory": dirs.temp.as_ref().map(|p| p.display().to_string()),
        "config_directory": dirs.config.as_ref().map(|p| p.display().to_string()),
        "data_directory": dirs.data.as_ref().map(|p| p.display().to_string()),
        "path_separator": separator,
        "path_entries": entries,
        "environment_variables": visible,
        "withheld_variables": withheld,
    })
}

// ── which_command ─────────────────────────────────────────────────────────

/// The candidate file names for `program` on a platform of this family.
///
/// On Windows a bare `git` is spelled `git.exe` on disk, and which suffixes
/// count is `PATHEXT`'s business - so a lookup that only tried the bare name
/// would report every program missing. A name that already carries one of those
/// suffixes is tried as written, and only as written.
pub(crate) fn candidate_names_for(
    program: &str,
    family: &str,
    pathext: Option<&str>,
) -> Vec<String> {
    if family != "windows" {
        return vec![program.to_string()];
    }
    let exts: Vec<String> = pathext
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .map(|e| e.to_ascii_lowercase())
        .collect();
    let lower = program.to_ascii_lowercase();
    if exts.iter().any(|e| lower.ends_with(e.as_str())) {
        return vec![program.to_string()];
    }
    exts.iter().map(|e| format!("{program}{e}")).collect()
}

/// Join a `PATH` directory to a candidate file name using the separator a
/// platform of this family uses.
///
/// [`Path::join`] would use the *host's* separator, which is wrong twice over:
/// it makes the Windows branch unreachable in any meaningful sense from a Unix
/// test, and it is the reason this function exists rather than being inlined.
/// A directory that already ends in a separator is not given a second one.
pub(crate) fn join_for(dir: &str, name: &str, family: &str) -> PathBuf {
    let separator = match family {
        "windows" => '\\',
        _ => '/',
    };
    match dir.ends_with(['/', '\\']) {
        true => PathBuf::from(format!("{dir}{name}")),
        false => PathBuf::from(format!("{dir}{separator}{name}")),
    }
}

/// Resolve `program` against `path_var`, reporting the first match.
///
/// `exists` is injected as `&dyn Fn` rather than `impl Fn` so there is one
/// monomorphization: a generic parameter here produced a coverage report where
/// every source position had a covered instantiation and the summary still
/// counted misses.
///
/// A program named with a separator in it is a path, not a `PATH` lookup, and is
/// probed where it points. That is what a shell does, and an agent that has just
/// been told `./scripts/build` is missing because `PATH` has no such entry would
/// be told something untrue.
pub(crate) fn resolve_on_path(
    program: &str,
    path_var: Option<&str>,
    pathext: Option<&str>,
    family: &str,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let names = candidate_names_for(program, family, pathext);
    if program.contains('/') || (family == "windows" && program.contains('\\')) {
        return names
            .iter()
            .map(PathBuf::from)
            .find(|candidate| exists(candidate));
    }
    let separator = path_separator_for(family);
    path_var?
        .split(separator)
        .filter(|dir| !dir.is_empty())
        .flat_map(|dir| names.iter().map(move |name| join_for(dir, name, family)))
        .find(|candidate| exists(candidate))
}

/// Whether `path` is a file that exists.
///
/// A named `fn` rather than a closure at the call site, so it is one region
/// covered by the tool's own tests instead of a closure written inside a
/// call the tests reach by only one route.
fn is_existing_file(path: &Path) -> bool {
    path.is_file()
}

// ── dispatch ──────────────────────────────────────────────────────────────

impl BuiltinTools {
    /// Run one of the environment tools, or `None` when `name` is not one.
    ///
    /// Synchronous: see the module docs. The `Option` is what lets a caller ask
    /// "is this one of mine?" and dispatch in the same step, which is what keeps
    /// the shortcut in [`BuiltinTools::execute`] free of an arm nothing reaches.
    pub fn execute_env_tool(&self, name: &str, args: &Value) -> Option<String> {
        match name {
            "current_time" => Some(self.current_time()),
            "system_info" => Some(self.system_info()),
            "locale_info" => Some(Self::locale_info()),
            "environment_info" => Some(self.environment_info()),
            "which_command" => Some(Self::which_command(args)),
            _ => None,
        }
    }

    /// The current instant, in UTC and in the host's local zone.
    fn current_time(&self) -> String {
        let now = Utc::now();
        let local = Local::now().fixed_offset();
        let zone = iana_time_zone::get_timezone().ok();
        pretty(&describe_instant(now, local, zone.as_deref()))
    }

    /// What machine this is.
    fn system_info(&self) -> String {
        let disk = DiskSpace {
            available: leviath_sys::disk::available_bytes(&self.ctx.workdir),
            total: leviath_sys::disk::total_bytes(&self.ctx.workdir),
        };
        let version = leviath_sys::osinfo::current_version();
        let host = leviath_sys::osinfo::hostname();
        // `available_parallelism` fails only where the platform will not report
        // it, and one is the honest answer there rather than a refusal: the
        // question is "how much can I do at once", and the answer is "one".
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        pretty(&describe_system(
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::consts::FAMILY,
            version.as_deref(),
            host.as_deref(),
            cpus,
            &disk,
        ))
    }

    /// The user's language and region.
    fn locale_info() -> String {
        let tag = leviath_sys::locale::current_tag();
        pretty(&describe_locale(tag.as_deref()))
    }

    /// Where the interesting directories are, and what is in the environment.
    fn environment_info(&self) -> String {
        let dirs = WellKnownDirs {
            home: leviath_core::home_dir(),
            temp: Some(std::env::temp_dir()),
            config: dirs::config_dir(),
            data: dirs::data_dir(),
        };
        let vars: Vec<(String, String)> = std::env::vars().collect();
        let partitioned = partition_env(
            vars.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            &self.ctx.shell_env.allow_env_vars,
        );
        let path_var = std::env::var("PATH").ok();
        pretty(&describe_environment(
            &self.ctx.workdir,
            &dirs,
            std::env::consts::FAMILY,
            path_var.as_deref(),
            partitioned,
        ))
    }

    /// Whether a program is installed, and where.
    fn which_command(args: &Value) -> String {
        let Some(program) = args.get("command").and_then(|v| v.as_str()) else {
            return "[error] which_command requires a 'command' string".to_string();
        };
        if program.trim().is_empty() {
            return "[error] which_command requires a non-empty 'command'".to_string();
        }
        let path_var = std::env::var("PATH").ok();
        let pathext = std::env::var("PATHEXT").ok();
        let found = resolve_on_path(
            program,
            path_var.as_deref(),
            pathext.as_deref(),
            std::env::consts::FAMILY,
            &is_existing_file,
        );
        pretty(&json!({
            "command": program,
            "found": found.is_some(),
            "path": found.map(|p| p.display().to_string()),
        }))
    }
}

/// Render a value the way these tools hand it back.
///
/// Indented rather than compact: the same text is what a seeded context region
/// holds, and that region is read by people in the dashboard as well as by the
/// model. The objects are a dozen fields each, so the whitespace costs little.
fn pretty(value: &Value) -> String {
    // Serializing a `Value` fails only for a map with non-string keys or a
    // non-finite float, and every value here is built by `json!` from strings,
    // integers, booleans and nulls. A fallback rendering would be a second
    // formatting decision that nothing could ever reach or test.
    serde_json::to_string_pretty(value).expect("a Value of primitives always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(dir: &Path) -> BuiltinTools {
        BuiltinTools::new(ToolContext::new(dir.to_path_buf()))
    }

    /// A fixed instant, so every field is asserted against a known answer
    /// rather than against whatever the clock said. 2026-08-18 is a Tuesday,
    /// day 230 of the year, in ISO week 34.
    fn fixed() -> (DateTime<Utc>, DateTime<FixedOffset>) {
        let utc = DateTime::parse_from_rfc3339("2026-08-18T19:32:07Z")
            .expect("a literal RFC3339 instant parses")
            .with_timezone(&Utc);
        let local = DateTime::parse_from_rfc3339("2026-08-18T14:32:07-05:00")
            .expect("a literal RFC3339 instant parses");
        (utc, local)
    }

    #[test]
    fn an_instant_is_described_in_both_zones_with_its_calendar_position() {
        let (utc, local) = fixed();
        let v = describe_instant(utc, local, Some("America/Chicago"));
        assert_eq!(v["utc"], "2026-08-18T19:32:07Z");
        assert_eq!(v["local"], "2026-08-18T14:32:07-05:00");
        assert_eq!(v["timezone"], "America/Chicago");
        assert_eq!(v["utc_offset"], "-05:00");
        assert_eq!(v["unix"], 1787081527_i64);
        assert_eq!(v["date"], "2026-08-18");
        assert_eq!(v["time"], "14:32:07");
        assert_eq!(v["weekday"], "Tuesday");
        assert_eq!(v["iso_week"], "2026-W34");
        assert_eq!(v["day_of_year"], 230);
        assert_eq!(v["hour_24"], 14);
    }

    /// The calendar fields follow the *local* rendering, not UTC. Here the two
    /// fall on different days, which is the case that catches reading the wrong
    /// one: it is already the 19th in UTC while the user is still on the 18th.
    #[test]
    fn calendar_fields_follow_the_local_day_not_the_utc_one() {
        let utc = DateTime::parse_from_rfc3339("2026-08-19T02:30:00Z")
            .expect("literal parses")
            .with_timezone(&Utc);
        let local = DateTime::parse_from_rfc3339("2026-08-18T21:30:00-05:00").expect("literal");
        let v = describe_instant(utc, local, None);
        assert_eq!(v["date"], "2026-08-18");
        assert_eq!(v["weekday"], "Tuesday");
        assert_eq!(v["utc"], "2026-08-19T02:30:00Z");
        // An unresolvable zone is reported as null rather than guessed at.
        assert_eq!(v["timezone"], Value::Null);
    }

    /// The real clock. What can be asserted without re-implementing the tool is
    /// that it answers with parseable JSON whose instants agree with each other.
    #[test]
    fn current_time_reports_the_real_clock_as_parseable_json() {
        let dir = tempfile::tempdir().unwrap();
        let out = tools(dir.path()).execute_env_tool("current_time", &json!({}));
        let v: Value = serde_json::from_str(&out.expect("current_time is an env tool"))
            .expect("the tool returns JSON");
        let utc = v["utc"].as_str().expect("utc is a string");
        let local = v["local"].as_str().expect("local is a string");
        // Same moment, two renderings: parsed back to UTC they must agree.
        let a = DateTime::parse_from_rfc3339(utc).expect("utc round-trips");
        let b = DateTime::parse_from_rfc3339(local).expect("local round-trips");
        assert_eq!(a.timestamp(), b.timestamp());
        assert_eq!(v["unix"].as_i64(), Some(a.timestamp()));
    }

    #[test]
    fn system_facts_report_what_the_caller_found() {
        let disk = DiskSpace {
            available: Some(1024),
            total: Some(4096),
        };
        let v = describe_system(
            "linux",
            "aarch64",
            "unix",
            Some("Ubuntu 24.04"),
            Some("builder"),
            8,
            &disk,
        );
        assert_eq!(v["os"], "linux");
        assert_eq!(v["os_name"], "Linux");
        assert_eq!(v["os_version"], "Ubuntu 24.04");
        assert_eq!(v["os_family"], "unix");
        assert_eq!(v["arch"], "aarch64");
        assert_eq!(v["cpu_count"], 8);
        assert_eq!(v["hostname"], "builder");
        assert_eq!(v["path_separator"], ":");
        assert_eq!(v["line_ending"], "lf");
        assert_eq!(v["disk"]["available_bytes"], 1024);
        assert_eq!(v["disk"]["total_bytes"], 4096);
    }

    /// A platform that publishes no version and no hostname reports nulls, not
    /// omitted keys: "this machine did not say" has to be distinguishable from
    /// "the tool did not look".
    #[test]
    fn unknown_system_facts_are_null_rather_than_absent() {
        let disk = DiskSpace {
            available: None,
            total: None,
        };
        let v = describe_system("windows", "x86_64", "windows", None, None, 1, &disk);
        assert_eq!(v["os_name"], "Windows");
        assert!(v.get("os_version").is_some());
        assert_eq!(v["os_version"], Value::Null);
        assert_eq!(v["hostname"], Value::Null);
        assert_eq!(v["disk"]["available_bytes"], Value::Null);
        // The Windows answers for both family-keyed fields, from any host.
        assert_eq!(v["path_separator"], ";");
        assert_eq!(v["line_ending"], "crlf");
    }

    #[test]
    fn family_keyed_answers_cover_both_platforms_from_either() {
        assert_eq!(path_separator_for("windows"), ";");
        assert_eq!(path_separator_for("unix"), ":");
        assert_eq!(line_ending_name_for("windows"), "crlf");
        assert_eq!(line_ending_name_for("unix"), "lf");
    }

    #[test]
    fn system_info_answers_for_the_real_host() {
        let dir = tempfile::tempdir().unwrap();
        let out = tools(dir.path()).execute_env_tool("system_info", &json!({}));
        let v: Value = serde_json::from_str(&out.expect("system_info is an env tool")).unwrap();
        assert_eq!(v["os"], std::env::consts::OS);
        assert_eq!(v["arch"], std::env::consts::ARCH);
        // Whatever the host reports, it must be able to do at least one thing
        // at a time - a zero here would mean the fallback was mis-wired.
        assert!(v["cpu_count"].as_u64().unwrap_or(0) >= 1);
    }

    #[test]
    fn locale_tags_split_on_both_separators_and_drop_encoding() {
        assert_eq!(
            split_locale_tag("en_US.UTF-8"),
            Some(("en-US".into(), "en".into(), Some("US".into())))
        );
        assert_eq!(
            split_locale_tag("en-US"),
            Some(("en-US".into(), "en".into(), Some("US".into())))
        );
        // Case is normalized in both directions: language down, region up.
        assert_eq!(
            split_locale_tag("PT_br"),
            Some(("pt-BR".into(), "pt".into(), Some("BR".into())))
        );
        // A modifier describes collation, not language.
        assert_eq!(
            split_locale_tag("de_DE@euro"),
            Some(("de-DE".into(), "de".into(), Some("DE".into())))
        );
        // A bare language has no region rather than an invented one.
        assert_eq!(
            split_locale_tag("fr"),
            Some(("fr".into(), "fr".into(), None))
        );
    }

    /// `C` and `POSIX` are how a platform spells "no locale set". Reporting
    /// either as a language would have the model believe the user works in a
    /// language called C.
    #[test]
    fn the_absent_locale_sentinels_are_not_languages() {
        assert_eq!(split_locale_tag("C"), None);
        assert_eq!(split_locale_tag("c"), None);
        assert_eq!(split_locale_tag("POSIX"), None);
        assert_eq!(split_locale_tag("C.UTF-8"), None);
        assert_eq!(split_locale_tag(""), None);
        assert_eq!(split_locale_tag("   "), None);
        // A tag that is only separators names no language.
        assert_eq!(split_locale_tag("_"), None);
        assert_eq!(split_locale_tag(".UTF-8"), None);
    }

    #[test]
    fn a_locale_that_cannot_be_split_keeps_its_raw_tag() {
        let v = describe_locale(Some("C"));
        assert_eq!(v["locale"], Value::Null);
        assert_eq!(v["language"], Value::Null);
        assert_eq!(v["region"], Value::Null);
        assert_eq!(v["raw"], "C");

        // And an unset locale reports nulls throughout, raw included.
        let none = describe_locale(None);
        assert_eq!(none["locale"], Value::Null);
        assert_eq!(none["raw"], Value::Null);

        let ok = describe_locale(Some("en_GB.UTF-8"));
        assert_eq!(ok["locale"], "en-GB");
        assert_eq!(ok["language"], "en");
        assert_eq!(ok["region"], "GB");
        assert_eq!(ok["raw"], "en_GB.UTF-8");
        // A language with no region reports the region as null.
        assert_eq!(describe_locale(Some("ja"))["region"], Value::Null);
    }

    #[test]
    fn locale_info_answers_for_the_real_host() {
        let dir = tempfile::tempdir().unwrap();
        let out = tools(dir.path()).execute_env_tool("locale_info", &json!({}));
        let v: Value = serde_json::from_str(&out.expect("locale_info is an env tool")).unwrap();
        // The host may have no locale at all; what must hold is that the object
        // always carries every key, so the model never has to test for absence.
        for key in ["locale", "language", "region", "raw"] {
            assert!(v.get(key).is_some(), "{key} must always be present");
        }
    }

    /// The credential-name rule is `leviath_core`'s, not a second copy: a
    /// key-shaped name is withheld and listed, an ordinary one passes, and the
    /// user's allowlist releases the specific name they named.
    #[test]
    fn environment_variables_are_partitioned_by_the_shared_credential_rule() {
        let vars = [
            ("LANG", "en_US.UTF-8"),
            ("ANTHROPIC_API_KEY", "sk-ant-secret"),
            ("EDITOR", "vim"),
        ];
        let (visible, withheld) = partition_env(vars.iter().copied(), &[]);
        assert_eq!(visible["LANG"], "en_US.UTF-8");
        assert_eq!(visible["EDITOR"], "vim");
        assert!(!visible.contains_key("ANTHROPIC_API_KEY"));
        // Named, so the agent knows it exists without seeing it.
        assert_eq!(withheld, vec!["ANTHROPIC_API_KEY".to_string()]);
        // The value really is nowhere in the output.
        assert!(!serde_json::to_string(&visible).unwrap().contains("sk-ant"));

        // The user's allowlist is what releases it.
        let (visible, withheld) =
            partition_env(vars.iter().copied(), &["anthropic_api_key".to_string()]);
        assert_eq!(visible["ANTHROPIC_API_KEY"], "sk-ant-secret");
        assert!(withheld.is_empty());
    }

    /// Withheld names are sorted, so two runs on the same machine produce the
    /// same region text - an unstable order would look like a change to every
    /// digest and cache comparison downstream.
    #[test]
    fn withheld_names_come_back_in_a_stable_order() {
        let vars = [("ZZ_TOKEN", "a"), ("AA_SECRET", "b"), ("MM_PASSWORD", "c")];
        let (_, withheld) = partition_env(vars.iter().copied(), &[]);
        assert_eq!(withheld, vec!["AA_SECRET", "MM_PASSWORD", "ZZ_TOKEN"]);
    }

    #[test]
    fn the_path_is_split_with_the_platforms_own_separator() {
        let dirs = WellKnownDirs {
            home: Some(PathBuf::from("/home/u")),
            temp: Some(PathBuf::from("/tmp")),
            config: None,
            data: None,
        };
        let v = describe_environment(
            Path::new("/work"),
            &dirs,
            "unix",
            Some("/usr/bin:/bin::/opt/x"),
            (serde_json::Map::new(), Vec::new()),
        );
        assert_eq!(v["working_directory"], "/work");
        assert_eq!(v["home_directory"], "/home/u");
        assert_eq!(v["config_directory"], Value::Null);
        // Empty entries are dropped rather than reported as a directory named "".
        assert_eq!(v["path_entries"], json!(["/usr/bin", "/bin", "/opt/x"]));

        // The Windows split, from any host.
        let w = describe_environment(
            Path::new("C:\\work"),
            &WellKnownDirs::default(),
            "windows",
            Some("C:\\bin;C:\\tools"),
            (serde_json::Map::new(), Vec::new()),
        );
        assert_eq!(w["path_separator"], ";");
        assert_eq!(w["path_entries"], json!(["C:\\bin", "C:\\tools"]));
        // Every directory absent is four nulls, not four missing keys.
        assert_eq!(w["home_directory"], Value::Null);
        assert_eq!(w["temp_directory"], Value::Null);
        assert_eq!(w["data_directory"], Value::Null);
        // And an unset PATH is an empty list rather than a missing key.
        let n = describe_environment(
            Path::new("/w"),
            &WellKnownDirs::default(),
            "unix",
            None,
            (serde_json::Map::new(), Vec::new()),
        );
        assert_eq!(n["path_entries"], json!([]));
    }

    #[test]
    fn environment_info_answers_for_the_real_host() {
        let dir = tempfile::tempdir().unwrap();
        let out = tools(dir.path()).execute_env_tool("environment_info", &json!({}));
        let v: Value =
            serde_json::from_str(&out.expect("environment_info is an env tool")).unwrap();
        // The workdir it reports is the fence the file tools resolve against,
        // canonicalized - not the path as handed in.
        assert_eq!(
            v["working_directory"],
            std::fs::canonicalize(dir.path())
                .unwrap()
                .display()
                .to_string()
        );
        assert!(v["environment_variables"].is_object());
        assert!(v["withheld_variables"].is_array());
    }

    #[test]
    fn windows_candidates_come_from_pathext_and_a_suffixed_name_is_left_alone() {
        assert_eq!(
            candidate_names_for("git", "windows", Some(".COM;.EXE;.BAT")),
            vec!["git.com", "git.exe", "git.bat"]
        );
        // Already suffixed: tried as written, and only as written.
        assert_eq!(
            candidate_names_for("git.exe", "windows", Some(".COM;.EXE")),
            vec!["git.exe"]
        );
        // No PATHEXT set: the documented Windows default still applies.
        assert!(candidate_names_for("git", "windows", None).contains(&"git.exe".to_string()));
        // Blank entries in PATHEXT contribute no candidate.
        assert_eq!(
            candidate_names_for("g", "windows", Some(".EXE;;  ;.BAT")),
            vec!["g.exe", "g.bat"]
        );
        // Unix takes the name as given, whatever PATHEXT says.
        assert_eq!(
            candidate_names_for("git", "unix", Some(".EXE")),
            vec!["git"]
        );
    }

    /// The join uses the *named* platform's separator, not the host's, which
    /// is the whole reason it exists: `Path::join` on a Unix test host would
    /// build `C:\\tools/git.exe` and quietly never match.
    #[test]
    fn path_entries_are_joined_with_the_named_platforms_separator() {
        assert_eq!(
            join_for("/usr/bin", "git", "unix"),
            PathBuf::from("/usr/bin/git")
        );
        assert_eq!(
            join_for("C:\\tools", "git.exe", "windows"),
            PathBuf::from("C:\\tools\\git.exe")
        );
        // A directory that already ends in a separator does not get a second.
        assert_eq!(
            join_for("/usr/bin/", "git", "unix"),
            PathBuf::from("/usr/bin/git")
        );
        assert_eq!(
            join_for("C:\\", "git.exe", "windows"),
            PathBuf::from("C:\\git.exe")
        );
    }

    /// Both platforms' lookups, driven from whichever host runs this: the
    /// probe is injected, so no file has to exist for either to be exercised.
    #[test]
    fn a_program_is_resolved_against_each_platforms_path() {
        let unix_hit = |p: &Path| p == Path::new("/usr/bin/git");
        assert_eq!(
            resolve_on_path("git", Some("/bin:/usr/bin"), None, "unix", &unix_hit),
            Some(PathBuf::from("/usr/bin/git"))
        );
        // First match wins, in PATH order.
        let all = |_: &Path| true;
        assert_eq!(
            resolve_on_path("git", Some("/bin:/usr/bin"), None, "unix", &all),
            Some(PathBuf::from("/bin/git"))
        );
        // Windows finds the suffixed name behind a bare request.
        let win_hit = |p: &Path| p == Path::new("C:\\tools\\git.exe");
        assert_eq!(
            resolve_on_path(
                "git",
                Some("C:\\bin;C:\\tools"),
                Some(".EXE"),
                "windows",
                &win_hit
            ),
            Some(PathBuf::from("C:\\tools\\git.exe"))
        );
    }

    #[test]
    fn a_lookup_with_nothing_to_search_or_nothing_to_find_is_none() {
        let none = |_: &Path| false;
        assert_eq!(
            resolve_on_path("git", Some("/bin"), None, "unix", &none),
            None
        );
        // No PATH at all: nothing to search, and no panic reaching for it.
        let all = |_: &Path| true;
        assert_eq!(resolve_on_path("git", None, None, "unix", &all), None);
        // Empty PATH entries are skipped rather than probed as the empty dir.
        assert_eq!(resolve_on_path("git", Some("::"), None, "unix", &all), None);
    }

    /// A name with a separator is a path. Resolving it against `PATH` would
    /// report `./scripts/build` missing because no `PATH` entry contains a
    /// directory called `scripts`, which is both wrong and confusing.
    #[test]
    fn a_pathlike_name_is_probed_where_it_points() {
        let hit = |p: &Path| p == Path::new("./scripts/build");
        assert_eq!(
            resolve_on_path("./scripts/build", Some("/bin"), None, "unix", &hit),
            Some(PathBuf::from("./scripts/build"))
        );
        let none = |_: &Path| false;
        assert_eq!(
            resolve_on_path("./scripts/build", Some("/bin"), None, "unix", &none),
            None
        );
        // Windows separators count on Windows.
        let win = |p: &Path| p == Path::new("C:\\t\\b.exe");
        assert_eq!(
            resolve_on_path("C:\\t\\b", None, Some(".EXE"), "windows", &win),
            Some(PathBuf::from("C:\\t\\b.exe"))
        );
    }

    #[test]
    fn which_command_finds_a_program_that_is_really_installed() {
        let dir = tempfile::tempdir().unwrap();
        let t = tools(dir.path());
        // The test binary itself, rather than a program named per platform:
        // every host has exactly one of these and its path is absolute, so the
        // real `is_existing_file` probe runs on a file that certainly exists
        // without a `cfg!` arm that is dead on whichever host is running.
        let exe = std::env::current_exe().expect("the test binary has a path");
        let probe = exe.display().to_string();
        let out = t
            .execute_env_tool("which_command", &json!({ "command": probe }))
            .expect("which_command is an env tool");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["command"], probe);
        assert_eq!(v["found"], true);
        assert_eq!(v["path"], probe);

        // A name nothing could plausibly install reports not-found with a null
        // path, rather than an error.
        let missing = t
            .execute_env_tool(
                "which_command",
                &json!({ "command": "leviath-no-such-program-xyzzy" }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&missing).unwrap();
        assert_eq!(v["found"], false);
        assert_eq!(v["path"], Value::Null);
        // The real probe really does answer false for a path nothing created.
        assert!(!is_existing_file(Path::new("/definitely/not/here")));
    }

    #[test]
    fn which_command_refuses_a_call_with_no_usable_program_name() {
        let dir = tempfile::tempdir().unwrap();
        let t = tools(dir.path());
        let missing = t.execute_env_tool("which_command", &json!({})).unwrap();
        assert!(missing.starts_with("[error]"), "{missing}");
        assert!(missing.contains("'command'"));
        // A non-string argument is the same refusal, not a panic.
        let wrong = t
            .execute_env_tool("which_command", &json!({ "command": 7 }))
            .unwrap();
        assert!(wrong.starts_with("[error]"));
        // Present but blank is refused too: it would otherwise probe every
        // PATH directory for a file with no name.
        let blank = t
            .execute_env_tool("which_command", &json!({ "command": "  " }))
            .unwrap();
        assert!(blank.starts_with("[error]"), "{blank}");
        assert!(blank.contains("non-empty"));
    }

    /// The tools this module owns, as the dispatch answers for them. Kept as a
    /// list so the advertising side can be checked against it: a tool defined
    /// in `defs.rs` with no arm here would be advertised and then reported
    /// unknown, which is the failure mode this pairing exists to catch.
    pub(crate) const NATIVE_ENV_TOOLS: &[&str] = &[
        "current_time",
        "system_info",
        "locale_info",
        "environment_info",
        "which_command",
    ];

    #[test]
    fn every_environment_tool_dispatches_and_nothing_else_does() {
        let dir = tempfile::tempdir().unwrap();
        let t = tools(dir.path());
        for name in NATIVE_ENV_TOOLS {
            // `is_some_and` rather than an unwrap with a panicking closure:
            // that closure is a region only a failing run would ever enter, so
            // it reads as uncovered on every passing one.
            assert!(
                t.execute_env_tool(name, &json!({ "command": "sh" }))
                    .is_some_and(|out| serde_json::from_str::<Value>(&out).is_ok()),
                "{name} must dispatch and answer with JSON"
            );
        }
        // `runtime_info` is advertised by this crate but answered by the
        // runtime, so it must NOT dispatch here - a native arm for it would
        // report a run's stage as whatever this process happened to know.
        assert!(t.execute_env_tool("runtime_info", &json!({})).is_none());
        assert!(t.execute_env_tool("read_file", &json!({})).is_none());
    }

    /// The env tools are reachable through the ordinary `execute` entry point,
    /// not only through `execute_env_tool`. That shortcut sits ahead of the
    /// dispatch match, so this is what proves the match is not shadowing it.
    #[tokio::test]
    async fn the_ordinary_dispatch_entry_point_routes_environment_tools() {
        let dir = tempfile::tempdir().unwrap();
        let t = tools(dir.path());
        let out = t.execute("current_time", json!({})).await;
        let v: Value = serde_json::from_str(&out).expect("execute returns the tool's JSON");
        assert!(v["utc"].as_str().is_some());

        // `runtime_info` is advertised by this crate but answered by the
        // runtime, so reaching it here is a refusal naming where it belongs -
        // never a native answer with invented stage or iteration numbers.
        let refused = t.execute("runtime_info", json!({})).await;
        assert_eq!(
            refused,
            "[error] runtime_info must be handled by the runtime"
        );
    }

    /// Every environment tool renders as indented JSON, because the same text
    /// is what a seeded region shows a person in the dashboard.
    #[test]
    fn results_are_rendered_as_indented_json() {
        assert_eq!(pretty(&json!({"a": 1})), "{\n  \"a\": 1\n}");
    }
}
