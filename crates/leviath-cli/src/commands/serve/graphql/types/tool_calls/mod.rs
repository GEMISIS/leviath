//! One tool call, typed by the tool it names.
//!
//! A call is an intended operation: a tool and its arguments, paired. Each tool
//! takes one argument shape, so each type here pins its own, and a client asking
//! for `ShellCall` cannot be handed a path where a command belongs. That is the
//! whole reason this is an interface with a type per tool rather than a blob of
//! JSON: a console rendering a run's history knows what it is looking at.
//!
//! A call is not proof anything ran. One attempt to carry it out is an execution,
//! and that is where the outcome lives.
//!
//! Three things keep this honest rather than decorative:
//!
//! - Every typed call also carries `rawArguments`: exactly what the model sent,
//!   untouched. The typed view is a convenience over that, never a replacement,
//!   because a debugger that could only show the tidied version would hide the
//!   malformed call that caused the bug.
//! - A call whose arguments do not fit its tool's shape comes back as
//!   `UntypedToolCall` with a reason, rather than as a typed call with invented
//!   or missing fields.
//! - A test holds every type here against the tool catalog's own declared
//!   schemas, field by field, so a tool that gains an argument cannot leave this
//!   silently behind.

use async_graphql::{Enum, Interface, SimpleObject};

use super::super::scalars::Json;

mod args_context;
mod args_files;
mod args_rest;

#[cfg(test)]
mod tests;

use args_context::{
    ContextAppendArgs, ContextAttachArgs, ContextDeleteArgs, ContextExportArgs, ContextListArgs,
    ContextReadArgs, ContextWriteArgs, TodoAddArgs, TodoDoneArgs, TodoNoteArgs,
};
use args_files::{
    EditFileArgs, InstallGlobalToolArgs, InstallSelfToolArgs, ListDirArgs, ReadFileArgs,
    ReadFilesArgs, ShellArgs, WhichCommandArgs, WriteFileArgs,
};
use args_rest::{
    AskUserChoiceArgs, AskUserConfirmArgs, AskUserTextArgs, CheckAgentArgs, EditDocumentArgs,
    FanOutArgs, KillAgentArgs, PresentForReviewArgs, SendToAgentArgs, SpawnAgentArgs,
    SubmitOutputArgs, WaitForAgentArgs,
};

/// Why a call came back untyped.
///
/// The two are different situations and a client should treat them differently.
/// An MCP or script tool has no type here and never will, which is ordinary. A
/// built-in whose arguments did not fit its own schema is a call that the model
/// got wrong or that a later build changed, and it is worth looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum UntypedCallReason {
    /// This build has no type for this tool: an MCP tool, or a script one.
    NoTypeForThisTool,
    /// The tool is known, and the recorded arguments did not fit its shape.
    ArgumentsDidNotMatch,
}

/// One call whose arguments stay as they were recorded.
#[derive(Debug, SimpleObject)]
pub(crate) struct UntypedToolCall {
    /// Tool name, exactly as the model called it.
    tool_name: String,
    /// What the tool does, where this build knows the tool.
    tool_description: Option<String>,
    /// The arguments as the model sent them, untouched.
    raw_arguments: Json,
    /// Why this call is not one of the typed ones.
    reason: UntypedCallReason,
}

/// Define the typed calls, the interface over them, and the dispatcher.
///
/// One table so the three can never disagree: a tool listed here gets a type, a
/// variant and a dispatch arm at once, and the fields every call shares are
/// written once instead of thirty-five times.
macro_rules! tool_calls {
    (
        with_arguments {
            $($doc:literal, $variant:ident, $call:ident, $args:ty, $tool:literal;)*
        }
        without_arguments {
            $($ndoc:literal, $nvariant:ident, $ncall:ident, $ntool:literal;)*
        }
    ) => {
        $(
            #[doc = $doc]
            #[derive(Debug, SimpleObject)]
            pub(crate) struct $call {
                /// Tool name, exactly as the model called it.
                tool_name: String,
                /// What the tool does, as this build describes it.
                tool_description: Option<String>,
                /// The arguments as the model sent them, untouched. The typed
                /// form beside this is a reading of it, not a replacement.
                raw_arguments: Json,
                /// The call's arguments.
                args: $args,
            }
        )*
        $(
            #[doc = $ndoc]
            #[derive(Debug, SimpleObject)]
            pub(crate) struct $ncall {
                /// Tool name, exactly as the model called it.
                tool_name: String,
                /// What the tool does, as this build describes it.
                tool_description: Option<String>,
                /// The arguments as the model sent them, which for this tool is
                /// an empty object unless the model sent something it should not
                /// have.
                raw_arguments: Json,
            }
        )*

        /// One tool call: the tool plus its arguments, paired.
        ///
        /// Ask for `__typename` to find out which tool it is, then select that
        /// type's `args`. The shared fields answer without knowing the tool.
        #[derive(Debug, Interface)]
        #[graphql(
            field(
                name = "tool_name",
                ty = "&String",
                desc = "Tool name, exactly as the model called it."
            ),
            field(
                name = "tool_description",
                ty = "&Option<String>",
                desc = "What the tool does, where this build knows the tool."
            ),
            field(
                name = "raw_arguments",
                ty = "&Json",
                desc = "The arguments as the model sent them, untouched."
            )
        )]
        pub(crate) enum ToolCall {
            $(
                #[doc = $doc]
                $variant($call),
            )*
            $(
                #[doc = $ndoc]
                $nvariant($ncall),
            )*
            /// A call this build has no type for, or one whose arguments did not
            /// fit the tool's shape.
            Untyped(UntypedToolCall),
        }

        /// The typed form of a recorded call, or nothing when there is none.
        ///
        /// `None` means two different things to the caller, which is why the
        /// caller and not this decides what to say about it: no type for the
        /// tool, or arguments that did not fit.
        fn typed(
            tool: &str,
            description: Option<String>,
            arguments: &serde_json::Value,
        ) -> Option<ToolCall> {
            let known = leviath_tools::canonical_tool_name(tool);
            Some(match known {
                $(
                    $tool => ToolCall::$variant($call {
                        tool_name: tool.to_string(),
                        tool_description: description,
                        raw_arguments: Json(arguments.clone()),
                        args: serde_json::from_value(arguments.clone()).ok()?,
                    }),
                )*
                $(
                    $ntool => ToolCall::$nvariant($ncall {
                        tool_name: tool.to_string(),
                        tool_description: description,
                        raw_arguments: Json(arguments.clone()),
                    }),
                )*
                _ => return None,
            })
        }

        /// Whether this build has a type for `tool`, canonical name or alias.
        fn is_typed(tool: &str) -> bool {
            let known = leviath_tools::canonical_tool_name(tool);
            matches!(known, $($tool)|* | $($ntool)|*)
        }

        /// Every tool this module types, paired with the arguments type that
        /// mirrors its declared schema. Read by the test that holds the two in
        /// step; nothing else needs it.
        #[cfg(test)]
        pub(crate) const TYPED_TOOLS: &[(&str, &str)] = &[
            $(($tool, stringify!($args)),)*
            $(($ntool, ""),)*
        ];
    };
}

tool_calls! {
    with_arguments {
        "One `read_file` call.", ReadFile, ReadFileCall, ReadFileArgs, "read_file";
        "One `write_file` call.", WriteFile, WriteFileCall, WriteFileArgs, "write_file";
        "One `edit_file` call.", EditFile, EditFileCall, EditFileArgs, "edit_file";
        "One `list_dir` call.", ListDir, ListDirCall, ListDirArgs, "list_dir";
        "One `read_files` call.", ReadFiles, ReadFilesCall, ReadFilesArgs, "read_files";
        "One `shell` call.", Shell, ShellCall, ShellArgs, "shell";
        "One `which_command` call.", WhichCommand, WhichCommandCall, WhichCommandArgs, "which_command";
        "One `install_self_tool` call.", InstallSelfTool, InstallSelfToolCall, InstallSelfToolArgs, "install_self_tool";
        "One `install_global_tool` call.", InstallGlobalTool, InstallGlobalToolCall, InstallGlobalToolArgs, "install_global_tool";
        "One `present_for_review` call.", PresentForReview, PresentForReviewCall, PresentForReviewArgs, "present_for_review";
        "One `ask_user_text` call.", AskUserText, AskUserTextCall, AskUserTextArgs, "ask_user_text";
        "One `ask_user_choice` call.", AskUserChoice, AskUserChoiceCall, AskUserChoiceArgs, "ask_user_choice";
        "One `ask_user_confirm` call.", AskUserConfirm, AskUserConfirmCall, AskUserConfirmArgs, "ask_user_confirm";
        "One `edit_document` call.", EditDocument, EditDocumentCall, EditDocumentArgs, "edit_document";
        "One `context_write` call.", ContextWrite, ContextWriteCall, ContextWriteArgs, "context_write";
        "One `context_attach` call.", ContextAttach, ContextAttachCall, ContextAttachArgs, "context_attach";
        "One `context_export` call.", ContextExport, ContextExportCall, ContextExportArgs, "context_export";
        "One `context_append` call.", ContextAppend, ContextAppendCall, ContextAppendArgs, "context_append";
        "One `context_read` call.", ContextRead, ContextReadCall, ContextReadArgs, "context_read";
        "One `context_delete` call.", ContextDelete, ContextDeleteCall, ContextDeleteArgs, "context_delete";
        "One `context_list` call.", ContextList, ContextListCall, ContextListArgs, "context_list";
        "One `todo_add` call.", TodoAdd, TodoAddCall, TodoAddArgs, "todo_add";
        "One `todo_done` call.", TodoDone, TodoDoneCall, TodoDoneArgs, "todo_done";
        "One `todo_note` call.", TodoNote, TodoNoteCall, TodoNoteArgs, "todo_note";
        "One `submit_output` call: the run's own answer.", SubmitOutput, SubmitOutputCall, SubmitOutputArgs, "submit_output";
        "One `fan_out` call.", FanOut, FanOutCall, FanOutArgs, "fan_out";
        "One `spawn_agent` call.", SpawnAgent, SpawnAgentCall, SpawnAgentArgs, "spawn_agent";
        "One `check_agent` call.", CheckAgent, CheckAgentCall, CheckAgentArgs, "check_agent";
        "One `wait_for_agent` call.", WaitForAgent, WaitForAgentCall, WaitForAgentArgs, "wait_for_agent";
        "One `send_to_agent` call.", SendToAgent, SendToAgentCall, SendToAgentArgs, "send_to_agent";
        "One `kill_agent` call.", KillAgent, KillAgentCall, KillAgentArgs, "kill_agent";
    }
    without_arguments {
        "One `current_time` call, which takes nothing.", CurrentTime, CurrentTimeCall, "current_time";
        "One `system_info` call, which takes nothing.", SystemInfo, SystemInfoCall, "system_info";
        "One `locale_info` call, which takes nothing.", LocaleInfo, LocaleInfoCall, "locale_info";
        "One `environment_info` call, which takes nothing.", EnvironmentInfo, EnvironmentInfoCall, "environment_info";
        "One `runtime_info` call, which takes nothing.", RuntimeInfo, RuntimeInfoCall, "runtime_info";
    }
}

/// One recorded call, typed where its tool and arguments allow.
///
/// `arguments` is the text the journal holds, which is what the model sent. Text
/// that is not JSON at all is kept as a string rather than dropped: a torn write
/// or a model that sent a bare word is exactly the sort of thing somebody reading
/// a run's history is trying to find.
pub(crate) fn tool_call(tool: &str, description: Option<String>, arguments: &str) -> ToolCall {
    let raw: serde_json::Value = serde_json::from_str(arguments)
        .unwrap_or_else(|_| serde_json::Value::String(arguments.to_string()));
    from_value(tool, description, raw)
}

/// One call whose arguments are already parsed, which is how a live request
/// carries them.
pub(crate) fn from_value(
    tool: &str,
    description: Option<String>,
    arguments: serde_json::Value,
) -> ToolCall {
    if let Some(call) = typed(tool, description.clone(), &arguments) {
        return call;
    }
    ToolCall::Untyped(UntypedToolCall {
        tool_name: tool.to_string(),
        tool_description: description,
        raw_arguments: Json(arguments),
        reason: match is_typed(tool) {
            true => UntypedCallReason::ArgumentsDidNotMatch,
            false => UntypedCallReason::NoTypeForThisTool,
        },
    })
}
