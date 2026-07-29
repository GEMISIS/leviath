//! Rhai *script tools* - drop-in tool definitions for agent blueprints.
//!
//! A `.rhai` file in an agent's `tools/` directory (or the global
//! `~/.leviath/tools/`) defines one custom tool. Its metadata comes from comment
//! annotations at the top of the file (`// @tool`, `// @description`, `// @param`)
//! or an optional sibling `tool.toml` (which, when present, overrides the
//! annotations). Each script is compiled to a Rhai [`AST`] once at agent boot.
//!
//! Scripts run sandboxed: the only way they reach the outside world is the small,
//! controlled set of host functions registered on the tool engine. Five of
//! them (`http_get`, `http_post`, `shell`, `read_file`, `env_var`) do I/O and go
//! through a [`ScriptHost`] trait object so the host can enforce permissions and
//! tests can inject a fake; the other three (`parse_json`, `to_json`,
//! `encode_uri`) are pure and defined here.
//!
//! Errors never bubble as a `Result` to the agent - [`execute`] always returns a
//! `String`, using the `[error] …` prefix convention the rest of the tool layer
//! uses, so a failing script surfaces to the model the same way a built-in
//! tool's error does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rhai::{AST, Dynamic, Engine, EvalAltResult, Map, Position, Scope};
use serde::Deserialize;

use crate::{Error, Result};

/// One declared parameter of a script tool.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamSpec {
    /// Parameter name (the key the script reads from `params`).
    pub name: String,
    /// JSON-schema type: `string`, `integer`, `number`, `boolean`, `array`, `object`.
    /// Ignored when [`schema`](Self::schema) is set.
    pub ty: String,
    /// Whether the model must supply this parameter.
    pub required: bool,
    /// Human description shown to the model. Ignored when [`schema`](Self::schema)
    /// is set (the raw fragment supplies its own).
    pub description: String,
    /// An optional raw JSON-Schema fragment for this parameter, used verbatim as
    /// the property's schema instead of the flat `{ type, description }`. Lets a
    /// `tool.toml` author express what annotations can't - enums, array `items`,
    /// numeric bounds, nested object shapes, formats, defaults - matching the
    /// richness built-in and MCP tools advertise. `None` = the flat default.
    pub schema: Option<serde_json::Value>,
}

/// Metadata describing a script tool: its name, description, and parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptToolMeta {
    /// Tool name advertised to the model (must match a blueprint `available_tools` entry).
    pub name: String,
    /// One-line description of what the tool does.
    pub description: String,
    /// Declared parameters, in declaration order.
    pub params: Vec<ParamSpec>,
    /// Platform capabilities the tool declares it needs (e.g. `network`, `shell`,
    /// `filesystem`). The host drops the tool when the platform can't provide one -
    /// a script self-declares what it depends on. Empty = always available.
    pub required_caps: Vec<String>,
}

impl ScriptToolMeta {
    /// Build the JSON-schema `parameters` object advertised to the model, from
    /// the declared [`ParamSpec`]s. Mirrors the hand-written schemas in
    /// `leviath-tools` (`{ type: object, properties, required }`).
    pub fn parameters_schema(&self) -> serde_json::Value {
        let mut properties = serde_json::Map::new();
        let mut required: Vec<serde_json::Value> = Vec::new();
        for p in &self.params {
            // A raw fragment (from `tool.toml`) is used verbatim; otherwise the
            // flat `{ type, description }` default. `required` is governed by the
            // param's `required` flag either way (it lives in the parent schema,
            // not the property).
            let property = match &p.schema {
                Some(fragment) => fragment.clone(),
                None => serde_json::json!({ "type": p.ty, "description": p.description }),
            };
            properties.insert(p.name.clone(), property);
            if p.required {
                required.push(serde_json::Value::String(p.name.clone()));
            }
        }
        serde_json::json!({
            "type": "object",
            "properties": serde_json::Value::Object(properties),
            "required": serde_json::Value::Array(required),
        })
    }
}

// ─── Metadata parsing ───────────────────────────────────────────────────────

/// Parse a script tool's metadata from its `.rhai` source comment annotations.
///
/// Recognized leading `//`-comment directives (order-independent):
/// - `// @tool <name>` - required; names the tool.
/// - `// @description <text>` - optional one-liner.
/// - `// @param <name> <type> <required|optional> "<description>"` - repeatable.
/// - `// @requires <cap> [<cap>...]` - platform capabilities the tool needs
///   (`network`, `shell`, `filesystem`); comma/space-separated, repeatable.
///
/// Non-comment / unrecognized lines are ignored, so a script can mix ordinary
/// comments with directives. A missing `@tool` name is an error.
pub fn parse_annotations(src: &str) -> Result<ScriptToolMeta> {
    let mut name: Option<String> = None;
    let mut description = String::new();
    let mut params: Vec<ParamSpec> = Vec::new();
    let mut required_caps: Vec<String> = Vec::new();

    for line in src.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("//") else {
            continue;
        };
        let rest = rest.trim();
        let Some(directive) = rest.strip_prefix('@') else {
            continue;
        };
        // Split the directive keyword from its argument text.
        let (keyword, arg) = match directive.split_once(char::is_whitespace) {
            Some((k, a)) => (k, a.trim()),
            None => (directive, ""),
        };
        match keyword {
            "tool" => {
                if arg.is_empty() {
                    return Err(Error::ValidationFailed(
                        "@tool directive requires a tool name".to_string(),
                    ));
                }
                name = Some(arg.to_string());
            }
            "description" => description = arg.to_string(),
            "param" => params.push(parse_param_directive(arg)?),
            // `@requires <cap> [<cap>...]` - whitespace/comma-separated, repeatable.
            "requires" => required_caps.extend(
                arg.split([' ', ',', '\t'])
                    .filter(|c| !c.is_empty())
                    .map(str::to_string),
            ),
            _ => {} // unknown directive - ignore
        }
    }

    let name = name.ok_or_else(|| {
        Error::ValidationFailed("script tool is missing a `// @tool <name>` directive".to_string())
    })?;
    Ok(ScriptToolMeta {
        name,
        description,
        params,
        required_caps,
    })
}

/// Parse the argument of a `@param` directive:
/// `<name> <type> <required|optional> "<description>"`.
///
/// The description (everything after the third token) is optional and its
/// surrounding double quotes are stripped when present.
fn parse_param_directive(arg: &str) -> Result<ParamSpec> {
    let mut it = arg.splitn(4, char::is_whitespace).map(str::trim);
    let name = it.next().filter(|s| !s.is_empty());
    let ty = it.next().filter(|s| !s.is_empty());
    let requiredness = it.next().filter(|s| !s.is_empty());
    let (name, ty, requiredness) = match (name, ty, requiredness) {
        (Some(n), Some(t), Some(r)) => (n, t, r),
        _ => {
            return Err(Error::ValidationFailed(format!(
                "@param requires `<name> <type> <required|optional>`, got: `{arg}`"
            )));
        }
    };
    let required = match requiredness {
        "required" => true,
        "optional" => false,
        other => {
            return Err(Error::ValidationFailed(format!(
                "@param requiredness must be `required` or `optional`, got: `{other}`"
            )));
        }
    };
    let description = it
        .next()
        .map(|d| d.trim().trim_matches('"').to_string())
        .unwrap_or_default();
    Ok(ParamSpec {
        name: name.to_string(),
        ty: ty.to_string(),
        required,
        description,
        // Comment annotations have no syntax for a raw schema fragment; that
        // richness is `tool.toml`-only.
        schema: None,
    })
}

/// Serde shape of an optional `tool.toml` sibling manifest.
#[derive(Debug, Deserialize)]
struct ToolTomlDoc {
    tool: ToolTomlTool,
}

#[derive(Debug, Deserialize)]
struct ToolTomlTool {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    params: Vec<ToolTomlParam>,
    /// Platform capabilities the tool requires (`network`, `shell`, `filesystem`).
    #[serde(default)]
    requires: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ToolTomlParam {
    name: String,
    /// The scalar type for the flat default. Optional: a param that supplies its
    /// own `schema` fragment doesn't need it.
    #[serde(default, rename = "type")]
    ty: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    description: String,
    /// Optional raw JSON-Schema fragment, used verbatim as this param's property
    /// schema (enums, `items`, bounds, nested objects, …).
    #[serde(default)]
    schema: Option<serde_json::Value>,
}

/// Parse a `tool.toml` manifest into [`ScriptToolMeta`]. When a `tool.toml` sits
/// beside a script it takes precedence over the script's comment annotations.
pub fn parse_tool_toml(src: &str) -> Result<ScriptToolMeta> {
    let doc: ToolTomlDoc = toml::from_str(src)
        .map_err(|e| Error::ValidationFailed(format!("invalid tool.toml: {e}")))?;
    if doc.tool.name.trim().is_empty() {
        return Err(Error::ValidationFailed(
            "tool.toml `[tool] name` must not be empty".to_string(),
        ));
    }
    let params = doc
        .tool
        .params
        .into_iter()
        .map(|p| ParamSpec {
            name: p.name,
            ty: p.ty,
            required: p.required,
            description: p.description,
            schema: p.schema,
        })
        .collect();
    Ok(ScriptToolMeta {
        name: doc.tool.name,
        description: doc.tool.description,
        params,
        required_caps: doc.tool.requires,
    })
}

// ─── Host seam ──────────────────────────────────────────────────────────────

/// The side-effecting host functions a script tool can call. Implemented by the
/// daemon (with permission enforcement + real I/O) and by tests (with canned
/// responses). Every method returns `Result<String, String>`; an `Err(msg)` is
/// turned into a Rhai exception by the tool engine, which surfaces to the
/// agent as an `[error] …` result.
pub trait ScriptHost: Send + Sync {
    /// HTTP GET `url` with the given request headers, returning the response body.
    fn http_get(
        &self,
        url: &str,
        headers: BTreeMap<String, String>,
    ) -> std::result::Result<String, String>;
    /// HTTP POST `body` to `url` with the given headers, returning the response body.
    fn http_post(
        &self,
        url: &str,
        body: &str,
        headers: BTreeMap<String, String>,
    ) -> std::result::Result<String, String>;
    /// Run a shell command, returning its combined output.
    fn shell(&self, command: &str) -> std::result::Result<String, String>;
    /// Read a file (confined to the agent workdir by the implementor).
    fn read_file(&self, path: &str) -> std::result::Result<String, String>;
    /// Write `content` to a file (confined to the agent workdir by the
    /// implementor), returning a short confirmation.
    fn write_file(&self, path: &str, content: &str) -> std::result::Result<String, String>;
    /// Read an environment variable.
    fn env_var(&self, name: &str) -> std::result::Result<String, String>;
}

// ─── Compiled tool + tool set ───────────────────────────────────────────────

/// A discovered, compiled script tool: its metadata plus the Rhai AST (compiled
/// once) and the path it came from.
#[derive(Clone, Debug)]
pub struct ScriptTool {
    /// Tool metadata (name/description/params).
    pub meta: ScriptToolMeta,
    /// Compiled script AST, evaluated on each call.
    pub ast: AST,
    /// Source `.rhai` path (for diagnostics).
    pub source_path: PathBuf,
}

/// A `.rhai` file that could not be turned into a tool (bad annotations,
/// `tool.toml`, or a compile error). Surfaced so the caller can log it - the
/// library itself does no logging, keeping that policy decision in the host.
#[derive(Debug, Clone)]
pub struct SkippedTool {
    /// The offending file.
    pub path: PathBuf,
    /// Why it was skipped.
    pub reason: String,
}

/// The set of script tools available to one agent, keyed by tool name.
#[derive(Clone, Default)]
pub struct ScriptToolSet {
    tools: BTreeMap<String, ScriptTool>,
}

impl ScriptToolSet {
    /// Discover and compile every `*.rhai` tool in `dirs`, in order. Earlier
    /// directories win on a name collision (so a per-agent `tools/` shadows the
    /// global one). A file that fails to parse (bad annotations/`tool.toml`) or
    /// compile is skipped and reported in the returned [`SkippedTool`] list,
    /// never failing the whole agent. A `tool.toml` sitting beside
    /// `<name>.rhai` overrides that script's annotations.
    pub fn discover(dirs: &[PathBuf]) -> (Self, Vec<SkippedTool>) {
        let mut tools: BTreeMap<String, ScriptTool> = BTreeMap::new();
        let mut skipped: Vec<SkippedTool> = Vec::new();
        // A bare engine is enough to compile (produce an AST); host functions are
        // only needed at eval time.
        let engine = Engine::new();
        for dir in dirs {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(_) => continue, // missing dir is normal (agent has no tools/)
            };
            let mut paths: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|ext| ext == "rhai"))
                .collect();
            paths.sort();
            for path in paths {
                match compile_tool(&engine, &path) {
                    Ok(tool) => {
                        // Earlier dir wins: only insert if not already present.
                        tools.entry(tool.meta.name.clone()).or_insert(tool);
                    }
                    Err(e) => skipped.push(SkippedTool {
                        path,
                        reason: e.to_string(),
                    }),
                }
            }
        }
        (Self { tools }, skipped)
    }

    /// Whether a tool of this name exists in the set.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Look up a compiled tool by name.
    pub fn get(&self, name: &str) -> Option<&ScriptTool> {
        self.tools.get(name)
    }

    /// The names of all tools in the set.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// The metadata of every tool, for building `Tool` defs in the caller.
    pub fn metas(&self) -> Vec<ScriptToolMeta> {
        self.tools.values().map(|t| t.meta.clone()).collect()
    }

    /// Number of tools in the set.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Compile a single `.rhai` file into a [`ScriptTool`], resolving metadata from a
/// sibling `tool.toml` when present, else from the script's comment annotations.
fn compile_tool(engine: &Engine, path: &Path) -> Result<ScriptTool> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| Error::ValidationFailed(format!("read {}: {e}", path.display())))?;
    // tool.toml sibling (`<name>.rhai` → `<name>.toml`)? It overrides annotations.
    let toml_path = path.with_extension("toml");
    let meta = match std::fs::read_to_string(&toml_path) {
        Ok(toml_src) => parse_tool_toml(&toml_src)?,
        Err(_) => parse_annotations(&src)?,
    };
    let ast = engine
        .compile(&src)
        .map_err(|e| Error::CompilationFailed(format!("{}: {e}", path.display())))?;
    Ok(ScriptTool {
        meta,
        ast,
        source_path: path.to_path_buf(),
    })
}

// ─── Execution ──────────────────────────────────────────────────────────────

/// Maximum wall-clock a single script tool call may run. Enforced via the Rhai
/// operation limit already set on the engine; this constant documents intent for
/// the (blocking) host wrapper.
pub const SCRIPT_TOOL_MAX_OPERATIONS: u64 = 500_000;

/// Execute a compiled script tool with the model-supplied `args`, returning the
/// result as a string for the agent. `args` is exposed to the script as the
/// `params` object-map. The returned Rhai value is serialized to JSON unless it
/// is already a string (returned verbatim). Any script error becomes an
/// `[error] …` string.
///
/// A panic raised by a native (host) function never unwinds through this call:
/// it is caught at the native-function boundary by `guard_str`/`guard_dyn`
/// and arrives here as an ordinary script error. Anything that
/// still escapes - a panic from Rhai's own internals - is contained one level
/// up, where the daemon runs this on a `spawn_blocking` task and turns the
/// resulting `JoinError` into a tool error.
pub fn execute(tool: &ScriptTool, args: serde_json::Value, host: Arc<dyn ScriptHost>) -> String {
    let engine = build_tool_engine(host);
    // Converting a `serde_json::Value` to a Rhai `Dynamic` is infallible (any
    // JSON maps to a Dynamic); fall back to unit on the impossible error rather
    // than carry a dead error arm.
    let params = rhai::serde::to_dynamic(args).unwrap_or(Dynamic::UNIT);
    let mut scope = Scope::new();
    scope.push_dynamic("params", params);
    match engine.eval_ast_with_scope::<Dynamic>(&mut scope, &tool.ast) {
        Ok(value) => dynamic_to_result_string(value),
        Err(e) => format!("[error] {}: {}", tool.meta.name, e),
    }
}

/// Serialize a script's return value for the agent: strings pass through
/// verbatim; everything else is JSON-encoded (so an array/map return renders as
/// JSON). Unit `()` becomes an empty string.
fn dynamic_to_result_string(value: Dynamic) -> String {
    if value.is_string() {
        // `into_string` cannot fail here (checked `is_string`).
        return value.into_string().unwrap_or_default();
    }
    if value.is_unit() {
        return String::new();
    }
    match rhai::serde::from_dynamic::<serde_json::Value>(&value) {
        // `Value`'s `Display` (to_string) is infallible, unlike `serde_json::to_string`.
        Ok(json) => json.to_string(),
        Err(e) => format!("[error] cannot serialize result: {e}"),
    }
}

/// A Rhai engine with sandbox limits, the shared Leviath helpers, and the eight
/// script-tool host functions registered.
fn build_tool_engine(host: Arc<dyn ScriptHost>) -> Engine {
    let mut engine = Engine::new();
    crate::harden(&mut engine, SCRIPT_TOOL_MAX_OPERATIONS);
    crate::functions::register_functions(&mut engine);
    crate::types::register_types(&mut engine);
    register_host_functions(&mut engine, host);
    engine
}

/// What a registered native function hands back to Rhai.
type HostRes<T> = std::result::Result<T, Box<EvalAltResult>>;

/// Turn a host `Result<String, String>` into a Rhai fn result, mapping `Err`
/// into a runtime exception (which `execute` renders as `[error] …`).
fn to_rhai(r: std::result::Result<String, String>) -> HostRes<String> {
    r.map_err(|msg| Box::new(EvalAltResult::ErrorRuntime(msg.into(), Position::NONE)))
}

/// Turn a caught panic payload into the same runtime exception a normal host
/// error produces, so Rhai unwinds nothing.
fn panic_to_rhai(name: &str, payload: Box<dyn std::any::Any + Send>) -> Box<EvalAltResult> {
    let msg = leviath_core::panic_message(payload.as_ref());
    tracing::warn!(
        host_fn = name,
        panic = %msg,
        "a script-tool host function panicked; surfacing it as a script error (issue #109)"
    );
    Box::new(EvalAltResult::ErrorRuntime(
        format!("{name} panicked: {msg}").into(),
        Position::NONE,
    ))
}

/// Run a `String`-returning native function so a panic inside it **never**
/// unwinds into Rhai.
///
/// Rhai's `exec_native_fn_call` takes an `ArgBackup` whenever the first
/// argument is a variable reference (which is every real call shape, e.g.
/// `http_get(params.url)`), and restores it *after* the call returns. A
/// panicking native function skips that restore, and `ArgBackup`'s destructor
/// then asserts during unwinding - a second panic while panicking, which Rust
/// turns into `abort()`, taking down the whole daemon and every concurrent
/// run. Catching here means the unwind never reaches Rhai's frame at all.
///
/// Deliberately **not generic**: a generic guard monomorphizes per closure, and
/// each instantiation's panic arm would then need its own test to hold the
/// workspace's 100% coverage gate. One `&mut dyn FnMut` instantiation keeps
/// every caller's regions merged into one.
fn guard_str(name: &str, f: &mut dyn FnMut() -> HostRes<String>) -> HostRes<String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => Err(panic_to_rhai(name, payload)),
    }
}

/// [`guard_str`] for the one native function that returns a `Dynamic`
/// (`parse_json`). Same rationale, different return type - kept non-generic for
/// the same coverage reason.
fn guard_dyn(name: &str, f: &mut dyn FnMut() -> HostRes<Dynamic>) -> HostRes<Dynamic> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => Err(panic_to_rhai(name, payload)),
    }
}

/// Convert a Rhai object-map of headers into a `BTreeMap<String,String>`, each
/// value stringified.
/// Borrows rather than consumes so the guarded `FnMut` wrappers in
/// [`register_host_functions`] can call it without moving out of a capture.
fn headers_from_map(map: &Map) -> BTreeMap<String, String> {
    map.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Register the eight host functions. Five delegate to [`ScriptHost`]; three
/// (`parse_json`, `to_json`, `encode_uri`) are pure.
///
/// **Every** registration goes through [`guard_str`] / [`guard_dyn`], so a panic
/// anywhere in a native function becomes an ordinary Rhai runtime error instead
/// of unwinding into Rhai and aborting the process. The pure
/// helpers are guarded too - they run on untrusted, model- and network-supplied
/// input, so "this one can't panic" is not a property worth betting the daemon on.
fn register_host_functions(engine: &mut Engine, host: Arc<dyn ScriptHost>) {
    // http_get(url) / http_get(url, headers)
    let h = host.clone();
    engine.register_fn("http_get", move |url: &str| {
        guard_str("http_get", &mut || {
            to_rhai(h.http_get(url, BTreeMap::new()))
        })
    });
    let h = host.clone();
    engine.register_fn("http_get", move |url: &str, headers: Map| {
        guard_str("http_get", &mut || {
            to_rhai(h.http_get(url, headers_from_map(&headers)))
        })
    });

    // http_post(url, body) / http_post(url, body, headers)
    let h = host.clone();
    engine.register_fn("http_post", move |url: &str, body: &str| {
        guard_str("http_post", &mut || {
            to_rhai(h.http_post(url, body, BTreeMap::new()))
        })
    });
    let h = host.clone();
    engine.register_fn("http_post", move |url: &str, body: &str, headers: Map| {
        guard_str("http_post", &mut || {
            to_rhai(h.http_post(url, body, headers_from_map(&headers)))
        })
    });

    // shell(cmd)
    let h = host.clone();
    engine.register_fn("shell", move |cmd: &str| {
        guard_str("shell", &mut || to_rhai(h.shell(cmd)))
    });

    // read_file(path)
    let h = host.clone();
    engine.register_fn("read_file", move |path: &str| {
        guard_str("read_file", &mut || to_rhai(h.read_file(path)))
    });

    // write_file(path, content)
    let h = host.clone();
    engine.register_fn("write_file", move |path: &str, content: &str| {
        guard_str("write_file", &mut || to_rhai(h.write_file(path, content)))
    });

    // env_var(name)
    let h = host.clone();
    engine.register_fn("env_var", move |name: &str| {
        guard_str("env_var", &mut || to_rhai(h.env_var(name)))
    });

    // Pure helpers. Their bodies live in named free functions (not inline
    // closures) so they get a single, cleanly-attributed monomorphization under
    // coverage instrumentation instead of being inlined into rhai's generic
    // `register_fn` wrapper (a known attribution artifact).
    engine.register_fn("parse_json", |s: &str| -> HostRes<Dynamic> {
        guard_dyn("parse_json", &mut || parse_json_fn(s))
    });
    engine.register_fn("to_json", |v: Dynamic| -> HostRes<String> {
        guard_str("to_json", &mut || to_json_fn(&v))
    });
    engine.register_fn("encode_uri", |s: &str| -> HostRes<String> {
        guard_str("encode_uri", &mut || Ok(percent_encode(s)))
    });
    engine.register_fn("html_to_text", |s: &str| -> HostRes<String> {
        guard_str("html_to_text", &mut || Ok(html_to_text(s)))
    });
}

/// `parse_json(str)` host function: JSON string → Rhai value.
fn parse_json_fn(s: &str) -> HostRes<Dynamic> {
    let value: serde_json::Value = serde_json::from_str(s).map_err(|e| {
        Box::new(EvalAltResult::ErrorRuntime(
            format!("parse_json: {e}").into(),
            Position::NONE,
        ))
    })?;
    rhai::serde::to_dynamic(value)
}

/// `to_json(value)` host function: Rhai value → JSON string. `from_dynamic`
/// fails for values with no JSON representation (e.g. a function pointer);
/// `Value::to_string` (Display) is then infallible.
fn to_json_fn(v: &Dynamic) -> HostRes<String> {
    let json: serde_json::Value = rhai::serde::from_dynamic(v)?;
    Ok(json.to_string())
}

/// Percent-encode a string for use in a URL query component. Unreserved
/// characters (`A-Z a-z 0-9 - _ . ~`, per RFC 3986) pass through; every other
/// byte becomes `%XX`.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0f));
            }
        }
    }
    out
}

/// Map a nibble (0–15) to its uppercase hex digit.
fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

/// `html_to_text(html)` host function: best-effort HTML → readable plain text.
/// Drops `<script>`/`<style>` blocks, strips tags, decodes common entities, and
/// collapses whitespace - so a script tool (e.g. `web_fetch`) can hand the model
/// prose from a server-rendered page instead of markup. Not a full HTML parser;
/// content injected by client-side JS is not present in the source and cannot be
/// recovered here.
fn html_to_text(html: &str) -> String {
    let without_raw = strip_raw_text_elements(html);
    let without_tags = strip_tags(&without_raw);
    let decoded = decode_entities(&without_tags);
    collapse_whitespace(&decoded)
}

/// Remove `<script>…</script>` and `<style>…</style>` element contents (their
/// text is code/CSS, never prose). Case-insensitive; an unclosed element drops
/// the remainder.
fn strip_raw_text_elements(html: &str) -> String {
    let mut s = html.to_string();
    for tag in ["script", "style"] {
        s = strip_element(&s, tag);
    }
    s
}

#[expect(
    clippy::string_slice,
    reason = "`i` only ever advances by `ch.len_utf8()` and `rel` comes from `find`, so both are \
              char boundaries; `to_ascii_lowercase` preserves byte lengths, so `lower` and `html` \
              share them"
)]
fn strip_element(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        if lower[i..].starts_with(&open) {
            match lower[i..].find(&close) {
                Some(rel) => {
                    i += rel + close.len();
                    continue;
                }
                None => break, // unclosed element - drop the rest
            }
        }
        let ch = html[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Strip `<...>` tags. Each tag boundary becomes a space so adjacent words don't
/// run together. A `<` with no matching `>` drops the remainder (malformed).
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// How many characters past an `&` to look for the closing `;`. The longest
/// entity this decoder recognises is `&#x10FFFF;` (10 chars); 12 leaves headroom.
const ENTITY_SCAN_CHARS: usize = 12;

/// Decode common HTML entities (named + numeric decimal/hex). Unknown or
/// unterminated entities are left verbatim.
///
/// The scan is bounded by **characters**, not bytes. Bounding it by bytes
/// aborts the daemon: `after` begins at an `&`, so a fixed
/// byte-12 cut-off slices mid-character on any multi-byte text
/// (`"&日本語日本"` → *"byte index 12 is not a char boundary"*), and
/// `html_to_text` runs this over every fetched page. Clamping the byte window
/// down to a boundary would also work, but only because `&` is single-byte -
/// an unstated invariant that a later edit could quietly break. Indices from
/// `char_indices` are boundaries by construction, so there is nothing left to
/// get wrong. Entities are all ASCII, so the two bounds agree on any real one.
#[expect(
    clippy::string_slice,
    reason = "`amp` comes from `find` and `semi` from `char_indices`, so every index here is a \
              char boundary; `after[1..]` is safe because `after` starts at the single-byte '&'"
)]
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let semi = after
            .char_indices()
            .take(ENTITY_SCAN_CHARS)
            .find(|&(_, c)| c == ';')
            .map(|(i, _)| i);
        match semi {
            Some(semi) => match decode_one_entity(&after[1..semi]) {
                Some(ch) => {
                    out.push(ch);
                    rest = &after[semi + 1..];
                }
                None => {
                    out.push('&');
                    rest = &after[1..];
                }
            },
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_one_entity(e: &str) -> Option<char> {
    match e {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        "mdash" => Some('\u{2014}'),
        "ndash" => Some('–'),
        "hellip" => Some('…'),
        _ => {
            if let Some(hex) = e.strip_prefix("#x").or_else(|| e.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = e.strip_prefix('#') {
                dec.parse::<u32>().ok().and_then(char::from_u32)
            } else {
                None
            }
        }
    }
}

/// Collapse every run of whitespace to a single space and trim.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, PoisonError};

    /// Serializes the tests that swap the **process-global** panic hook. Without
    /// it they interleave under the parallel test runner: one test's `set_hook`
    /// replaces another's silencing closure before that test's panic fires, so
    /// the closure never runs and reads as uncovered.
    static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

    // ── A fake host recording calls and returning canned results. ──

    type Headers = BTreeMap<String, String>;
    /// Recorded `http_get` call: (url, headers).
    type GetCall = Option<(String, Headers)>;
    /// Recorded `http_post` call: (url, body, headers).
    type PostCall = Option<(String, String, Headers)>;
    type HostResult = std::result::Result<String, String>;

    struct FakeHost {
        get_response: Mutex<HostResult>,
        post_response: Mutex<HostResult>,
        shell_response: Mutex<HostResult>,
        read_response: Mutex<HostResult>,
        env_response: Mutex<HostResult>,
        last_get: Mutex<GetCall>,
        last_post: Mutex<PostCall>,
    }

    impl FakeHost {
        fn arc() -> Arc<FakeHost> {
            Arc::new(FakeHost {
                get_response: Mutex::new(Ok("GET-OK".to_string())),
                post_response: Mutex::new(Ok("POST-OK".to_string())),
                shell_response: Mutex::new(Ok("SHELL-OK".to_string())),
                read_response: Mutex::new(Ok("READ-OK".to_string())),
                env_response: Mutex::new(Ok("ENV-OK".to_string())),
                last_get: Mutex::new(None),
                last_post: Mutex::new(None),
            })
        }
    }

    impl ScriptHost for FakeHost {
        fn http_get(
            &self,
            url: &str,
            headers: BTreeMap<String, String>,
        ) -> std::result::Result<String, String> {
            *self.last_get.lock().unwrap() = Some((url.to_string(), headers));
            self.get_response.lock().unwrap().clone()
        }
        fn http_post(
            &self,
            url: &str,
            body: &str,
            headers: BTreeMap<String, String>,
        ) -> std::result::Result<String, String> {
            *self.last_post.lock().unwrap() = Some((url.to_string(), body.to_string(), headers));
            self.post_response.lock().unwrap().clone()
        }
        fn shell(&self, _command: &str) -> std::result::Result<String, String> {
            self.shell_response.lock().unwrap().clone()
        }
        fn read_file(&self, _path: &str) -> std::result::Result<String, String> {
            self.read_response.lock().unwrap().clone()
        }
        fn write_file(&self, path: &str, content: &str) -> std::result::Result<String, String> {
            Ok(format!("WROTE:{path}={content}"))
        }
        fn env_var(&self, _name: &str) -> std::result::Result<String, String> {
            self.env_response.lock().unwrap().clone()
        }
    }

    fn tool_from(src: &str) -> ScriptTool {
        let engine = Engine::new();
        let ast = engine.compile(src).expect("compile");
        ScriptTool {
            meta: parse_annotations(src).expect("annotations"),
            ast,
            source_path: PathBuf::from("mem.rhai"),
        }
    }

    // ── parse_annotations ──

    #[test]
    fn annotations_full() {
        let src = r#"
// @tool web_search
// @description Search the web
// @param query string required "Search query"
// @param count integer optional "How many"
42
"#;
        let meta = parse_annotations(src).unwrap();
        assert_eq!(meta.name, "web_search");
        assert_eq!(meta.description, "Search the web");
        assert_eq!(meta.params.len(), 2);
        assert_eq!(
            meta.params[0],
            ParamSpec {
                name: "query".into(),
                ty: "string".into(),
                required: true,
                description: "Search query".into(),
                schema: None,
            }
        );
        assert!(!meta.params[1].required);
        assert!(meta.required_caps.is_empty());
    }

    #[test]
    fn annotations_requires_capabilities() {
        // Space- and comma-separated, repeatable across lines.
        let src = "// @tool t\n// @requires network, shell\n// @requires filesystem\n1";
        let meta = parse_annotations(src).unwrap();
        assert_eq!(meta.required_caps, ["network", "shell", "filesystem"]);
    }

    #[test]
    fn annotations_missing_tool_name_errors() {
        let err = parse_annotations("// @description no name\n1").unwrap_err();
        assert!(err.to_string().contains("missing a `// @tool"));
    }

    #[test]
    fn annotations_empty_tool_name_errors() {
        let err = parse_annotations("// @tool   \n1").unwrap_err();
        assert!(err.to_string().contains("requires a tool name"));
    }

    #[test]
    fn annotations_ignore_non_comment_and_non_directive_lines() {
        let src = "let x = 1; // trailing\n// plain comment\n// @tool t\nx";
        let meta = parse_annotations(src).unwrap();
        assert_eq!(meta.name, "t");
        assert!(meta.params.is_empty());
        assert_eq!(meta.description, "");
    }

    #[test]
    fn annotations_unknown_directive_ignored() {
        let meta = parse_annotations("// @tool t\n// @bogus whatever\n1").unwrap();
        assert_eq!(meta.name, "t");
    }

    #[test]
    fn annotations_directive_with_no_arg_is_handled() {
        // A directive keyword with no whitespace/arg (the `None` split arm).
        let meta = parse_annotations("// @tool t\n// @description\n1").unwrap();
        assert_eq!(meta.description, "");
    }

    #[test]
    fn param_without_description_defaults_empty() {
        let meta = parse_annotations("// @tool t\n// @param x string required\n1").unwrap();
        assert_eq!(meta.params[0].description, "");
        assert!(meta.params[0].required);
    }

    #[test]
    fn param_optional_flag() {
        let meta = parse_annotations("// @tool t\n// @param x string optional\n1").unwrap();
        assert!(!meta.params[0].required);
    }

    #[test]
    fn param_too_few_tokens_errors() {
        let err = parse_annotations("// @tool t\n// @param x string\n1").unwrap_err();
        assert!(err.to_string().contains("requires `<name> <type>"));
    }

    #[test]
    fn param_bad_requiredness_errors() {
        let err = parse_annotations("// @tool t\n// @param x string maybe\n1").unwrap_err();
        assert!(err.to_string().contains("must be `required` or `optional`"));
    }

    // ── parse_tool_toml ──

    #[test]
    fn tool_toml_full() {
        let src = r#"
[tool]
name = "fetch"
description = "Fetch a URL"
[[tool.params]]
name = "url"
type = "string"
required = true
description = "The URL"
"#;
        let meta = parse_tool_toml(src).unwrap();
        assert_eq!(meta.name, "fetch");
        assert_eq!(meta.description, "Fetch a URL");
        assert_eq!(meta.params.len(), 1);
        assert!(meta.params[0].required);
        assert_eq!(meta.params[0].ty, "string");
    }

    #[test]
    fn tool_toml_requires() {
        let meta = parse_tool_toml("[tool]\nname = \"t\"\nrequires = [\"network\"]").unwrap();
        assert_eq!(meta.required_caps, ["network"]);
    }

    #[test]
    fn tool_toml_defaults() {
        let meta = parse_tool_toml("[tool]\nname = \"t\"").unwrap();
        assert_eq!(meta.description, "");
        assert!(meta.params.is_empty());
        assert!(meta.required_caps.is_empty());
    }

    #[test]
    fn tool_toml_raw_schema_fragment() {
        // A param supplying its own `schema` fragment (and no `type`) parses the
        // fragment into ParamSpec.schema for verbatim use.
        let src = r#"
[tool]
name = "export"
[[tool.params]]
name = "format"
required = true
schema = { type = "string", enum = ["json", "yaml"], description = "Output format" }
"#;
        let meta = parse_tool_toml(src).unwrap();
        assert_eq!(meta.params.len(), 1);
        assert!(meta.params[0].required);
        // No `type` key was given → the flat `ty` defaulted to empty.
        assert_eq!(meta.params[0].ty, "");
        let frag = meta.params[0].schema.as_ref().unwrap();
        assert_eq!(frag["enum"][0], "json");
    }

    #[test]
    fn tool_toml_invalid_syntax_errors() {
        let err = parse_tool_toml("not = valid = toml").unwrap_err();
        assert!(err.to_string().contains("invalid tool.toml"));
    }

    #[test]
    fn tool_toml_empty_name_errors() {
        let err = parse_tool_toml("[tool]\nname = \"\"").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    // ── parameters_schema ──

    #[test]
    fn parameters_schema_shape() {
        let meta = parse_annotations(
            "// @tool t\n// @param a string required \"A\"\n// @param b integer optional \"B\"\n1",
        )
        .unwrap();
        let schema = meta.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["a"]["type"], "string");
        assert_eq!(schema["properties"]["b"]["description"], "B");
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "a");
    }

    #[test]
    fn parameters_schema_uses_raw_fragment_verbatim() {
        // A param carrying a raw fragment: the fragment becomes the property
        // schema as-is (enum preserved), and `required` still governs the parent
        // `required` array.
        let meta = parse_tool_toml(
            "[tool]\nname = \"t\"\n[[tool.params]]\nname = \"fmt\"\nrequired = true\nschema = { type = \"string\", enum = [\"a\", \"b\"] }\n",
        )
        .unwrap();
        let schema = meta.parameters_schema();
        assert_eq!(schema["properties"]["fmt"]["type"], "string");
        assert_eq!(schema["properties"]["fmt"]["enum"][1], "b");
        // The flat `{type, description}` shape is NOT applied over the fragment.
        assert!(schema["properties"]["fmt"].get("description").is_none());
        assert_eq!(schema["required"][0], "fmt");
    }

    // ── discover ──

    #[test]
    fn discover_compiles_and_collides() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        // Same tool name in both dirs; dir_a listed first must win.
        std::fs::write(
            dir_a.path().join("dup.rhai"),
            "// @tool dup\n// @description from A\n1",
        )
        .unwrap();
        std::fs::write(
            dir_b.path().join("dup.rhai"),
            "// @tool dup\n// @description from B\n2",
        )
        .unwrap();
        std::fs::write(dir_b.path().join("solo.rhai"), "// @tool solo\n3").unwrap();
        // A non-.rhai file is ignored; a broken script is skipped.
        std::fs::write(dir_b.path().join("note.txt"), "ignored").unwrap();
        std::fs::write(
            dir_b.path().join("broken.rhai"),
            "// no tool directive\nlet",
        )
        .unwrap();

        let (set, skipped) = ScriptToolSet::discover(&[
            dir_a.path().to_path_buf(),
            dir_b.path().to_path_buf(),
            dir_a.path().join("does-not-exist"),
        ]);
        assert_eq!(set.len(), 2);
        assert!(!set.is_empty());
        assert!(set.contains("dup"));
        assert!(set.contains("solo"));
        assert_eq!(set.get("dup").unwrap().meta.description, "from A");
        let mut names = set.names();
        names.sort();
        assert_eq!(names, vec!["dup".to_string(), "solo".to_string()]);
        assert_eq!(set.metas().len(), 2);
        // The broken.rhai (no @tool directive) was skipped and reported.
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].path.ends_with("broken.rhai"));
        assert!(!skipped[0].reason.is_empty());
    }

    #[test]
    fn discover_uses_tool_toml_override() {
        let dir = tempfile::tempdir().unwrap();
        // Annotations say name "ann"; tool.toml overrides to "override".
        std::fs::write(dir.path().join("t.rhai"), "// @tool ann\n1").unwrap();
        std::fs::write(
            dir.path().join("t.toml"),
            "[tool]\nname = \"override\"\ndescription = \"D\"",
        )
        .unwrap();
        let (set, skipped) = ScriptToolSet::discover(&[dir.path().to_path_buf()]);
        assert!(set.contains("override"));
        assert!(!set.contains("ann"));
        assert!(skipped.is_empty());
    }

    #[test]
    fn discover_skips_invalid_tool_toml() {
        let dir = tempfile::tempdir().unwrap();
        // A valid script, but a broken sibling tool.toml → compile_tool errors on
        // the `parse_tool_toml(..)?` arm → skipped.
        std::fs::write(dir.path().join("t.rhai"), "// @tool t\n1").unwrap();
        std::fs::write(dir.path().join("t.toml"), "name = broken").unwrap();
        let (set, skipped) = ScriptToolSet::discover(&[dir.path().to_path_buf()]);
        assert!(set.is_empty());
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("tool.toml"));
    }

    #[test]
    fn discover_skips_uncompilable_but_valid_annotation() {
        let dir = tempfile::tempdir().unwrap();
        // Valid annotation, but the body is a syntax error → compile fails → skip.
        std::fs::write(dir.path().join("t.rhai"), "// @tool t\nlet x = ;").unwrap();
        let (set, _) = ScriptToolSet::discover(&[dir.path().to_path_buf()]);
        assert!(set.is_empty());
    }

    #[test]
    fn default_set_is_empty() {
        let set = ScriptToolSet::default();
        assert!(set.is_empty());
        assert!(set.get("x").is_none());
    }

    // ── execute ──

    #[test]
    fn execute_returns_string_verbatim() {
        let tool = tool_from("// @tool t\n\"hello \" + params.name");
        let out = execute(&tool, serde_json::json!({"name": "world"}), FakeHost::arc());
        assert_eq!(out, "hello world");
    }

    #[test]
    fn execute_serializes_non_string_result() {
        let tool = tool_from("// @tool t\n[1, 2, 3]");
        let out = execute(&tool, serde_json::json!({}), FakeHost::arc());
        assert_eq!(out, "[1,2,3]");
    }

    #[test]
    fn execute_unserializable_result_errors() {
        // A script returning a function pointer has no JSON representation, so
        // dynamic_to_result_string hits its `Err` arm.
        let tool = tool_from("// @tool t\n|| 1");
        let out = execute(&tool, serde_json::json!({}), FakeHost::arc());
        assert!(out.contains("cannot serialize result"), "got: {out}");
    }

    #[test]
    fn execute_unit_result_is_empty() {
        let tool = tool_from("// @tool t\nlet x = 1;");
        let out = execute(&tool, serde_json::json!({}), FakeHost::arc());
        assert_eq!(out, "");
    }

    #[test]
    fn execute_html_to_text_host_fn_via_script() {
        // Exercises the registered `html_to_text` engine binding (not just the
        // free function): a script strips markup to prose.
        let tool = tool_from("// @tool t\nhtml_to_text(\"<p>Hi&amp;<b>bye</b></p>\")");
        let out = execute(&tool, serde_json::json!({}), FakeHost::arc());
        assert_eq!(out, "Hi& bye");
    }

    #[test]
    fn execute_missing_optional_param_reads_as_unit() {
        // Mirrors the issue's `params.count == ()` idiom.
        let tool = tool_from("// @tool t\nif params.count == () { \"default\" } else { \"set\" }");
        let out = execute(&tool, serde_json::json!({"query": "x"}), FakeHost::arc());
        assert_eq!(out, "default");
    }

    #[test]
    fn execute_script_error_is_prefixed() {
        let tool = tool_from("// @tool t\nthrow \"boom\"");
        let out = execute(&tool, serde_json::json!({}), FakeHost::arc());
        assert!(out.starts_with("[error] t:"), "got: {out}");
        assert!(out.contains("boom"));
    }

    /// Controls what kind of panic payload a [`PanickingHost`] produces.
    enum PanicPayload {
        /// `panic!("{}", msg)` → `String` payload (downcast_ref::<String>).
        Formatted(&'static str),
        /// `panic!("…")` → `&'static str` payload (downcast_ref::<&str>).
        Literal,
        /// `panic_any(42i32)` → non-string payload (falls through to
        /// "unknown panic").
        NonString,
    }

    /// A [`ScriptHost`] where every method panics unconditionally, using
    /// the payload kind specified by `payload`. This avoids dead
    /// `Ok(…)` branches that would show up as uncovered.
    struct PanickingHost {
        payload: PanicPayload,
    }

    impl PanickingHost {
        fn do_panic(&self) -> ! {
            match &self.payload {
                PanicPayload::Formatted(msg) => panic!("{}", msg),
                PanicPayload::Literal => panic!("literal str panic"),
                PanicPayload::NonString => std::panic::panic_any(42_i32),
            }
        }
    }

    impl ScriptHost for PanickingHost {
        fn http_get(
            &self,
            _u: &str,
            _h: BTreeMap<String, String>,
        ) -> std::result::Result<String, String> {
            self.do_panic();
        }
        fn http_post(
            &self,
            _u: &str,
            _b: &str,
            _h: BTreeMap<String, String>,
        ) -> std::result::Result<String, String> {
            self.do_panic();
        }
        fn shell(&self, _c: &str) -> std::result::Result<String, String> {
            self.do_panic();
        }
        fn read_file(&self, _p: &str) -> std::result::Result<String, String> {
            self.do_panic();
        }
        fn write_file(&self, _p: &str, _c: &str) -> std::result::Result<String, String> {
            self.do_panic();
        }
        fn env_var(&self, _n: &str) -> std::result::Result<String, String> {
            self.do_panic();
        }
    }

    /// Run a script whose only host call panics and return the tool's output,
    /// with the process panic hook silenced for the duration (the panic is
    /// expected; its default backtrace would just spam the test log).
    fn execute_with_panicking_host(payload: PanicPayload, script: &str) -> String {
        let host: Arc<dyn ScriptHost> = Arc::new(PanickingHost { payload });
        let tool = tool_from(script);
        let _guard = PANIC_HOOK_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let out = execute(&tool, serde_json::json!({}), host);
        std::panic::set_hook(prev);
        out
    }

    /// Assert the tool reported a guarded panic from `host_fn` carrying `detail`.
    fn assert_guarded_panic(out: &str, tool_name: &str, host_fn: &str, detail: &str) {
        assert!(
            out.starts_with(&format!("[error] {tool_name}:")),
            "got: {out}"
        );
        assert!(out.contains(&format!("{host_fn} panicked")), "got: {out}");
        assert!(out.contains(detail), "got: {out}");
    }

    #[test]
    fn every_host_fn_panic_becomes_a_script_error() {
        // A panicking host function must NEVER unwind into Rhai: rhai's
        // `exec_native_fn_call` holds an `ArgBackup` whose destructor asserts,
        // so unwinding through it is a double panic → `abort()` → the whole
        // daemon dies (issue #109). Each of these would have aborted the test
        // process before the guards existed.
        for (host_fn, tool_name, script) in [
            ("http_get", "t", "// @tool t\nhttp_get(\"http://x\")"),
            (
                "http_get",
                "t",
                "// @tool t\nhttp_get(\"http://x\", #{ \"A\": \"b\" })",
            ),
            (
                "http_post",
                "t",
                "// @tool t\nhttp_post(\"http://x\", \"b\")",
            ),
            (
                "http_post",
                "t",
                "// @tool t\nhttp_post(\"http://x\", \"b\", #{ \"A\": \"b\" })",
            ),
            ("shell", "sh", "// @tool sh\nshell(\"ls\")"),
            ("read_file", "rf", "// @tool rf\nread_file(\"x.txt\")"),
            (
                "write_file",
                "wf",
                "// @tool wf\nwrite_file(\"out.txt\", \"data\")",
            ),
            ("env_var", "ev", "// @tool ev\nenv_var(\"HOME\")"),
        ] {
            let out =
                execute_with_panicking_host(PanicPayload::Formatted("TLS init failed"), script);
            assert_guarded_panic(&out, tool_name, host_fn, "TLS init failed");
        }
    }

    #[test]
    fn guarded_panic_renders_str_and_non_string_payloads() {
        let out = execute_with_panicking_host(
            PanicPayload::Literal,
            "// @tool t\nhttp_get(\"http://x\")",
        );
        assert_guarded_panic(&out, "t", "http_get", "literal str panic");

        let out = execute_with_panicking_host(
            PanicPayload::NonString,
            "// @tool t\nhttp_get(\"http://x\")",
        );
        assert_guarded_panic(&out, "t", "http_get", "unknown panic");
    }

    #[test]
    fn guards_pass_through_success_and_convert_panics() {
        // Both guards, both arms, called directly: the pure host functions
        // (`parse_json` / `html_to_text` / …) can't be made to panic through a
        // script, so their panic arm is exercised here.
        let _guard = PANIC_HOOK_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            guard_str("ok_str", &mut || Ok("value".to_string())).unwrap(),
            "value"
        );
        assert!(
            guard_dyn("ok_dyn", &mut || Ok(Dynamic::from(7_i64)))
                .unwrap()
                .is_int()
        );

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let str_err = guard_str("boom_str", &mut || panic!("string arm")).unwrap_err();
        let dyn_err = guard_dyn("boom_dyn", &mut || panic!("dynamic arm")).unwrap_err();
        std::panic::set_hook(prev);
        assert!(
            str_err
                .to_string()
                .contains("boom_str panicked: string arm")
        );
        assert!(
            dyn_err
                .to_string()
                .contains("boom_dyn panicked: dynamic arm")
        );
    }

    #[test]
    fn execute_scalar_args_run() {
        // Any JSON (including a scalar) converts to a `params` Dynamic; the
        // script simply ignores it here.
        let tool = tool_from("// @tool t\n\"ok\"");
        let out = execute(&tool, serde_json::json!(5), FakeHost::arc());
        assert_eq!(out, "ok");
    }

    #[test]
    fn execute_print_and_debug_are_noop() {
        // Exercises the no-op `on_print`/`on_debug` closures on the tool engine.
        let tool = tool_from("// @tool t\nprint(\"p\"); debug(\"d\"); \"done\"");
        let out = execute(&tool, serde_json::json!({}), FakeHost::arc());
        assert_eq!(out, "done");
    }

    #[test]
    fn compile_tool_read_error() {
        let engine = Engine::new();
        let err = compile_tool(&engine, Path::new("/no/such/dir/tool.rhai")).unwrap_err();
        assert!(err.to_string().contains("read"));
    }

    #[test]
    fn to_json_on_unserializable_value_errors() {
        // A function pointer has no JSON representation → from_dynamic errors,
        // surfacing as a script `[error]`.
        let tool = tool_from("// @tool t\nlet f = || 1; to_json(f)");
        let out = execute(&tool, serde_json::json!({}), FakeHost::arc());
        assert!(out.starts_with("[error]"), "got: {out}");
    }

    // ── host functions via a script ──

    #[test]
    fn http_get_no_headers() {
        let host = FakeHost::arc();
        let tool = tool_from("// @tool t\nhttp_get(\"http://x\")");
        let out = execute(&tool, serde_json::json!({}), host.clone());
        assert_eq!(out, "GET-OK");
        let (url, headers) = host.last_get.lock().unwrap().clone().unwrap();
        assert_eq!(url, "http://x");
        assert!(headers.is_empty());
    }

    #[test]
    fn http_get_with_headers() {
        let host = FakeHost::arc();
        let tool = tool_from("// @tool t\nhttp_get(\"http://x\", #{ \"K\": \"V\" })");
        let out = execute(&tool, serde_json::json!({}), host.clone());
        assert_eq!(out, "GET-OK");
        let (_, headers) = host.last_get.lock().unwrap().clone().unwrap();
        assert_eq!(headers.get("K").map(String::as_str), Some("V"));
    }

    #[test]
    fn http_get_error_surfaces() {
        let host = FakeHost::arc();
        *host.get_response.lock().unwrap() = Err("[denied] http_get".to_string());
        let tool = tool_from("// @tool t\nhttp_get(\"http://x\")");
        let out = execute(&tool, serde_json::json!({}), host);
        assert!(out.contains("[denied] http_get"));
    }

    #[test]
    fn http_post_variants() {
        let host = FakeHost::arc();
        let tool = tool_from("// @tool t\nhttp_post(\"http://x\", \"body\")");
        assert_eq!(
            execute(&tool, serde_json::json!({}), host.clone()),
            "POST-OK"
        );
        let (_, body, headers) = host.last_post.lock().unwrap().clone().unwrap();
        assert_eq!(body, "body");
        assert!(headers.is_empty());

        let tool2 = tool_from("// @tool t\nhttp_post(\"http://x\", \"b\", #{ \"H\": \"1\" })");
        assert_eq!(
            execute(&tool2, serde_json::json!({}), host.clone()),
            "POST-OK"
        );
        let (_, _, headers2) = host.last_post.lock().unwrap().clone().unwrap();
        assert_eq!(headers2.get("H").map(String::as_str), Some("1"));
    }

    #[test]
    fn shell_read_env_hosts() {
        let host = FakeHost::arc();
        assert_eq!(
            execute(
                &tool_from("// @tool t\nshell(\"ls\")"),
                serde_json::json!({}),
                host.clone()
            ),
            "SHELL-OK"
        );
        assert_eq!(
            execute(
                &tool_from("// @tool t\nread_file(\"a\")"),
                serde_json::json!({}),
                host.clone()
            ),
            "READ-OK"
        );
        assert_eq!(
            execute(
                &tool_from("// @tool t\nenv_var(\"A\")"),
                serde_json::json!({}),
                host.clone()
            ),
            "ENV-OK"
        );
        assert_eq!(
            execute(
                &tool_from("// @tool t\nwrite_file(\"out.txt\", \"body\")"),
                serde_json::json!({}),
                host
            ),
            "WROTE:out.txt=body"
        );
    }

    // ── pure host functions ──

    #[test]
    fn parse_and_to_json_roundtrip() {
        let host = FakeHost::arc();
        let tool = tool_from("// @tool t\nlet d = parse_json(\"{\\\"a\\\": 1}\"); to_json(d)");
        let out = execute(&tool, serde_json::json!({}), host);
        assert_eq!(out, "{\"a\":1}");
    }

    #[test]
    fn parse_json_invalid_errors() {
        let tool = tool_from("// @tool t\nparse_json(\"not json\")");
        let out = execute(&tool, serde_json::json!({}), FakeHost::arc());
        assert!(out.contains("parse_json"));
    }

    #[test]
    fn parse_json_result_used_as_value() {
        // parse_json returns a Dynamic map; access a field, return it (string).
        let tool = tool_from("// @tool t\nlet d = parse_json(\"{\\\"k\\\": \\\"v\\\"}\"); d.k");
        let out = execute(&tool, serde_json::json!({}), FakeHost::arc());
        assert_eq!(out, "v");
    }

    #[test]
    fn encode_uri_encodes_reserved_and_passes_unreserved() {
        let tool = tool_from("// @tool t\nencode_uri(\"a b&c-_.~\")");
        let out = execute(&tool, serde_json::json!({}), FakeHost::arc());
        assert_eq!(out, "a%20b%26c-_.~");
    }

    #[test]
    fn to_json_fn_direct_success_and_failure() {
        // Direct calls give clean coverage attribution for the named helper,
        // independent of rhai's generic `register_fn` wrapper.
        let mut map = Map::new();
        map.insert("a".into(), Dynamic::from(1_i64));
        assert_eq!(to_json_fn(&Dynamic::from_map(map)).unwrap(), "{\"a\":1}");
        // A function pointer has no JSON representation → Err.
        let engine = Engine::new();
        let fnptr: Dynamic = engine.eval("|| 1").unwrap();
        assert!(to_json_fn(&fnptr).is_err());
    }

    #[test]
    fn parse_json_fn_direct_success_and_failure() {
        let d = parse_json_fn("{\"k\": \"v\"}").unwrap();
        assert!(d.is_map());
        assert!(parse_json_fn("not json").is_err());
    }

    #[test]
    fn encode_uri_non_ascii() {
        // '€' (U+20AC) is 3 UTF-8 bytes E2 82 AC.
        assert_eq!(percent_encode("€"), "%E2%82%AC");
    }

    #[test]
    fn hex_digit_covers_both_arms() {
        assert_eq!(hex_digit(9), '9');
        assert_eq!(hex_digit(15), 'F');
        assert_eq!(hex_digit(0), '0');
    }

    #[test]
    fn headers_from_map_stringifies_values() {
        let mut m = Map::new();
        m.insert("n".into(), Dynamic::from(42_i64));
        let headers = headers_from_map(&m);
        assert_eq!(headers.get("n").map(String::as_str), Some("42"));
    }

    #[test]
    fn html_to_text_full_pipeline() {
        let html = "<html><head><style>.a{color:red}</style></head>\
            <body><h1>Tit&amp;le</h1><script>var x=1<2;</script>\
            <p>Hello&nbsp;world &#39;quoted&#39; &#x2014; done.</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Tit&le"), "entity decoded: {text}");
        assert!(
            text.contains("Hello world 'quoted' \u{2014} done."),
            "got: {text}"
        );
        assert!(!text.contains("color:red"), "style content dropped");
        assert!(!text.contains("var x"), "script content dropped");
        assert!(!text.contains('<'), "tags stripped");
    }

    #[test]
    fn strip_element_handles_case_unclosed_and_utf8() {
        // Case-insensitive open + close.
        assert_eq!(strip_element("a<SCRIPT>x</script>b", "script"), "ab");
        // Unclosed element drops the remainder.
        assert_eq!(strip_element("keep<style>rest", "style"), "keep");
        // Non-matching content (incl. multi-byte chars) passes through.
        assert_eq!(strip_element("café < 3", "script"), "café < 3");
    }

    #[test]
    fn strip_tags_edges() {
        assert_eq!(strip_tags("<b>hi</b>").trim(), "hi");
        // '>' outside a tag is kept.
        assert_eq!(strip_tags("2 > 1").trim(), "2 > 1");
        // Unclosed '<' drops the rest.
        assert_eq!(strip_tags("ok <broken").trim(), "ok");
    }

    #[test]
    fn decode_entities_named_numeric_and_unknown() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("&lt;&gt;&quot;&apos;"), "<>\"'");
        assert_eq!(decode_entities("x&nbsp;y"), "x y");
        assert_eq!(decode_entities("&mdash;&ndash;&hellip;"), "\u{2014}–…");
        assert_eq!(decode_entities("&#65;&#x42;&#X43;"), "ABC");
        // Unknown entity kept verbatim.
        assert_eq!(decode_entities("&bogus;"), "&bogus;");
        // No terminating ';' within the window → '&' kept, scan continues.
        assert_eq!(decode_entities("a & b"), "a & b");
        // Invalid numeric → kept verbatim.
        assert_eq!(decode_entities("&#zz;"), "&#zz;"); // decimal parse Err
        assert_eq!(decode_entities("&#xZZ;"), "&#xZZ;"); // hex from_str_radix Err
        assert_eq!(decode_entities("&#x110000;"), "&#x110000;"); // hex out of range (from_u32 None)
        assert_eq!(decode_entities("&#99999999;"), "&#99999999;"); // decimal out of range (from_u32 None)
        // No ampersand at all.
        assert_eq!(decode_entities("plain"), "plain");
    }

    #[test]
    fn decode_entities_survives_multibyte_after_an_ampersand() {
        // Regression for issue #109: the entity-scan window is a byte count, so
        // a bare '&' followed by multi-byte text can slice mid-character and
        // panic ("byte index 12 is not a char boundary") - inside a Rhai native
        // fn, which aborts the daemon. Every one of these is a real shape from
        // fetched HTML.
        assert_eq!(decode_entities("&日本語日本"), "&日本語日本");
        assert_eq!(decode_entities("R&D 日本語です"), "R&D 日本語です");
        assert_eq!(decode_entities("&🎉🎉🎉🎉"), "&🎉🎉🎉🎉");
        assert_eq!(
            decode_entities("&\u{2014}\u{2014}\u{2014}\u{2014}"),
            "&\u{2014}\u{2014}\u{2014}\u{2014}"
        );
        // A real entity immediately followed by multi-byte text still decodes.
        assert_eq!(decode_entities("&amp;日本語"), "&日本語");
        // Trailing '&' at the very end of the string (window == 1).
        assert_eq!(decode_entities("tail&"), "tail&");
        // Issue #115 re-reported the same crash with a flag emoji. '&' plus nine
        // ASCII bytes puts the four-byte regional indicator at bytes 10..14, so
        // a fixed byte-12 window cuts straight through it. Pinned verbatim so
        // the reported input, not just an equivalent one, stays covered.
        assert_eq!(decode_entities("&abcdefghi🇸"), "&abcdefghi🇸");
    }

    #[test]
    fn collapse_whitespace_runs_and_trims() {
        assert_eq!(collapse_whitespace("  a \n\t b  "), "a b");
        assert_eq!(collapse_whitespace(""), "");
    }
}
