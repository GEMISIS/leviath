//! How much of one request the next one in the same stage repeats, byte for
//! byte, in the bodies the providers actually send: the part a provider that
//! caches by prefix can read back.
//!
//! Drives the coder's implement stage in miniature through its tool calls:
//! file reads routed into `codebase`, edits into `implementation`, the shell
//! into `test_results`, notes appended to the `decisions`, `changelog` and
//! `errors` sliding windows that sit ahead of the dialogue, and the moves no
//! turn can say (a re-read that replaces a file's copy, a `context_write`, a
//! compaction, a sliding window dropping its oldest entry at `max_items`, a
//! turn of the dialogue written into a sliding window). Each request is built
//! the way dispatch builds it, sent through the real OpenRouter and Anthropic
//! providers to a local server that records the body, and consecutive bodies
//! are compared as the upstream cache sees them: tools, system, then
//! messages, with the `cache_control` markers noted where they landed and then
//! set aside. A Mistral model through OpenRouter is sent the same requests,
//! and no body may put a user turn after a tool result.
//!
//! Run with `--nocapture` for the tables.

use super::inference::{PriorCalls, build_request};
use crate::components::{ContextWindow, ToolCall};
use crate::pipeline::StageInference;
use crate::spec::graph::ToolRoutingDef;
use crate::spec::names::ToolName;
use leviath_core::{EntryKind, EvictionStrategy, Region, RegionKind, Volatility};
use leviath_providers::{InferenceRequest, Provider};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A provider that is never called: requests are only built.
struct Builds;

#[async_trait::async_trait]
impl Provider for Builds {
    async fn infer(
        &self,
        _r: &InferenceRequest,
    ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
        Err(leviath_providers::ProviderError::Other("unused".into()))
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        400_000
    }
    fn name(&self) -> &str {
        "builds"
    }
    fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities {
            max_output_tokens: 8_000,
            ..Default::default()
        }
    }
}

/// Text of about `tokens` tokens, distinct per `seed`.
fn prose(seed: &str, tokens: usize) -> String {
    let line = format!("{seed}: a line of reference text that holds still.\n");
    line.repeat((tokens * 4).div_ceil(line.len()))
}

fn region(name: &str, kind: RegionKind, volatility: Volatility) -> Region {
    let mut region = Region::new(name.to_string(), kind, 200_000);
    region.volatility = volatility;
    region
}

fn sliding(name: &str, max_items: usize) -> Region {
    region(
        name,
        RegionKind::SlidingWindow {
            max_items,
            eviction_strategy: EvictionStrategy::PerItem,
        },
        Volatility::default(),
    )
}

/// The coder's implement stage in miniature, in the coder graph's order:
/// stable pinned context, the regions its tools are routed to, the sliding
/// windows of notes, then the conversation.
fn coder_window() -> ContextWindow {
    let mut w = ContextWindow::new(400_000);
    let compacting = || RegionKind::Compacting {
        threshold_tokens: 1_000_000,
    };
    let mut task = region("task", RegionKind::Pinned, Volatility::Stable);
    task.add_entry(prose("task", 3_000), 3_000).unwrap();
    let mut conventions = region("conventions", RegionKind::Pinned, Volatility::Stable);
    conventions
        .add_entry(prose("conventions", 1_500), 1_500)
        .unwrap();
    let mut plan = region("plan", RegionKind::Pinned, Volatility::default());
    plan.add_entry("1. read the files\n2. edit them\n3. test", 12)
        .unwrap();
    let mut codebase = region("codebase", compacting(), Volatility::Grows);
    codebase.add_entry(prose("README.md", 400), 400).unwrap();
    let mut decisions = sliding("decisions", 10);
    decisions
        .add_typed_entry("Keep the public API unchanged.", 8, EntryKind::Text)
        .unwrap();
    let mut conversation = sliding("conversation", 40);
    conversation
        .add_typed_entry("Implement the plan.", 5, EntryKind::UserMessage)
        .unwrap();
    for r in [
        task,
        conventions,
        plan,
        codebase,
        region(
            "codebase_history",
            RegionKind::CompactHistory {
                source_region: "codebase".to_string(),
            },
            Volatility::Grows,
        ),
        region("implementation", compacting(), Volatility::default()),
        region("test_results", RegionKind::Clearable, Volatility::default()),
        decisions,
        sliding("changelog", 20),
        sliding("errors", 2),
        conversation,
    ] {
        w.add_region(r);
    }
    w
}

/// The implement stage's routing: reads to `codebase`, edits to
/// `implementation`, the shell to `test_results`, results kept.
fn coder_routing() -> ToolRoutingDef {
    let to = |tool: &str, region: &str| {
        (
            ToolName::new(tool).unwrap(),
            crate::test_graph::region_name(region),
        )
    };
    ToolRoutingDef {
        default_region: crate::test_graph::region_name("conversation"),
        tool_regions: [
            to("read_file", "codebase"),
            to("edit_file", "implementation"),
            to("shell", "test_results"),
        ]
        .into(),
        keep_results: true,
        max_result_tokens: None,
        tool_max_result_tokens: Default::default(),
    }
}

fn stage() -> StageInference {
    let tool = |name: &str| leviath_core::Tool {
        name: name.to_string(),
        description: format!("The {name} tool."),
        parameters: serde_json::json!({ "type": "object" }),
    };
    StageInference {
        provider_name: "builds".to_string(),
        model: "m".to_string(),
        tools: vec![
            tool("read_file"),
            tool("edit_file"),
            tool("shell"),
            tool("context_append"),
            tool("context_write"),
        ],
        ..Default::default()
    }
}

/// One model call: the tool it asks for, what that does to the window, and
/// whether the request after it has to show the window afresh.
struct Step {
    tool: &'static str,
    arguments: serde_json::Value,
    /// The tool's result. For a `context_*` call, the tool's own reply.
    result: Option<String>,
    /// Something the runtime does between calls, beside the tool.
    between: Option<fn(&mut ContextWindow)>,
    /// The next request cannot repeat this one.
    moves: bool,
}

fn read(path: &'static str, body: String) -> Step {
    Step {
        tool: "read_file",
        arguments: serde_json::json!({ "path": path }),
        result: Some(body),
        between: None,
        moves: false,
    }
}

fn note(region: &'static str, text: &'static str) -> Step {
    Step {
        tool: "context_append",
        arguments: serde_json::json!({ "region": region, "content": text }),
        result: None,
        between: None,
        moves: false,
    }
}

/// What `collect_compaction` does to a compacting region with a history: the
/// region empties and its summary rolls into the history.
fn compact_codebase(window: &mut ContextWindow) {
    window.get_region_mut("codebase").unwrap().clear();
    window
        .add_to_region(
            "codebase_history",
            "Summary: lib.rs, parse.rs and eval.rs were read.".to_string(),
            12,
        )
        .unwrap();
    window.current_tokens = window.calculate_tokens();
}

fn steps() -> Vec<Step> {
    vec![
        read("src/lib.rs", prose("src/lib.rs", 1_500)),
        read("src/parse.rs", prose("src/parse.rs", 2_000)),
        Step {
            tool: "edit_file",
            arguments: serde_json::json!({ "path": "src/parse.rs", "old": "a", "new": "b" }),
            result: Some("Edited src/parse.rs: 1 replacement.".to_string()),
            between: None,
            moves: false,
        },
        note("changelog", "src/parse.rs: accept trailing commas"),
        // The file changed under the first read: this copy replaces it.
        Step {
            moves: true,
            ..read("src/parse.rs", prose("src/parse.rs (edited)", 2_000))
        },
        Step {
            tool: "shell",
            arguments: serde_json::json!({ "command": "cargo test" }),
            result: Some(prose("test output", 600)),
            between: None,
            moves: false,
        },
        note("decisions", "Trailing commas are accepted, not required."),
        Step {
            tool: "context_write",
            arguments: serde_json::json!({
                "region": "plan",
                "content": "1. read the files\n2. edit them\n3. test\n4. update the docs",
            }),
            result: None,
            between: None,
            moves: true,
        },
        read("src/eval.rs", prose("src/eval.rs", 1_800)),
        // `codebase` passed its threshold: it is compacted before the next call.
        Step {
            tool: "shell",
            arguments: serde_json::json!({ "command": "git diff --stat" }),
            result: Some(prose("diff stat", 300)),
            between: Some(compact_codebase),
            moves: true,
        },
        read("tests/parse.rs", prose("tests/parse.rs", 900)),
        note("changelog", "tests/parse.rs: cover trailing commas"),
        note("errors", "src/parse.rs:12: unused import"),
        note("errors", "src/eval.rs:40: unused variable"),
        // `errors` holds two: the oldest goes.
        Step {
            moves: true,
            ..note("errors", "tests/parse.rs:7: dead code")
        },
        // Text spelled as the assistant's reply is a turn of the dialogue,
        // which cannot be sent later than it was written.
        Step {
            moves: true,
            ..note("changelog", "Assistant: I will tidy the imports next.")
        },
        read("src/main.rs", prose("src/main.rs", 700)),
    ]
}

/// One request per model call through the stage, each call's effect landed
/// the way the tool lane lands it, and whether each request was expected to
/// show the window afresh.
fn drive() -> (Vec<InferenceRequest>, Vec<bool>) {
    let provider: Arc<dyn Provider> = Arc::new(Builds);
    let routing = coder_routing();
    let stage = stage();
    let mut window = coder_window();
    let mut prior = PriorCalls::default();
    let mut requests = Vec::new();
    let mut afresh = vec![true];
    for (index, step) in steps().into_iter().enumerate() {
        let (request, system_hash, block_hashes, frame) = build_request(
            &window,
            None,
            &stage,
            &provider,
            "implement",
            index,
            prior.clone(),
        );
        prior.system_hash = Some(system_hash);
        prior.block_hashes = block_hashes;
        prior.frame = Some(frame);
        requests.push(request);
        afresh.push(step.moves);
        let result = match step.result {
            Some(result) => result,
            None => {
                crate::context_tools::handle_context_tool(step.tool, &step.arguments, &mut window)
            }
        };
        let id = format!("call_{index}");
        super::apply_tool_results(
            &mut window,
            "",
            &[ToolCall {
                tool_id: id.clone(),
                name: step.tool.to_string(),
                arguments: step.arguments,
                thought_signature: None,
            }],
            &[(id, result.into())],
            Some(&routing),
            None,
            None,
        );
        if let Some(between) = step.between {
            between(&mut window);
        }
    }
    let (last, _, _, _) = build_request(
        &window,
        None,
        &stage,
        &provider,
        "implement",
        requests.len(),
        prior,
    );
    requests.push(last);
    (requests, afresh)
}

/// The bodies `provider` sends for `requests`, recorded by a local server that
/// refuses each one after reading it.
async fn sent(
    provider: impl Fn(String) -> Arc<dyn Provider>,
    requests: &[InferenceRequest],
) -> Vec<serde_json::Value> {
    let listener = leviath_testkit::bind_local().await;
    let url = format!("http://{}", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&bodies);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut raw = Vec::new();
            let mut chunk = [0u8; 65_536];
            let (head_end, length) = loop {
                let read = socket.read(&mut chunk).await.unwrap();
                raw.extend_from_slice(&chunk[..read]);
                if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let length = String::from_utf8_lossy(&raw[..end])
                        .lines()
                        .find_map(|l| {
                            let (name, value) = l.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break (end + 4, length);
                }
            };
            while raw.len() < head_end + length {
                let read = socket.read(&mut chunk).await.unwrap();
                raw.extend_from_slice(&chunk[..read]);
            }
            let body: serde_json::Value = serde_json::from_slice(&raw[head_end..]).unwrap();
            recorder.lock().unwrap().push(body);
            let reply = br#"{"error":{"type":"invalid_request_error","message":"recorded"}}"#;
            let head = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(reply).await;
            let _ = socket.shutdown().await;
        }
    });
    let provider = provider(url);
    for request in requests {
        assert!(provider.infer(request).await.is_err());
    }
    let recorded = bodies.lock().unwrap().clone();
    assert_eq!(recorded.len(), requests.len());
    recorded
}

fn openrouter(url: String) -> Arc<dyn Provider> {
    Arc::new(
        leviath_providers::OpenRouterProvider::new(reqwest::Client::new(), "key".to_string())
            .with_base_url(Some(url)),
    )
}

fn anthropic(url: String) -> Arc<dyn Provider> {
    Arc::new(
        leviath_providers::AnthropicProvider::new(reqwest::Client::new(), "key".to_string())
            .with_base_url(Some(url)),
    )
}

/// `requests` for `model`.
fn on(model: &str, requests: &[InferenceRequest]) -> Vec<InferenceRequest> {
    requests
        .iter()
        .cloned()
        .map(|mut r| {
            r.model = model.to_string();
            r
        })
        .collect()
}

/// A body as the pieces the upstream cache compares, in order, each with
/// whether it carried a `cache_control` marker. The marker is then removed
/// and a plain string content written as the one text block it becomes
/// upstream, so a piece differs only where its content does.
fn pieces(body: &serde_json::Value) -> Vec<(String, bool, String)> {
    fn normal(value: &serde_json::Value) -> (bool, serde_json::Value) {
        let mut value = value.clone();
        let mut marked = false;
        if let Some(text) = value.get("content").and_then(|c| c.as_str()) {
            value["content"] = serde_json::json!([{ "type": "text", "text": text }]);
        }
        let mut strip = |v: &mut serde_json::Value| {
            if let Some(object) = v.as_object_mut() {
                marked |= object.remove("cache_control").is_some();
            }
        };
        strip(&mut value);
        if let Some(parts) = value.get_mut("content").and_then(|c| c.as_array_mut()) {
            parts.iter_mut().for_each(&mut strip);
        }
        (marked, value)
    }
    let mut out = vec![(
        "tools".to_string(),
        false,
        body.get("tools").map(|t| t.to_string()).unwrap_or_default(),
    )];
    let system = body.get("system").and_then(|s| s.as_array()).cloned();
    for (index, block) in system.unwrap_or_default().iter().enumerate() {
        let (marked, block) = normal(block);
        out.push((format!("system {index}"), marked, block.to_string()));
    }
    for (index, message) in body["messages"].as_array().unwrap().iter().enumerate() {
        let (marked, message) = normal(message);
        let label = match message["role"].as_str().unwrap() {
            "system" => format!("system {index}"),
            role => format!("message {index} ({role})"),
        };
        out.push((label, marked, message.to_string()));
    }
    out
}

/// Where the second of two consecutive bodies first differs from the first.
#[derive(Debug)]
struct Divergence {
    /// Bytes the two share from the start.
    common: usize,
    /// The bytes of the later body.
    total: usize,
    /// The piece of the later body the first difference falls in.
    at: String,
    /// The pieces of the later body wholly before the difference.
    before: usize,
}

fn divergence(a: &[(String, bool, String)], b: &[(String, bool, String)]) -> Divergence {
    let flat =
        |p: &[(String, bool, String)]| p.iter().map(|(_, _, t)| t.as_str()).collect::<String>();
    let (fa, fb) = (flat(a), flat(b));
    let common = fa
        .bytes()
        .zip(fb.bytes())
        .take_while(|(x, y)| x == y)
        .count();
    let mut offset = 0;
    let mut at = "(end)".to_string();
    let mut before = 0;
    for (label, _, text) in b {
        if common < offset + text.len() {
            at = label.clone();
            break;
        }
        offset += text.len();
        before += 1;
    }
    Divergence {
        common,
        total: fb.len(),
        at,
        before,
    }
}

fn marked(pieces: &[(String, bool, String)]) -> Vec<usize> {
    pieces
        .iter()
        .enumerate()
        .filter(|(_, (_, m, _))| *m)
        .map(|(i, _)| i)
        .collect()
}

/// Compare consecutive bodies, print the table, and hold each request to what
/// it should repeat of the one before.
fn measure(route: &str, bodies: &[serde_json::Value], afresh: &[bool], markers_follow: bool) {
    eprintln!(
        "{route}: call | common/total bytes | first difference | pieces before it | marked pieces"
    );
    for (call, pair) in bodies.windows(2).enumerate() {
        let (a, b) = (pieces(&pair[0]), pieces(&pair[1]));
        let d = divergence(&a, &b);
        eprintln!(
            "{route}: {} -> {} | {}/{} | {} | {} of {} | {:?}",
            call + 1,
            call + 2,
            d.common,
            d.total,
            d.at,
            d.before,
            a.len(),
            marked(&b),
        );
        assert!(
            marked(&b).len() <= 4,
            "{route} call {}: {:?}",
            call + 2,
            marked(&b)
        );
        // Shown afresh: the window as it is, which differs from the previous
        // body before that body's end - in the system blocks for a region
        // that moved, in its turns for a sliding window that did.
        if afresh[call + 1] {
            assert!(
                d.before < a.len(),
                "{route} call {} should show afresh: {d:?}",
                call + 2
            );
            continue;
        }
        assert!(
            d.before >= a.len(),
            "{route} call {} repeats only {} of the {} pieces before it: {d:?}",
            call + 2,
            d.before,
            a.len(),
        );
        // Through OpenRouter the markers are where the runtime put them: the
        // newest message, and the one the previous body marked last, so the
        // entry it wrote is read back.
        let messages_marked = |p: &[(String, bool, String)]| {
            marked(p)
                .into_iter()
                .filter(|&i| p[i].0.starts_with("message"))
                .max()
        };
        if markers_follow && let Some(previous) = messages_marked(&a) {
            assert!(marked(&b).contains(&previous), "{route} call {}", call + 2);
            assert!(
                messages_marked(&b) > Some(previous),
                "{route} call {}",
                call + 2
            );
        }
    }
}

/// Measured before a stage held its prefix: every call's first difference fell
/// inside the system blocks, with nothing of the history ahead of it, and no
/// message carried a marker. Now a request repeats the whole of the one before
/// it, except right after a move no turn can say, which shows the window afresh
/// once and caches again from the call after.
#[tokio::test]
async fn consecutive_bodies_in_a_stage_share_their_prefix() {
    let (requests, afresh) = drive();
    let through_openrouter = sent(openrouter, &on("anthropic/claude-opus-5.5", &requests)).await;
    measure("openrouter", &through_openrouter, &afresh, true);
    let direct = sent(anthropic, &on("claude-opus-5.5", &requests)).await;
    measure("anthropic", &direct, &afresh, false);
    // Mistral refuses a user turn after a tool result: a gain is folded into
    // the result before it, and the prefix still holds.
    let mistral = sent(openrouter, &on("mistralai/devstral-2", &requests)).await;
    measure("mistral", &mistral, &afresh, false);
    for (call, body) in mistral.iter().enumerate() {
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert!(
            !roles.windows(2).any(|w| w == ["tool", "user"]),
            "mistral call {}: {roles:?}",
            call + 1
        );
    }
}

/// Holding the prefix still costs the model nothing: every routed result and
/// every note reaches the request after it, once.
#[test]
fn every_result_and_note_reaches_the_model_once() {
    let (requests, _) = drive();
    for (call, step) in steps().into_iter().enumerate() {
        let next = &requests[call + 1];
        let system: String = next.system.iter().map(|b| b.text.as_str()).collect();
        let messages = serde_json::to_string(&next.messages).unwrap();
        // A note is also in the arguments of the call that wrote it, and is
        // sent without the speaker it was spelled with.
        let (wanted, copies) = match step.result {
            // A result short enough for its pointer to quote whole is in the
            // conversation twice by design.
            Some(result) if result.len() <= leviath_core::tool_routing::PREVIEW_CHARS => continue,
            Some(result) => (result, 1),
            None => {
                let note = step.arguments["content"].as_str().unwrap();
                let said = ["Assistant: ", "User: "]
                    .iter()
                    .find_map(|speaker| note.strip_prefix(speaker))
                    .unwrap_or(note);
                (said.to_string(), 2)
            }
        };
        let quoted = serde_json::to_string(&wanted).unwrap();
        let escaped = quoted.trim_matches('"');
        assert_eq!(
            system.matches(&wanted).count() + messages.matches(escaped).count(),
            copies,
            "call {}: {} reached the model a different number of times than once",
            call + 2,
            step.tool,
        );
    }
}
