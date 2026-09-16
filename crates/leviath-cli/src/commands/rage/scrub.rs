//! Taking the secrets out of what `lev rage` packs.
//!
//! Two layers, because each one misses what the other catches. The
//! **structural** scrub knows where a config file keeps its keys: any value
//! under a secret-shaped key, and every value in a header or env map, is
//! replaced before the file is copied. The **textual** scrub then runs over
//! every text member, including the ones the structural pass produced and the
//! ones it never saw (a tool result inside a run, a drop-in script): every
//! secret *value* the loaded config and the environment hold is searched for
//! literally, and anything shaped like a token (`sk-…`, `AKIA…`, a bearer
//! header, a private-key block) is replaced whether or not it is known.
//!
//! What stays: the run's task, the model's replies, tool output, file
//! contents, paths. That is the bundle's whole point, and the docs say so in
//! red before anyone uploads one.

use std::io::{self, Write};

use regex::Regex;

use leviath_core::secrets::{is_secret_header, is_sensitive_env_name};

/// What a known secret is replaced with.
pub(crate) const REDACTED: &str = "[REDACTED]";

/// Below this many characters a value is not searched for. A four-letter
/// "secret" would erase every ordinary word that happens to match it.
const MIN_KNOWN_LEN: usize = 8;

/// Keys whose value is a mode or a name, not a credential, even though the
/// key's spelling says otherwise.
const NOT_A_SECRET_KEY: &[&str] = &["credential_store", "auth_kind", "keychain_providers"];

/// One token-shaped pattern and the label its replacement carries.
struct Pattern {
    kind: &'static str,
    regex: Regex,
    /// A capture group to keep in front of the replacement (`Bearer `, the
    /// `NAME=` of an assignment); `0` replaces the whole match.
    keep: usize,
}

/// The scrubber: the values it knows, and the shapes it recognises.
pub(crate) struct Scrubber {
    /// Longest first, so a value that contains another is replaced whole.
    known: Vec<String>,
    patterns: Vec<Pattern>,
}

impl Scrubber {
    /// A scrubber that knows `known` (short and duplicate values dropped)
    /// and every built-in pattern.
    pub(crate) fn new(known: impl IntoIterator<Item = String>) -> Self {
        let mut known: Vec<String> = known
            .into_iter()
            .filter(|value| value.chars().count() >= MIN_KNOWN_LEN)
            .collect();
        known.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        known.dedup();
        Self {
            known,
            patterns: patterns(),
        }
    }

    /// Replace every known value and every token-shaped run in `text`.
    /// Returns the scrubbed text and how many replacements were made.
    pub(crate) fn scrub(&self, text: &str) -> (String, usize) {
        let mut out = text.to_string();
        let mut count = 0;
        for value in &self.known {
            let hits = out.matches(value.as_str()).count();
            if hits > 0 {
                out = out.replace(value.as_str(), REDACTED);
                count += hits;
            }
        }
        for pattern in &self.patterns {
            let label = format!("[REDACTED:{}]", pattern.kind);
            let keep = pattern.keep;
            let mut hits = 0;
            let replaced = pattern.regex.replace_all(&out, |caps: &regex::Captures| {
                hits += 1;
                match keep {
                    0 => label.clone(),
                    group => format!("{}{label}", &caps[group]),
                }
            });
            if hits > 0 {
                out = replaced.into_owned();
                count += hits;
            }
        }
        let (out, assignments) = scrub_env_assignments(&out);
        (out, count + assignments)
    }

    /// Scrub every string in a JSON value, and blank any `callback_secret`
    /// (a run's webhook signing key, which `RunMeta::redacted` drops the same
    /// way). Returns how many replacements were made.
    pub(crate) fn scrub_json(&self, value: &mut serde_json::Value) -> usize {
        match value {
            serde_json::Value::String(text) => {
                let (scrubbed, count) = self.scrub(text);
                if count > 0 {
                    *text = scrubbed;
                }
                count
            }
            serde_json::Value::Array(items) => items.iter_mut().map(|v| self.scrub_json(v)).sum(),
            serde_json::Value::Object(map) => {
                let mut count = 0;
                for (key, item) in map.iter_mut() {
                    if key == "callback_secret" && !item.is_null() {
                        *item = serde_json::Value::Null;
                        count += 1;
                        continue;
                    }
                    // A string under a secret-shaped key goes whole, once it
                    // is long enough to be one: a region named `key` with
                    // the value `task` is a label, not a credential.
                    if secret_key(key)
                        && item
                            .as_str()
                            .is_some_and(|text| text.chars().count() >= MIN_KNOWN_LEN)
                    {
                        *item = serde_json::Value::String(REDACTED.to_string());
                        count += 1;
                        continue;
                    }
                    count += self.scrub_json(item);
                }
                count
            }
            _ => 0,
        }
    }

    /// Scrub a `config.toml`: the structural pass over its keys, then the
    /// textual pass over the result. Comments and layout survive. A file that
    /// does not parse as TOML gets the textual pass alone, which is also what
    /// a reader of the bundle needs to see: the broken file as it is.
    pub(crate) fn scrub_toml(&self, text: &str) -> (String, usize) {
        let (structural, count) = match text.parse::<toml_edit::DocumentMut>() {
            Ok(mut doc) => {
                let count = scrub_table(doc.as_table_mut(), false);
                (doc.to_string(), count)
            }
            Err(_) => (text.to_string(), 0),
        };
        let (out, textual) = self.scrub(&structural);
        (out, count + textual)
    }

    /// Re-encode a `run.lvr` with its secrets out: every record is scrubbed
    /// as JSON and written back frame by frame, so the copy reads with the
    /// same tools as the original. A frame this build cannot parse is
    /// dropped rather than copied blind, and counted in the returned skips.
    pub(crate) fn scrub_run_archive(&self, bytes: &[u8]) -> io::Result<ScrubbedArchive> {
        use leviath_core::run_archive::{
            Frame, read_archive_start, read_frame, write_archive_start,
        };

        let mut reader: &[u8] = bytes;
        let version = read_archive_start(&mut reader)?;
        let mut out = Vec::with_capacity(bytes.len());
        write_archive_start(&mut out, version).expect("a Vec accepts writes");
        let mut redactions = 0;
        let mut skipped = 0;
        while let Some(frame) = read_frame(&mut reader)? {
            match frame {
                Frame::Record(record) => {
                    // A record that came off the wire serializes: it is the
                    // codec's own model, with nothing map-keyed to refuse.
                    let mut value =
                        serde_json::to_value(&*record).expect("a run record serializes");
                    redactions += self.scrub_json(&mut value);
                    let payload = value.to_string();
                    out.write_all(&(payload.len() as u64).to_be_bytes())
                        .expect("a Vec accepts writes");
                    out.write_all(payload.as_bytes())
                        .expect("a Vec accepts writes");
                }
                Frame::Unreadable { .. } => skipped += 1,
            }
        }
        Ok(ScrubbedArchive {
            bytes: out,
            redactions,
            skipped,
        })
    }
}

/// A re-encoded run archive and what happened on the way.
pub(crate) struct ScrubbedArchive {
    pub bytes: Vec<u8>,
    pub redactions: usize,
    /// Frames this build could not parse, and so did not copy.
    pub skipped: usize,
}

/// Whether a key names a credential: the header rule, minus the few keys
/// that only sound like one.
fn secret_key(key: &str) -> bool {
    !NOT_A_SECRET_KEY.contains(&key) && is_secret_header(key)
}

/// Whether a table holds values that are credentials as often as not: a
/// header map, an MCP server's `env`, a script provider's forwarded extras.
fn secret_table(key: &str) -> bool {
    key == "env" || key == "extra" || key.contains("header")
}

/// Walk a TOML table, redacting string values under secret-shaped keys, and
/// every string inside a secret-shaped table. `inside_secret` says the
/// caller is already such a table.
fn scrub_table(table: &mut toml_edit::Table, inside_secret: bool) -> usize {
    let mut count = 0;
    for (key, item) in table.iter_mut() {
        count += scrub_item(&key, item, inside_secret);
    }
    count
}

/// One entry of a table or an inline table.
pub(super) fn scrub_item(key: &str, item: &mut toml_edit::Item, inside_secret: bool) -> usize {
    let secret_here = inside_secret || secret_table(key);
    match item {
        toml_edit::Item::Value(value) => scrub_value(key, value, secret_here),
        toml_edit::Item::Table(table) => scrub_table(table, secret_here),
        toml_edit::Item::ArrayOfTables(tables) => tables
            .iter_mut()
            .map(|table| scrub_table(table, secret_here))
            .sum(),
        toml_edit::Item::None => 0,
    }
}

/// A value: a string is redacted when its key or its table says so; an
/// inline table and an array recurse.
pub(super) fn scrub_value(key: &str, value: &mut toml_edit::Value, inside_secret: bool) -> usize {
    match value {
        toml_edit::Value::String(_) => {
            if inside_secret || secret_key(key) {
                *value = toml_edit::Value::from("<redacted>");
                1
            } else {
                0
            }
        }
        toml_edit::Value::InlineTable(table) => {
            let secret_here = inside_secret || secret_table(key);
            let mut count = 0;
            for (inner_key, inner) in table.iter_mut() {
                count += scrub_value(&inner_key, inner, secret_here);
            }
            count
        }
        toml_edit::Value::Array(items) => items
            .iter_mut()
            .map(|item| scrub_value(key, item, inside_secret))
            .sum(),
        _ => 0,
    }
}

/// The token shapes the textual pass recognises, whatever they are called.
fn patterns() -> Vec<Pattern> {
    let make = |kind, pattern: &str, keep| Pattern {
        kind,
        // Every pattern is a literal in this file, checked by the tests below.
        regex: Regex::new(pattern).expect("a pattern in this file compiles"),
        keep,
    };
    vec![
        // OpenAI, Anthropic, OpenRouter and friends all start their keys the
        // same way, which makes the shape worth more than any list of names.
        make("api-key", r"sk-[A-Za-z0-9_-]{16,}", 0),
        make("aws-key", r"AKIA[0-9A-Z]{16}", 0),
        make(
            "github-token",
            r"(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})",
            0,
        ),
        make("slack-token", r"xox[abposr]-[A-Za-z0-9-]{10,}", 0),
        make("google-key", r"AIza[0-9A-Za-z_-]{35}", 0),
        make(
            "private-key",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            0,
        ),
        make(
            "jwt",
            r"eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            0,
        ),
        make("bearer", r"(?i)(bearer\s+)([A-Za-z0-9._~+/=-]{16,})", 1),
        make(
            "header",
            // A value already replaced by the structural pass starts with
            // `<` or `[`, and is left as it is.
            r#"(?im)^(\s*"?(?:authorization|proxy-authorization|x-api-key|api-key|x-goog-api-key|x-auth-token)"?\s*[:=]\s*"?)([^"\r\n<\[\s][^"\r\n]*)"#,
            1,
        ),
        make(
            "query",
            r"([?&](?:api_key|apikey|key|token|access_token|secret)=)([^&\s\x22']+)",
            1,
        ),
    ]
}

/// Scrub `NAME=value` and `"NAME": "value"` where `NAME` is a credential
/// by the environment's own naming rule. The last step of
/// [`Scrubber::scrub`], on its own because the decision needs the name,
/// which a fixed pattern cannot ask [`is_sensitive_env_name`].
fn scrub_env_assignments(text: &str) -> (String, usize) {
    static ASSIGNMENT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let regex = ASSIGNMENT.get_or_init(|| {
        Regex::new(r#"(?m)(^\s*(?:export\s+)?|")([A-Z][A-Z0-9_]{2,})("?\s*[=:]\s*"?)([^\s"]+)"#)
            .expect("the assignment pattern compiles")
    });
    let mut hits = 0;
    let out = regex.replace_all(text, |caps: &regex::Captures| {
        if is_sensitive_env_name(&caps[2]) {
            hits += 1;
            format!("{}{}{}[REDACTED:env]", &caps[1], &caps[2], &caps[3])
        } else {
            caps[0].to_string()
        }
    });
    (out.into_owned(), hits)
}

/// Every string a JSON document keeps under a secret-shaped key, however
/// deep: what the two auth stores hold, read only so their tokens can be
/// erased from everything else.
pub(crate) fn secret_strings_in(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Array(items) => items.iter().for_each(|v| secret_strings_in(v, out)),
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                match item {
                    serde_json::Value::String(text) if secret_key(key) => out.push(text.clone()),
                    _ => secret_strings_in(item, out),
                }
            }
        }
        _ => {}
    }
}

/// The secret values a loaded config holds, wherever it holds them.
pub(crate) fn config_secrets(config: &crate::config::Config) -> Vec<String> {
    let providers = &config.providers;
    let mut out: Vec<String> = [
        &providers.anthropic_api_key,
        &providers.openai_api_key,
        &providers.google_api_key,
        &providers.meshy_api_key,
        &providers.bedrock_api_key,
        &config.openrouter_api_key,
    ]
    .into_iter()
    .flatten()
    .cloned()
    .collect();
    for headers in [
        &providers.anthropic_headers,
        &providers.openai_headers,
        &providers.google_headers,
        &providers.openrouter_headers,
        &providers.meshy_headers,
        &providers.bedrock_headers,
    ] {
        out.extend(headers.values().cloned());
    }
    for server in &config.mcp_servers {
        out.extend(server.env.values().cloned());
        out.extend(server.headers.values().cloned());
    }
    for gateway in config.model_providers.values() {
        out.extend(gateway.api_key.iter().cloned());
        out.extend(gateway.headers.iter().flat_map(|h| h.values().cloned()));
        out.extend(
            gateway
                .extra
                .iter()
                .filter(|(key, _)| secret_key(key))
                .filter_map(|(_, value)| value.as_str().map(str::to_string)),
        );
    }
    out
}

/// The values of every environment variable whose name says it is a
/// credential, from the names `env_names` lists.
pub(crate) fn env_secrets(
    env_names: &[String],
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    env_names
        .iter()
        .filter(|name| is_sensitive_env_name(name))
        .filter_map(|name| env_lookup(name))
        .collect()
}

/// Which of `names` are set, without their values: what the bundle says
/// about the environment.
pub(crate) fn present_names(
    names: &[String],
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut present: Vec<String> = names
        .iter()
        .filter(|name| env_lookup(name).is_some())
        .cloned()
        .collect();
    present.sort();
    present
}
