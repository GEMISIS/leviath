//! Leviath's extension methods: `_leviath/spawn` and `_leviath/validate_spawn`.
//!
//! Both take a whole spawn request as their params, the same JSON the HTTP
//! API and `lev run --request` take, so a host can name a blueprint and give
//! it typed inputs, or send a graph of its own. The wire shapes live in
//! [`leviath_agent_client::extensions`].
//!
//! ## Who may ask for what
//!
//! A host reaches `lev agent-client` over stdio, so it runs on this machine.
//! It is still not the operator: it relays what its user, or a model, wrote,
//! and a request in an extension's params is that relayed text. So these
//! methods treat every request the way the HTTP API treats one from the
//! network:
//!
//! - a blueprint read from a directory (`source.blueprint_file`, or a
//!   fan-out worker named by directory) is refused; name an installed
//!   blueprint or send the graph itself;
//! - an unattended run needs the operator to have started the server with
//!   `--yolo`, and a tool allowed outright needs `--yolo` or the operator's
//!   own `--allow` for that tool;
//! - `--no-seed-commands` turns a request's seed commands off;
//! - a request that names no workdir works in the directory the server was
//!   started in.
//!
//! What the operator configured stays theirs: the default blueprint that
//! `session/new` runs (`--agent`, or the `agent.toml` in the session's
//! directory) is read from wherever it is, since the operator chose it when
//! they set the host up.
//!
//! A refused request gets every problem in one answer: this server's own
//! refusals and the daemon's, as the `data` of an `invalid params` error.

use leviath_agent_client::extensions::SpawnResult;
use leviath_agent_client::{JsonRpcMessage, error_codes};
use leviath_runtime::control_socket::ControlResponse;
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::launch::Unattended;
use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};

use super::{ActiveSession, AgentClientArgs, Server, SessionRun, new_session_id};

impl Server {
    /// `_leviath/spawn`: start the run a request asks for, in a new session
    /// bound to it. The new session replaces any open one.
    pub(super) async fn on_spawn(
        &mut self,
        id: serde_json::Value,
        params: Option<serde_json::Value>,
    ) {
        let request = match self.admit(params).await {
            Ok(request) => request,
            Err(issues) => return self.reject(id, &issues).await,
        };
        let label = label_of(&request);
        let cwd = request
            .workdir
            .as_deref()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();
        match self.control.spawn(request).await {
            Ok(ControlResponse::Spawned { run_id }) => {
                let session_id = new_session_id(&label);
                self.session = Some(ActiveSession {
                    session_id: session_id.clone(),
                    cwd,
                    run: SessionRun::Running(run_id.clone()),
                });
                let result = SpawnResult { session_id, run_id };
                self.write(&JsonRpcMessage::response(id, &result)).await;
            }
            Ok(ControlResponse::Rejected { issues }) => self.reject(id, &issues).await,
            other => self.daemon_failed(id, other).await,
        }
    }

    /// `_leviath/validate_spawn`: what a request would run, without running
    /// it.
    pub(super) async fn on_validate_spawn(
        &mut self,
        id: serde_json::Value,
        params: Option<serde_json::Value>,
    ) {
        let request = match self.admit(params).await {
            Ok(request) => request,
            Err(issues) => return self.reject(id, &issues).await,
        };
        match self.control.validate_spawn(request).await {
            Ok(ControlResponse::Valid { summary }) => {
                let result = camel_case(&summary);
                self.write(&JsonRpcMessage::response(id, &result)).await;
            }
            Ok(ControlResponse::Rejected { issues }) => self.reject(id, &issues).await,
            other => self.daemon_failed(id, other).await,
        }
    }

    /// The request in `params`, with this server's rules applied, or every
    /// reason it may not run. When this server refuses a request for its own
    /// reasons, the daemon is asked for its issues too, so one answer says
    /// everything; a request naming a directory is not handed on, since the
    /// daemon would read that directory to check it.
    async fn admit(
        &mut self,
        params: Option<serde_json::Value>,
    ) -> Result<SpawnRequest, SpawnIssues> {
        let mut request = read_request(params)?;
        let local = request.check_remote().err();
        let mut issues = operator_issues(&self.args, &mut request);
        if let Some(refusal) = local {
            issues.absorb(refusal);
            return Err(issues);
        }
        if request.workdir.is_none() {
            request.workdir = Some(self.default_cwd.clone().into());
        }
        if issues.is_empty() {
            return Ok(request);
        }
        if let Ok(ControlResponse::Rejected { issues: more }) =
            self.control.validate_spawn(request).await
        {
            issues.absorb(more);
        }
        Err(issues)
    }

    /// Answer a refused request: `invalid params`, with the issues as data.
    async fn reject(&mut self, id: serde_json::Value, issues: &SpawnIssues) {
        self.write(&JsonRpcMessage::error_response_with_data(
            id,
            error_codes::INVALID_PARAMS,
            issues.to_string(),
            issues,
        ))
        .await;
    }

    /// Answer a request the daemon could not: it was unreachable, shutting
    /// down, or replied with something other than an answer.
    async fn daemon_failed(
        &mut self,
        id: serde_json::Value,
        reply: std::io::Result<ControlResponse>,
    ) {
        let why = match reply {
            Ok(ControlResponse::Error { message }) => message,
            Ok(other) => format!("unexpected reply {other:?}"),
            Err(e) => e.to_string(),
        };
        self.write(&JsonRpcMessage::error_response(
            id,
            error_codes::INTERNAL_ERROR,
            format!("the daemon could not answer: {why}"),
        ))
        .await;
    }
}

/// The spawn request in an extension's params. Params that are missing, or
/// that do not read as a request, are one issue at the request's root saying
/// so, with serde's own words (which name the unknown or missing field).
fn read_request(params: Option<serde_json::Value>) -> Result<SpawnRequest, SpawnIssues> {
    let params = params.ok_or_else(|| {
        SpawnIssue::new(
            SpecPath::root(),
            IssueCode::Missing,
            "the params must be a spawn request",
        )
        .hint("send at least {\"source\": {\"blueprint\": {\"name\": \"...\"}}}")
    })?;
    serde_json::from_value(params).map_err(|e| {
        SpawnIssue::new(SpecPath::root(), IssueCode::Invalid, e.to_string())
            .expected("a spawn request")
            .into()
    })
}

/// The refusals the operator's flags make of `request`, which they also
/// adjust: seed commands go off under `--no-seed-commands`.
fn operator_issues(args: &AgentClientArgs, request: &mut SpawnRequest) -> SpawnIssues {
    let mut issues = SpawnIssues::new();
    let launch_at = SpecPath::root().field("launch");
    let operator_yolo = args.yolo.is_some();
    let launch = &mut request.launch;
    if !operator_yolo && launch.unattended != Unattended::Off {
        issues.push(
            SpawnIssue::new(
                launch_at.field("unattended"),
                IssueCode::NotAllowed,
                "this server runs nothing unattended",
            )
            .expected("off")
            .hint("the operator allows unattended runs by starting `lev agent-client --yolo`"),
        );
    }
    for (i, tool) in launch.allow.iter().enumerate() {
        if !operator_yolo && !args.allow.iter().any(|a| a == tool.as_str()) {
            issues.push(
                SpawnIssue::new(
                    launch_at.field("allow").index(i),
                    IssueCode::NotAllowed,
                    format!("this server does not allow '{tool}' outright"),
                )
                .hint("the operator allows a tool with `lev agent-client --allow <tool>`")
                .known(&args.allow),
            );
        }
    }
    if args.no_seed_commands {
        launch.seed_commands = false;
    }
    issues
}

/// A dry run's summary as this protocol spells a result: every key in
/// camelCase, as `_leviath/spawn`'s `{sessionId, runId}` is. The inputs are
/// keyed by the names the blueprint gave them, which keep their spelling.
fn camel_case(summary: &leviath_runtime::spec::summary::SpawnSummary) -> serde_json::Value {
    let mut value = serde_json::to_value(summary).expect("a summary is plain data");
    let inputs = value["inputs"].take();
    let mut value = camel_keys(value);
    value["inputs"] = inputs;
    value
}

/// `value` with every object key in camelCase, all the way down.
fn camel_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => map
            .into_iter()
            .map(|(key, inner)| (camel(&key), camel_keys(inner)))
            .collect(),
        serde_json::Value::Array(items) => items.into_iter().map(camel_keys).collect(),
        other => other,
    }
}

/// `snake_case` as `camelCase`.
fn camel(key: &str) -> String {
    let mut words = key.split('_');
    let first = words.next().unwrap_or_default().to_string();
    words.fold(first, |mut out, word| {
        let mut chars = word.chars();
        out.extend(chars.next().map(|c| c.to_ascii_uppercase()));
        out.push_str(chars.as_str());
        out
    })
}

/// What a session spawned from `request` is called: its blueprint's name, or
/// a raw graph's title.
fn label_of(request: &SpawnRequest) -> String {
    match &request.source {
        SpawnSource::Blueprint(reference) => reference.name.to_string(),
        SpawnSource::Raw(graph) => graph.title.clone().unwrap_or_else(|| "run".to_string()),
        // Refused before a spawn, so never a session's name.
        SpawnSource::BlueprintFile(path) => path.to_string(),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A one-stage graph, as a host sending its own would write it.
    pub(in super::super) fn a_graph() -> leviath_runtime::spec::graph::RunGraph {
        leviath_blueprint::BlueprintFile::parse(
            "[blueprint]\nname = \"sketch\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\n\
             [graph]\nstages = [{ name = \"work\", system_prompt = \"Work.\" }]\n\
             layout = { total_budget_tokens = 1000, regions = [\
             { name = \"conversation\", kind = { kind = \"sliding_window\", max_items = 20 }, budget = 1000 }] }\n",
        )
        .unwrap()
        .run_graph()
    }

    fn named(name: &str) -> SpawnRequest {
        SpawnRequest::new(SpawnSource::Blueprint(
            leviath_runtime::spec::names::BlueprintRef::parse(name).unwrap(),
        ))
    }

    #[test]
    fn params_that_are_not_a_request_are_one_issue_at_the_root() {
        let missing = read_request(None).unwrap_err();
        assert_eq!(missing.0[0].code, IssueCode::Missing);
        assert_eq!(missing.0[0].path, SpecPath::root());
        let typo = read_request(Some(serde_json::json!({
            "source": {"blueprint": {"name": "coder"}},
            "input": {}
        })))
        .unwrap_err();
        assert_eq!(typo.0[0].code, IssueCode::Invalid);
        assert!(
            typo.0[0].message.contains("unknown field `input`"),
            "{typo}"
        );
        assert!(
            read_request(Some(
                serde_json::json!({"source": {"blueprint": {"name": "coder"}}})
            ))
            .is_ok()
        );
    }

    #[test]
    fn the_operator_flags_bound_what_a_request_may_ask_for() {
        use leviath_runtime::spec::names::ToolName;
        let mut request = named("coder");
        request.launch.unattended = Unattended::All;
        request.launch.allow = vec![
            ToolName::new("bash").unwrap(),
            ToolName::new("read_file").unwrap(),
        ];
        let mut args = AgentClientArgs {
            allow: vec!["read_file".to_string()],
            ..Default::default()
        };
        let issues = operator_issues(&args, &mut request.clone());
        let paths: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
        assert_eq!(paths, ["launch.unattended", "launch.allow[0]"]);
        assert_eq!(issues.0[1].known, ["read_file"]);
        // With `--yolo` the operator lets both through, and
        // `--no-seed-commands` turns the request's seed commands off.
        args.yolo = Some(String::new());
        args.no_seed_commands = true;
        assert!(operator_issues(&args, &mut request).is_empty());
        assert!(!request.launch.seed_commands);
    }

    #[test]
    fn a_session_is_named_after_its_blueprint_or_its_graph() {
        assert_eq!(label_of(&named("coder")), "coder");
        let mut graph = a_graph();
        graph.title = Some("sketch".into());
        let raw = SpawnRequest::new(SpawnSource::Raw(Box::new(graph.clone())));
        assert_eq!(label_of(&raw), "sketch");
        graph.title = None;
        let raw = SpawnRequest::new(SpawnSource::Raw(Box::new(graph)));
        assert_eq!(label_of(&raw), "run");
        let dir = std::env::temp_dir();
        let path = leviath_runtime::spec::names::BlueprintPath::new(dir.to_string_lossy()).unwrap();
        assert_eq!(
            label_of(&SpawnRequest::new(SpawnSource::BlueprintFile(path.clone()))),
            path.to_string()
        );
    }
}
