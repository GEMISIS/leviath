//! The sub-agent tools' definitions: starting, checking and reading child
//! runs, and finding out what a run may be started from.
//!
//! These are advertised to the model but run against the daemon's world, not
//! the built-in executor, so their handlers live in the CLI. Each schema here
//! is the shape that handler reads, key for key: the handler refuses any key
//! not listed here, and the CLI's tests hold the two to each other by
//! validating the same examples against both.

use super::*;

/// What `spawn_agent` and `validate_spawn` take: a spawn request, and whether
/// to wait for the run. One schema for both, so a call that validates is the
/// call that spawns.
fn spawn_request_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "source": {
                "description": "What the sub-agent runs: {\"blueprint\": \"<name>\"} for an installed blueprint (or {\"blueprint\": {\"name\": \"<name>\", \"digest\": \"<hex>\"}} to pin one revision), or {\"graph\": {...}} for a whole run graph you write yourself. `spawn_schema` with part \"RunGraph\" shows the graph's shape.",
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "blueprint": {
                                "oneOf": [
                                    {
                                        "type": "string",
                                        "description": "An installed blueprint's name, as `describe_blueprint` takes it."
                                    },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "name": { "type": "string", "description": "The blueprint's name." },
                                            "digest": { "type": "string", "description": "The revision to run, as a hex digest." }
                                        },
                                        "required": ["name"],
                                        "additionalProperties": false
                                    }
                                ]
                            }
                        },
                        "required": ["blueprint"],
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": {
                            "graph": {
                                "type": "object",
                                "description": "A whole run graph: stages, edges, regions and declared inputs. Needs the `spawn_raw_graph` permission, which asks a person by default."
                            }
                        },
                        "required": ["graph"],
                        "additionalProperties": false
                    }
                ]
            },
            "inputs": {
                "type": "object",
                "description": "Values for the inputs the blueprint or graph declares, by name: usually {\"task\": \"<the work, in full>\"}. Each is checked against its declared type, and a name nothing declares is refused. `describe_blueprint` lists a blueprint's inputs."
            },
            "wait": {
                "type": "boolean",
                "description": "Block until the sub-agent finishes and return its result. Default false: return its id at once, to check on later.",
                "default": false
            },
            "max_child_depth": {
                "type": "integer",
                "minimum": 0,
                "maximum": 255,
                "description": "How deep the sub-agent's own tree of children may grow. Never deeper than yours allows."
            },
            "allow": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Tools the sub-agent may call without asking. Left out, it is the list this run was given; either way, never a tool this run was not given."
            },
            "output": {
                "type": "object",
                "description": "The shape to ask the sub-agent's final answer in, over its blueprint's. Passed to the sub-agent as an instruction.",
                "properties": {
                    "format": { "type": "string", "description": "Any label: markdown, json, a mime type, your own." },
                    "instructions": { "type": "string", "description": "Guidance about that shape." },
                    "example": { "type": "string", "description": "An example answer." },
                    "schema": { "type": "object", "description": "A JSON Schema the answer must meet." }
                },
                "additionalProperties": false
            },
            "parts": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Stored parts of this run to hand the sub-agent, each by name or by a prefix of its sha256 (as listed in your context). Each lands in the child's context as a typed part."
            }
        },
        "required": ["source"],
        "additionalProperties": false
    })
}

/// A blueprint named the way `spawn_agent`'s `source.blueprint` names one.
fn blueprint_ref_schema(description: &str) -> Value {
    json!({
        "description": description,
        "oneOf": [
            { "type": "string" },
            {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "digest": { "type": "string" }
                },
                "required": ["name"],
                "additionalProperties": false
            }
        ]
    })
}

/// A schema for a tool that takes one run id.
fn agent_id_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": { "type": "string", "description": description }
        },
        "required": ["agent_id"]
    })
}

impl BuiltinTools {
    /// Tool definitions for the sub-agent tools, in [`SUBAGENT_TOOLS`] order.
    ///
    /// These are advertised to the LLM but executed externally (by the CLI's
    /// tool registry) since they need the daemon's world.
    pub fn subagent_tool_defs() -> Vec<Tool> {
        vec![
            Tool {
                name: "spawn_agent".to_string(),
                description: "Start a sub-agent: an installed blueprint, or a run graph you write. Returns its id at once, or with wait=true its result once it finishes. A refused spawn comes back as a numbered list, one problem per line with where it is and how to fix it; fix them all and call again. `validate_spawn` checks the same arguments without starting anything.".to_string(),
                parameters: spawn_request_schema(),
            },
            Tool {
                name: "check_agent".to_string(),
                description: "Check the status of a sub-agent. Returns its current status and result if complete. Non-blocking.".to_string(),
                parameters: agent_id_schema("ID of the agent to check"),
            },
            Tool {
                name: "wait_for_agent".to_string(),
                description: "Block until a sub-agent completes, then return its final result.".to_string(),
                parameters: agent_id_schema("ID of the agent to wait for"),
            },
            Tool {
                name: "send_to_agent".to_string(),
                description: "Send a message to a running sub-agent's context window.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "ID of the target agent"
                        },
                        "message": {
                            "type": "string",
                            "description": "Message content to send"
                        },
                        "target_region": {
                            "type": "string",
                            "description": "Context region to deliver to (default: conversation)"
                        }
                    },
                    "required": ["agent_id", "message"]
                }),
            },
            Tool {
                name: "kill_agent".to_string(),
                description: "Kill a sub-agent and all its descendants. Sets their cancellation tokens and marks them as cancelled.".to_string(),
                parameters: agent_id_schema("ID of the agent to kill"),
            },
            Tool {
                name: "spawn_schema".to_string(),
                description: "Show the JSON Schema of a spawn request, one part at a time. Without `part` it returns the request's top level and the names of every part; ask for a part by name (RunGraph, StageDef, RegionDef, InputDecl, InputType, ...) to see its fields. `spawn_agent`'s `source.graph` is a RunGraph.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "part": {
                            "type": "string",
                            "description": "The part to show, by the name the top level lists. Leave out for the top level."
                        }
                    },
                    "additionalProperties": false
                }),
            },
            Tool {
                name: "describe_blueprint".to_string(),
                description: "Describe an installed blueprint: what it is for, its stages, and the inputs it declares, with each input's type, whether it is required and its default. Call this before spawning a blueprint, to know what `inputs` it takes.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "blueprint": blueprint_ref_schema("The installed blueprint: its name, or {name, digest}.")
                    },
                    "required": ["blueprint"],
                    "additionalProperties": false
                }),
            },
            Tool {
                name: "validate_spawn".to_string(),
                description: "Check a spawn without starting anything: takes exactly what `spawn_agent` takes, and returns either a summary of the run it would start (its stages, models and tools, and the checked inputs) or every problem at once as a numbered list, each with where it is and how to fix it. `wait` has no effect here.".to_string(),
                parameters: spawn_request_schema(),
            },
            Tool {
                name: "run_history".to_string(),
                description: "Read a run you started (or this run itself): what it was started as, where it is now, and every stage change it made. Runs outside your own tree cannot be read. `view` picks what comes back: \"summary\" (the default) for the run in brief, \"state\" for its whole state, now or at step `at`, and \"transitions\" for each edge it took.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "run_id": {
                            "type": "string",
                            "description": "The run's id, as spawn_agent returned it."
                        },
                        "view": {
                            "type": "string",
                            "enum": ["summary", "state", "transitions"],
                            "description": "What to return. Default \"summary\"."
                        },
                        "at": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "The step to read the state at, with view \"state\". Leave out for now."
                        }
                    },
                    "required": ["run_id"],
                    "additionalProperties": false
                }),
            },
        ]
    }

    /// Names of the sub-agent tools, in [`SUBAGENT_TOOLS`] order.
    pub fn subagent_tool_names() -> Vec<String> {
        SUBAGENT_TOOLS.iter().map(|s| s.to_string()).collect()
    }
}
