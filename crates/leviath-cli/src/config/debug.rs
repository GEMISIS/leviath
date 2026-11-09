//! `Debug` for [`Config`], which holds credentials.
//!
//! Written by hand so that no way of printing a config can print a secret:
//! one `tracing::debug!(?config)`, a `dbg!`, or an error context that formats
//! a struct holding one would otherwise put every key it has into a log. The
//! sections that hold secrets redact themselves (`[providers]`,
//! `[model_providers]`, `[tool_credentials]`, each MCP server), which leaves
//! the one key that sits in the open at the top of the file.

use super::Config;

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Every field is named and none is skipped with `..`: a field added
        // to the config is a compile error here until someone has decided
        // how it prints.
        let Self {
            default_provider,
            providers,
            agent_paths,
            openrouter_api_key,
            ollama_base_url,
            mcp_servers,
            override_model,
            fallback_model,
            model_capabilities,
            model_providers,
            tool_permissions,
            agent_tool_permissions,
            safe_commands,
            agent_safe_commands,
            title,
            request_timeout_secs,
            rate_limits,
            taint_tracking,
            update_check,
            load_dotenv,
            limits,
            batch_tool_hint,
            shell_hint,
            nudge,
            webhook,
            observability,
            sandbox,
            tool_script_permissions,
            security,
            serve,
            mime,
            mime_types,
            agent_read_paths,
            tool_credentials,
            env_fill,
        } = self;
        // Whether the key is set is what a debug line is asking.
        let openrouter_api_key = match openrouter_api_key {
            Some(_) => "<set>",
            None => "<unset>",
        };
        f.debug_struct("Config")
            .field("default_provider", default_provider)
            .field("providers", providers)
            .field("agent_paths", agent_paths)
            .field("openrouter_api_key", &openrouter_api_key)
            .field("ollama_base_url", ollama_base_url)
            .field("mcp_servers", mcp_servers)
            .field("override_model", override_model)
            .field("fallback_model", fallback_model)
            .field("model_capabilities", model_capabilities)
            .field("model_providers", model_providers)
            .field("tool_permissions", tool_permissions)
            .field("agent_tool_permissions", agent_tool_permissions)
            .field("safe_commands", safe_commands)
            .field("agent_safe_commands", agent_safe_commands)
            .field("title", title)
            .field("request_timeout_secs", request_timeout_secs)
            .field("rate_limits", rate_limits)
            .field("taint_tracking", taint_tracking)
            .field("update_check", update_check)
            .field("load_dotenv", load_dotenv)
            .field("limits", limits)
            .field("batch_tool_hint", batch_tool_hint)
            .field("shell_hint", shell_hint)
            .field("nudge", nudge)
            .field("webhook", webhook)
            .field("observability", observability)
            .field("sandbox", sandbox)
            .field("tool_script_permissions", tool_script_permissions)
            .field("security", security)
            .field("serve", serve)
            .field("mime", mime)
            .field("mime_types", mime_types)
            .field("agent_read_paths", agent_read_paths)
            .field("tool_credentials", tool_credentials)
            .field("env_fill", env_fill)
            .finish()
    }
}

#[cfg(test)]
#[path = "debug_tests.rs"]
mod tests;
