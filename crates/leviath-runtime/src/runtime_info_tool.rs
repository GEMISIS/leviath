//! The `runtime_info` tool: a run reporting on itself.
//!
//! Everything it answers - which stage is running, how many iterations are
//! left, which model is being called, how much of the context window is spent,
//! whether anyone is there to answer a question - exists only in the live
//! world. So unlike the other environment tools it is advertised by
//! `leviath-tools` and handled here, alongside the `context_*` tools, for the
//! same reason they are: the async tool lane cannot reach the ECS window.
//!
//! The field that changes behaviour most is `unattended`. An agent that cannot
//! tell will call `ask_user_text` in a run with nobody watching and park there
//! until something times it out; one that can tell will write its question into
//! its output instead.

use crate::components::ContextWindow;

/// Whether `name` is the runtime self-report tool.
pub fn is_runtime_info_tool(name: &str) -> bool {
    name == "runtime_info"
}

/// Everything `runtime_info` reports, gathered by the caller from the world.
///
/// A struct rather than a dozen arguments because the caller assembles it from
/// six different components, and a positional list of that length is one
/// transposition away from reporting the stage index as the iteration count.
pub struct RuntimeFacts<'a> {
    /// The `lev` version this run is executing under.
    pub version: &'a str,
    /// The run's id, as it appears in `lev ps` and on disk.
    pub run_id: Option<&'a str>,
    /// The blueprint's name.
    pub agent: Option<&'a str>,
    /// The stage running right now.
    pub stage: &'a str,
    /// Which stage this is, and how many the blueprint has.
    pub stage_index: Option<(usize, usize)>,
    /// Inferences run in this stage, and the stage's own cap when it sets one.
    pub stage_iterations: (usize, Option<usize>),
    /// Inferences run across the whole run so far.
    pub total_iterations: usize,
    /// The provider and model being called for this stage.
    pub provider_model: (&'a str, &'a str),
    /// The tool names this stage advertises.
    pub tools: Vec<&'a str>,
    /// Whether the run was launched with nobody available to answer a prompt.
    pub unattended: bool,
    /// The run's working directory.
    pub workdir: Option<&'a str>,
}

/// Render the facts as the object the model reads.
///
/// Pure over [`RuntimeFacts`] so every field can be asserted without building a
/// world: the caller's job is to gather, this one's is to shape.
pub fn describe_runtime(facts: &RuntimeFacts<'_>, window: &ContextWindow) -> serde_json::Value {
    let used = window.calculate_tokens();
    let (stage_iterations, stage_max) = facts.stage_iterations;
    let (provider, model) = facts.provider_model;
    serde_json::json!({
        "leviath_version": facts.version,
        "run_id": facts.run_id,
        "agent": facts.agent,
        "working_directory": facts.workdir,
        "stage": {
            "name": facts.stage,
            "index": facts.stage_index.map(|(i, _)| i),
            "of": facts.stage_index.map(|(_, n)| n),
            "iterations": stage_iterations,
            "max_iterations": stage_max,
            // The number the model can actually act on. Absent when the stage
            // sets no cap, which is a different thing from "none left".
            "iterations_remaining": stage_max.map(|m| m.saturating_sub(stage_iterations)),
        },
        "total_iterations": facts.total_iterations,
        "provider": provider,
        "model": model,
        "context": {
            "used_tokens": used,
            "window_tokens": window.max_tokens,
            "remaining_tokens": window.max_tokens.saturating_sub(used),
        },
        "available_tools": facts.tools,
        "unattended": facts.unattended,
        // Spelled out rather than left for the model to infer from the flag,
        // because the inference is the part that goes wrong: an agent reads
        // `unattended: true` and still asks, having no rule attached to it.
        "interaction": match facts.unattended {
            true => "This run is unattended: nobody will answer ask_user_text, \
                     ask_user_choice, ask_user_confirm or present_for_review. \
                     Decide for yourself and record the assumption in your output.",
            false => "A person is available to answer ask_user_text, \
                      ask_user_choice, ask_user_confirm and present_for_review.",
        },
    })
}

/// Answer one `runtime_info` call.
pub fn handle_runtime_info(facts: &RuntimeFacts<'_>, window: &ContextWindow) -> String {
    let value = describe_runtime(facts, window);
    // Indented to match what the `leviath-tools` environment tools return, so
    // the family reads the same however it is dispatched. Serializing a `Value`
    // of primitives cannot fail, and a fallback rendering would be a branch
    // nothing could reach or test.
    serde_json::to_string_pretty(&value).expect("a Value of primitives always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>() -> RuntimeFacts<'a> {
        RuntimeFacts {
            version: "0.4.0",
            run_id: Some("deep-researcher-1787093177-d5005a3cfc4d"),
            agent: Some("deep-researcher"),
            stage: "gather",
            stage_index: Some((0, 7)),
            stage_iterations: (3, Some(20)),
            total_iterations: 11,
            provider_model: ("anthropic", "claude-sonnet-5"),
            tools: vec!["web_search", "current_time"],
            unattended: false,
            workdir: Some("/work"),
        }
    }

    #[test]
    fn the_report_names_the_run_the_stage_and_the_model() {
        let window = ContextWindow::new(200_000);
        let v = describe_runtime(&facts(), &window);
        assert_eq!(v["leviath_version"], "0.4.0");
        assert_eq!(v["run_id"], "deep-researcher-1787093177-d5005a3cfc4d");
        assert_eq!(v["agent"], "deep-researcher");
        assert_eq!(v["working_directory"], "/work");
        assert_eq!(v["stage"]["name"], "gather");
        assert_eq!(v["stage"]["index"], 0);
        assert_eq!(v["stage"]["of"], 7);
        assert_eq!(v["stage"]["iterations"], 3);
        assert_eq!(v["stage"]["max_iterations"], 20);
        assert_eq!(v["total_iterations"], 11);
        assert_eq!(v["provider"], "anthropic");
        assert_eq!(v["model"], "claude-sonnet-5");
        assert_eq!(
            v["available_tools"],
            serde_json::json!(["web_search", "current_time"])
        );
        assert_eq!(v["context"]["window_tokens"], 200_000);
    }

    /// The number a model can act on is what is *left*, and deriving it from
    /// two others is exactly the arithmetic it gets wrong under pressure.
    #[test]
    fn the_remaining_iterations_are_computed_rather_than_left_to_be_inferred() {
        let window = ContextWindow::new(1000);
        let v = describe_runtime(&facts(), &window);
        assert_eq!(v["stage"]["iterations_remaining"], 17);

        // Past the cap floors at zero rather than wrapping to a huge number,
        // which is what a plain subtraction on `usize` would produce.
        let mut over = facts();
        over.stage_iterations = (25, Some(20));
        let v = describe_runtime(&over, &window);
        assert_eq!(v["stage"]["iterations_remaining"], 0);

        // An uncapped stage reports null: "no limit" is not "none left".
        let mut uncapped = facts();
        uncapped.stage_iterations = (3, None);
        let v = describe_runtime(&uncapped, &window);
        assert_eq!(v["stage"]["max_iterations"], serde_json::Value::Null);
        assert_eq!(v["stage"]["iterations_remaining"], serde_json::Value::Null);
    }

    /// The whole point of the field: an unattended run says so in words the
    /// model can act on, naming the tools that will not be answered.
    #[test]
    fn an_unattended_run_is_told_not_to_ask() {
        let window = ContextWindow::new(1000);
        let mut f = facts();
        f.unattended = true;
        let v = describe_runtime(&f, &window);
        assert_eq!(v["unattended"], true);
        let guidance = v["interaction"].as_str().expect("guidance is a string");
        assert!(guidance.contains("unattended"));
        for tool in [
            "ask_user_text",
            "ask_user_choice",
            "ask_user_confirm",
            "present_for_review",
        ] {
            assert!(guidance.contains(tool), "{tool} must be named");
        }

        // And an attended run says the opposite rather than staying silent.
        let attended = describe_runtime(&facts(), &window);
        assert_eq!(attended["unattended"], false);
        let guidance = attended["interaction"].as_str().unwrap();
        assert!(guidance.contains("A person is available"));
    }

    /// A bare world - `lev run` with no daemon behind it - has no run metadata
    /// and no stage catalogue. Every such field reports null rather than the
    /// call failing: an agent asking what it is running under should not be
    /// punished for running somewhere thin.
    #[test]
    fn a_run_without_metadata_reports_nulls_rather_than_refusing() {
        let window = ContextWindow::new(1000);
        let bare = RuntimeFacts {
            version: "0.4.0",
            run_id: None,
            agent: None,
            stage: "main",
            stage_index: None,
            stage_iterations: (0, None),
            total_iterations: 0,
            provider_model: ("", ""),
            tools: Vec::new(),
            unattended: false,
            workdir: None,
        };
        let v = describe_runtime(&bare, &window);
        assert_eq!(v["run_id"], serde_json::Value::Null);
        assert_eq!(v["agent"], serde_json::Value::Null);
        assert_eq!(v["working_directory"], serde_json::Value::Null);
        assert_eq!(v["stage"]["index"], serde_json::Value::Null);
        assert_eq!(v["stage"]["of"], serde_json::Value::Null);
        assert_eq!(v["available_tools"], serde_json::json!([]));
    }

    #[test]
    fn context_use_is_reported_against_the_window_it_is_measured_in() {
        let mut window = ContextWindow::new(500);
        let before = describe_runtime(&facts(), &window);
        assert_eq!(before["context"]["used_tokens"], 0);
        assert_eq!(before["context"]["remaining_tokens"], 500);

        window.add_region(leviath_core::Region::new(
            "notes".to_string(),
            leviath_core::RegionKind::Pinned,
            400,
        ));
        window.replace_region("notes", "some content".to_string(), 120);
        let after = describe_runtime(&facts(), &window);
        assert_eq!(after["context"]["used_tokens"], 120);
        assert_eq!(after["context"]["remaining_tokens"], 380);
    }

    /// An overfull window floors at zero rather than wrapping, the same way the
    /// iteration remainder does.
    #[test]
    fn an_overfull_window_reports_nothing_remaining() {
        let mut window = ContextWindow::new(100);
        window.add_region(leviath_core::Region::new(
            "notes".to_string(),
            leviath_core::RegionKind::Pinned,
            1000,
        ));
        window.replace_region("notes", "lots".to_string(), 250);
        let v = describe_runtime(&facts(), &window);
        assert_eq!(v["context"]["remaining_tokens"], 0);
    }

    #[test]
    fn the_handler_answers_only_to_its_own_name_and_renders_indented_json() {
        assert!(is_runtime_info_tool("runtime_info"));
        assert!(!is_runtime_info_tool("current_time"));
        assert!(!is_runtime_info_tool("context_read"));

        let window = ContextWindow::new(1000);
        let text = handle_runtime_info(&facts(), &window);
        assert!(text.contains("\n  \""), "rendered indented: {text}");
        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("the handler returns JSON");
        assert_eq!(parsed["stage"]["name"], "gather");
    }
}
