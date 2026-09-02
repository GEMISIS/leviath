//! Tool definitions: the schemas advertised to the model.

use super::*;

/// The tool names routed to the sub-agent handler (they run against the daemon's
/// agent engine, not the builtin/MCP executors). One list, shared by the CLI's
/// dispatch routing and the runtime's crash-replay synthesis, so the two can't
/// drift.
pub const SUBAGENT_TOOLS: &[&str] = &[
    "spawn_agent",
    "check_agent",
    "wait_for_agent",
    "send_to_agent",
    "kill_agent",
];

/// Whether `name` is a sub-agent tool.
pub fn is_subagent_tool(name: &str) -> bool {
    SUBAGENT_TOOLS.contains(&name)
}

/// The `shell` tool's description, naming the shell this host actually resolved
/// instead of listing every platform's and leaving the model to guess which one
/// it got. Pure over the shell so both wordings are testable on any platform.
pub(crate) fn shell_tool_description(shell: &str) -> String {
    format!(
        "Execute a shell command in the working directory. On this machine commands run \
         through `{shell}`, so write them in its syntax. Use this for build commands, \
         running tests, installing dependencies, or other shell operations. Has a \
         60-second timeout."
    )
}

/// The `submit_output` tool's description, built from the output shape resolved
/// for the stage rather than fixed at compile time.
///
/// This is the whole mechanism by which an arbitrary format works. There is no
/// per-format code anywhere in the engine; what makes a model produce a2ui, or a
/// house schema, or a format invented after this function was written, is that
/// the format label, the author's instructions, and a literal example all arrive
/// here and go straight to the model. `described` is
/// [`leviath_core::describe_spec`]'s rendering of the resolved spec, and is
/// empty when nothing was declared.
pub fn submit_output_description(described: &str) -> String {
    let base = "Submit your final answer for this run. This is the value the caller receives - a \
                person reading the run, a parent agent, the API. Nothing else you write is \
                returned to them, so put the answer itself here rather than a pointer to it. \
                Call this once, when your work is done; calling it again replaces what you \
                submitted.\n\nYour answer is one response, so it cannot hold a large dataset or a \
                very long document. Write those to files as you go, then name them in \
                `artifacts` and describe them here.";
    match described.is_empty() {
        true => base.to_string(),
        false => format!("{base}\n\n{described}"),
    }
}

/// What an agent is told about fanning out.
///
/// Says the three things that go wrong. One: each item is all its worker gets,
/// because a worker is a separate agent with its own clean context - a reference
/// to "the topic above" reaches nobody. Two: put all the work in one call, since
/// the engine paces the concurrency itself and a second call would only wait for
/// the first. Three: an empty list is a real answer, which matters most on a
/// stage the run has entered before, where the honest reply is that the work is
/// already done.
const FAN_OUT_DESCRIPTION: &str = "Run many sub-agents at once, one per item, and get their results back \
     together. Use it when the work splits into parts that do not depend on each \
     other - separate topics to research, separate files to change, separate \
     questions to answer.\n\nEach worker is a separate agent with a clean \
     context window: it never sees this conversation, so everything it needs has \
     to be inside its own `context`. Name each item precisely enough that two \
     workers cannot end up doing the same thing.\n\nPut all the work in ONE \
     call, however many items that is - they are paced for you, and a second \
     call would just wait for the first to finish. This call blocks until every \
     worker is done; their results come back as its result.\n\nIf there is \
     nothing to hand out, call it with an empty `items` array. That is a valid \
     answer and the run moves on. Do not say so in prose instead.\n\nFor a \
     single sub-agent, use `spawn_agent` rather than a fan-out of one.";

/// [`shell_tool_description`] for the resolved shell, computed once.
///
/// `detect_shell` reads `$SHELL` and probes the filesystem on Unix, and
/// `tool_defs` runs on every request, so the answer is cached. The shell cannot
/// change under a running process in any way this would need to notice.
fn resolved_shell_description() -> &'static str {
    static DESCRIPTION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DESCRIPTION.get_or_init(|| shell_tool_description(BuiltinTools::detect_shell().0))
}

impl BuiltinTools {
    /// All tool definitions to advertise to the LLM, minus any whose required
    /// platform capabilities aren't provided by the current platform.
    pub fn tool_defs(&self) -> Vec<Tool> {
        let mut defs = vec![
            Tool {
                name: "read_file".to_string(),
                description: "Read the complete contents of a file. Use this to examine existing code, configurations, or data files before making changes.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file, relative to the working directory"
                        }
                    },
                    "required": ["path"]
                }),
            },
            Tool {
                name: "write_file".to_string(),
                description: "Write content to a file, creating it (and any parent directories) if necessary. Use this to create new files or completely replace existing file content.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file, relative to the working directory"
                        },
                        "content": {
                            "type": "string",
                            "description": "The full content to write to the file"
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
            Tool {
                name: "edit_file".to_string(),
                description: "Replace an exact string in an existing file. The old_str must appear exactly once in the file. Use this for targeted edits rather than rewriting entire files.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file, relative to the working directory"
                        },
                        "old_str": {
                            "type": "string",
                            "description": "The exact string to replace. Must appear exactly once in the file."
                        },
                        "new_str": {
                            "type": "string",
                            "description": "The string to replace old_str with"
                        }
                    },
                    "required": ["path", "old_str", "new_str"]
                }),
            },
            Tool {
                name: "list_dir".to_string(),
                description: "List the contents of a directory. Use this to explore the file structure before reading or writing files.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the directory, relative to the working directory. Defaults to the working directory root if omitted."
                        }
                    },
                    "required": []
                }),
            },
            Tool {
                name: "read_files".to_string(),
                description: "Read multiple files at once. Returns the contents of all requested files in a single response, separated by file path headers. More efficient than calling read_file repeatedly. Use this when you need to read several files (e.g. after list_dir).".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Array of file paths relative to the working directory"
                        }
                    },
                    "required": ["paths"]
                }),
            },
            Tool {
                name: "shell".to_string(),
                description: resolved_shell_description().to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute"
                        }
                    },
                    "required": ["command"]
                }),
            },
            Tool {
                name: "present_for_review".to_string(),
                description: "Present a document, plan, or report to the user for review. The agent run will pause and the dashboard will display the document prominently. Use this when you want the user to read and approve something before you continue - for example, a technical design, an implementation plan, or a summary report. The user can provide feedback or simply acknowledge to continue.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Short title for the review prompt shown to the user (e.g. 'Implementation Plan Ready for Review')"
                        },
                        "markdown": {
                            "type": "string",
                            "description": "The markdown document to present to the user. Supports headings, lists, code blocks, and mermaid diagrams."
                        }
                    },
                    "required": ["title", "markdown"]
                }),
            },
            Tool {
                name: "ask_user_text".to_string(),
                description: "Ask the user a free-form question and wait for their written answer. The run pauses until they respond. Use this when you need clarification, missing information, or a specific detail only the user knows - decide for yourself when this is necessary; don't ask about things you can figure out on your own.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The question to ask the user"
                        }
                    },
                    "required": ["prompt"]
                }),
            },
            Tool {
                name: "ask_user_choice".to_string(),
                description: "Ask the user to pick one option from a list and wait for their answer. The run pauses until they respond. Use this when you have a small number of distinct paths forward and want the user to decide which one, rather than guessing yourself.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The question to ask the user"
                        },
                        "options": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "At least two options for the user to choose from"
                        }
                    },
                    "required": ["prompt", "options"]
                }),
            },
            Tool {
                name: "ask_user_confirm".to_string(),
                description: "Ask the user a yes/no question and wait for their answer. The run pauses until they respond. Use this for a quick go/no-go decision before doing something significant or hard to undo.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The yes/no question to ask the user"
                        }
                    },
                    "required": ["prompt"]
                }),
            },
            Tool {
                name: "edit_document".to_string(),
                description: "Present a document to the user in an editable field pre-filled with its current text, and wait for them to edit it directly. The run pauses until they submit. Use this when the user wants to modify content themselves (e.g. tweak a plan or draft) rather than describe changes for you to make. Pass the current full text as `content`; the returned text is the user's edited version, which you should adopt as authoritative.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The current full document text to present for editing"
                        },
                        "prompt": {
                            "type": "string",
                            "description": "Optional instruction shown above the editable field"
                        }
                    },
                    "required": ["content"]
                }),
            },
            Tool {
                name: "context_write".to_string(),
                description: "Store or update content in a named section of your context window. This content will be included in your system prompt on subsequent turns, making it available for reference. Use this to save analysis, plans, notes, or structured information. If a key is provided and an entry with that key already exists, it will be replaced with the new content.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Name of the context window section (e.g. 'architecture', 'plan')"
                        },
                        "key": {
                            "type": "string",
                            "description": "Key for the entry. Replaces existing entry with the same key."
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to store"
                        }
                    },
                    "required": ["region", "content"]
                }),
            },
            Tool {
                name: "todo_add".to_string(),
                description: "Add an item to a checklist region. Returns the item's id, which todo_done and todo_note take. Use this for work you have identified but not finished, so that what is left is tracked rather than remembered.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Name of the checklist region (e.g. 'todos')"
                        },
                        "item": {
                            "type": "string",
                            "description": "What needs doing, in one line"
                        }
                    },
                    "required": ["region", "item"]
                }),
            },
            Tool {
                name: "todo_done".to_string(),
                description: "Mark a checklist item finished, by the id todo_add returned. Items you have finished must be ticked off: a stage can be held until its checklist has no open items.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Name of the checklist region"
                        },
                        "id": {
                            "type": "integer",
                            "description": "The item's id, as returned by todo_add"
                        }
                    },
                    "required": ["region", "id"]
                }),
            },
            Tool {
                name: "todo_note".to_string(),
                description: "Record a note against a checklist item without closing it - what you tried, what blocked you, what it depends on.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Name of the checklist region"
                        },
                        "id": {
                            "type": "integer",
                            "description": "The item's id"
                        },
                        "note": {
                            "type": "string",
                            "description": "The note to record"
                        }
                    },
                    "required": ["region", "id", "note"]
                }),
            },
            Tool {
                name: "context_append".to_string(),
                description: "Add content to an existing section of your context window without replacing what's already there.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Name of the context window section"
                        },
                        "key": {
                            "type": "string",
                            "description": "Optional name for this entry. Naming it lets you release it later with context_delete once you are done with it - worth doing for anything bulky you expect to finish with, like a fetched source you plan to distill."
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to append"
                        }
                    },
                    "required": ["region", "content"]
                }),
            },
            Tool {
                name: "context_read".to_string(),
                description: "Read what's currently stored in a section of your context window. If no key is specified and the section contains keyed entries, returns a summary of all keys and their sizes.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Name of the context window section to read"
                        },
                        "key": {
                            "type": "string",
                            "description": "Key of a specific entry to read"
                        }
                    },
                    "required": ["region"]
                }),
            },
            Tool {
                name: "context_delete".to_string(),
                description: "Release an entry you are finished with from a section of your context window, freeing its tokens. Use this when you have distilled what matters out of something bulky and no longer need the original - a source you have summarized, a file you have extracted from. Name the entry by 'key' if you gave it one, by 'index' as shown in context_list, or drop the oldest few with 'oldest'.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Name of the context window section"
                        },
                        "key": {
                            "type": "string",
                            "description": "Key of the entry to release, if it has one"
                        },
                        "index": {
                            "type": "integer",
                            "description": "Position of the entry to release, counting from 0 as the oldest, as shown by context_list"
                        },
                        "oldest": {
                            "type": "integer",
                            "description": "Release this many of the oldest entries. Use when you need room and the oldest material is the least useful."
                        }
                    },
                    "required": ["region"]
                }),
            },
            Tool {
                name: "context_list".to_string(),
                description: "List available sections of your context window with their current usage - section names, token counts, and number of entries. Use this to see what's available and what you've already stored.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Optional region name to list keys for"
                        }
                    },
                    "required": []
                }),
            },
            Tool {
                // The shape lives in the description, not the arguments, so
                // that a stage asking for a2ui and one asking for markdown
                // advertise the same schema. Nothing here parses `content`.
                name: crate::SUBMIT_OUTPUT_TOOL.to_string(),
                description: submit_output_description(""),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "Your final answer, in full."
                        },
                        "artifacts": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Files you produced that the caller should read, as paths relative to the working directory. Use this for anything too large to put in the answer: a dataset, a long report, a generated file. Name the file here rather than only mentioning it in prose."
                        }
                    },
                    "required": ["content"]
                }),
            },
            Tool {
                // The single entry point to the fan-out engine. A `fan_out`
                // stage grants it as sugar; any other stage can grant it
                // directly and fan out mid-work.
                name: crate::FAN_OUT_TOOL.to_string(),
                description: FAN_OUT_DESCRIPTION.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent": {
                            "type": "string",
                            "description": "Name of the installed agent to run for every item. Omit only inside a fan_out stage, which names its worker in the blueprint."
                        },
                        "items": {
                            "type": "array",
                            "description": "One entry per unit of work. They all run at the same time. An empty array means there is nothing to hand out, which is a valid answer.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": {
                                        "type": "string",
                                        "description": "Short slug identifying this item. Labels its worker in the merged report, so make it distinct and readable."
                                    },
                                    "context": {
                                        "type": "object",
                                        "description": "Everything the worker gets. It runs as a separate agent with a clean context and never sees yours, so this has to stand alone."
                                    }
                                },
                                "required": ["id", "context"]
                            }
                        },
                        "max_workers": {
                            "type": "integer",
                            "description": "How many run at once. Optional; the rest queue and start as slots free up."
                        }
                    },
                    "required": ["items"]
                }),
            },
            Tool {
                name: "current_time".to_string(),
                description: "Get the current date and time, in UTC and in this machine's local timezone. Your training data has a cutoff date; this does not. Call this before reasoning about anything current, recent, upcoming, or dated - what year it is, how old something is, whether a release has happened, or what counts as recent news. Do not assume today's date from memory.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            Tool {
                name: "system_info".to_string(),
                description: "Describe the machine this agent runs on: operating system and version, CPU architecture, core count, hostname, the path separator and line ending convention, and free disk space in the working directory. Use this before writing platform-specific paths or commands.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            Tool {
                name: "locale_info".to_string(),
                description: "Report the user's language and region (for example en-US), as the operating system has it configured. Use this to decide what language to write in, and how to format dates, numbers and currency for this user.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            Tool {
                name: "environment_info".to_string(),
                description: "Report the working directory, the home, temporary, config and data directories for this platform, the entries on PATH, and the environment variables this agent may see. Credential-shaped variables are named but their values are withheld, so you can tell a variable is set without seeing its secret.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            Tool {
                name: "which_command".to_string(),
                description: "Check whether a program is installed and report where it lives, the way `which` or `where` would. Looks the name up on PATH without running anything. Use this before writing a shell command that depends on a tool being present.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The program name to look up, such as \"git\" or \"python3\""
                        }
                    },
                    "required": ["command"]
                }),
            },
            Tool {
                name: "runtime_info".to_string(),
                description: "Report this run's own state: the agent and stage running now, which iteration this is and the limit, the model and provider in use, how much of the context window is spent, and whether anyone is available to answer a question. Check whether the run is unattended before using ask_user_text, ask_user_choice, ask_user_confirm or present_for_review - unattended, those have nobody to answer them.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            Tool {
                name: "install_tool".to_string(),
                description: "Compile a Rhai tool script and install it into the global tools directory so every future agent run can call it. Refuses a script that does not compile, lacks `// @tool <name>` or `// @description`, or collides with an existing tool name. Use for repeatable mechanical steps, never for judgement.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "The tool's name. Must equal the script's `// @tool` directive and be a plain file stem: letters, digits, '.', '_' or '-' only"
                        },
                        "source": {
                            "type": "string",
                            "description": "The complete .rhai source, starting with `// @tool <name>` and `// @description <text>`, then `// @param <name> <type> <required|optional> \"<description>\"` per parameter and an optional `// @requires <network|shell|filesystem>`. Arguments arrive in `params`; the script's value is the tool result"
                        },
                        "overwrite": {
                            "type": "boolean",
                            "description": "Replace an existing script of the same name (default false)"
                        }
                    },
                    "required": ["name", "source"]
                }),
            },
        ];
        defs.retain(|t| self.available(&t.name));
        defs
    }

    /// Tool definitions for sub-agent management tools.
    ///
    /// These are advertised to the LLM but executed externally (by the CLI's
    /// tool registry) since they require access to the AgentEngine.
    pub fn subagent_tool_defs() -> Vec<Tool> {
        vec![
            Tool {
                name: "spawn_agent".to_string(),
                description: "Spawn a sub-agent from a blueprint to work on a task. Returns the new agent's ID. If wait=true, blocks until the sub-agent completes and returns its result.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "blueprint": {
                            "type": "string",
                            "description": "Name of the agent blueprint to spawn"
                        },
                        "task": {
                            "type": "string",
                            "description": "Task prompt for the sub-agent"
                        },
                        "wait": {
                            "type": "boolean",
                            "description": "If true, block until the sub-agent completes and return its result. Default: false",
                            "default": false
                        },
                        "seed_context": {
                            "type": "string",
                            "description": "Optional initial context to inject into the sub-agent's first Pinned region"
                        },
                        "max_child_depth": {
                            "type": "integer",
                            "description": "Optional max depth for the sub-agent's own children"
                        },
                        "output_format": {
                            "type": "string",
                            "description": "Optional shape to ask the sub-agent for its final answer in, overriding its blueprint's. Any label works (markdown, json, xml, a media type, your own); it is passed to the sub-agent, not interpreted here."
                        },
                        "output_instructions": {
                            "type": "string",
                            "description": "Optional extra guidance about that shape, passed to the sub-agent alongside output_format."
                        }
                    },
                    "required": ["blueprint", "task"]
                }),
            },
            Tool {
                name: "check_agent".to_string(),
                description: "Check the status of a sub-agent. Returns its current status and result if complete. Non-blocking.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "ID of the agent to check"
                        }
                    },
                    "required": ["agent_id"]
                }),
            },
            Tool {
                name: "wait_for_agent".to_string(),
                description: "Block until a sub-agent completes, then return its final result.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "ID of the agent to wait for"
                        }
                    },
                    "required": ["agent_id"]
                }),
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
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "ID of the agent to kill"
                        }
                    },
                    "required": ["agent_id"]
                }),
            },
        ]
    }

    /// Names of sub-agent tools.
    pub fn subagent_tool_names() -> Vec<String> {
        vec![
            "spawn_agent".to_string(),
            "check_agent".to_string(),
            "wait_for_agent".to_string(),
            "send_to_agent".to_string(),
            "kill_agent".to_string(),
        ]
    }

    /// Names of all built-in tools, including every alias in [`TOOL_ALIASES`].
    ///
    /// Aliases are included so tool-call dispatch recognizes a call arriving
    /// under an alias name as a built-in; the canonical names are what get
    /// advertised to the model.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = [
            "read_file",
            "read_files",
            "write_file",
            "edit_file",
            "list_dir",
            "shell",
            "present_for_review",
            "ask_user_text",
            "ask_user_choice",
            "ask_user_confirm",
            "edit_document",
            "context_write",
            "context_append",
            "context_read",
            "context_delete",
            "context_list",
            "todo_add",
            "todo_done",
            "todo_note",
            "current_time",
            "system_info",
            "locale_info",
            "environment_info",
            "which_command",
            "runtime_info",
            "install_tool",
            crate::SUBMIT_OUTPUT_TOOL,
            crate::FAN_OUT_TOOL,
        ]
        .iter()
        // Drop any canonical built-in the current platform can't provide, so a
        // filtered-out tool (e.g. `shell` without `ProcessSpawn`) isn't even
        // recognized as a built-in on dispatch.
        .filter(|n| self.available(n))
        .map(|s| s.to_string())
        .collect();
        // Include an alias only when its canonical target survived filtering
        // (so `bash` disappears together with `shell`).
        names.extend(
            TOOL_ALIASES
                .iter()
                .filter(|(_, canonical)| self.available(canonical))
                .map(|(alias, _)| alias.to_string()),
        );
        names
    }
}
