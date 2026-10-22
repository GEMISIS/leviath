//! Where a tool's result goes, and what the model is told about it.
//!
//! A stage can route tool results out of the conversation and into a
//! region: a file read lands in a `sources` region, a search in `findings`.
//! The provider still needs a `tool_result` paired with each `tool_use`, so
//! the conversation keeps a short pointer in its place, and the pointer has
//! to be true. When the region kept only the start of the result, or none
//! of it, the pointer says so.
//!
//! This module is that logic with no runtime attached: pick the region by
//! tool name, cap the result, store it with a truncated fallback, and word
//! the pointer. `leviath-runtime` routes every tool result through it, so a
//! program that calls a model itself and routes with these functions shows
//! its model exactly the text a Leviath run would.
//!
//! ```
//! use leviath_core::{Region, RegionKind};
//! use leviath_core::tool_routing::{ToolRouting, route_into, Stored};
//!
//! let routing = ToolRouting {
//!     default_region: "conversation".into(),
//!     tool_regions: [("read_file".to_string(), "sources".to_string())].into(),
//!     keep_results: true,
//!     ..ToolRouting::default()
//! };
//! assert_eq!(routing.region_for("read_file", |n| n), "sources");
//!
//! let mut sources = Region::new("sources".into(), RegionKind::Temporary, 1_000);
//! let routed = route_into(&mut sources, "sources", "fn main() {}".into(), 4, false);
//! assert_eq!(routed.stored, Stored::Whole);
//! assert!(routed.pointer.starts_with("[output (4 tokens) is in your context under the 'sources' heading"));
//! ```

use std::collections::BTreeMap;

use crate::error::Error;
use crate::region::{EntryContent, Region};

/// The region a result lands in when nothing routes it elsewhere: the
/// conversation, paired with its `tool_use`.
pub const CONVERSATION: &str = "conversation";

/// The region a result lands in when the stage does not keep results and the
/// window has one by this name.
pub const SCRATCH: &str = "scratch";

/// How many characters of a result its pointer quotes.
pub const PREVIEW_CHARS: usize = 160;

/// A stage's tool-result routing, as plain data.
///
/// The same five settings a blueprint's `tool_routing` block holds. Each map
/// is keyed by tool name; `canonical` on the lookups resolves aliases, so a
/// `bash` entry can match a call to `shell`. Pass `|name| name` when your
/// tools have no aliases.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolRouting {
    /// The region results land in unless a tool has its own.
    pub default_region: String,
    /// Per-tool regions.
    pub tool_regions: BTreeMap<String, String>,
    /// Whether results stay in context after the turn that used them. When
    /// `false` and a [`SCRATCH`] region exists, results go there instead.
    pub keep_results: bool,
    /// The most tokens one result may take.
    pub max_result_tokens: Option<usize>,
    /// Per-tool caps on one result's tokens, over `max_result_tokens`.
    pub tool_max_result_tokens: BTreeMap<String, usize>,
}

impl ToolRouting {
    /// The region `tool`'s results are routed to, before [`route_target`].
    pub fn region_for(&self, tool: &str, canonical: impl Fn(&str) -> &str) -> &str {
        per_tool(
            self.tool_regions
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
            tool,
            canonical,
        )
        .unwrap_or(self.default_region.as_str())
    }

    /// The most tokens one of `tool`'s results may take, when capped.
    pub fn result_cap(&self, tool: &str, canonical: impl Fn(&str) -> &str) -> Option<usize> {
        per_tool(
            self.tool_max_result_tokens
                .iter()
                .map(|(k, v)| (k.as_str(), v)),
            tool,
            canonical,
        )
        .copied()
        .or(self.max_result_tokens)
    }

    /// Where `tool`'s result is stored: its region, or [`SCRATCH`] when
    /// results are not kept and `has_scratch` says the window has one.
    pub fn target(&self, tool: &str, canonical: impl Fn(&str) -> &str, has_scratch: bool) -> &str {
        route_target(
            self.region_for(tool, canonical),
            self.keep_results,
            has_scratch,
        )
    }
}

/// The value a per-tool map holds for `tool`, matching both sides through
/// `canonical` so an alias on either side finds the other.
pub fn per_tool<'m, V: ?Sized>(
    entries: impl IntoIterator<Item = (&'m str, &'m V)>,
    tool: &str,
    canonical: impl Fn(&str) -> &str,
) -> Option<&'m V> {
    let wanted = canonical(tool).to_string();
    entries
        .into_iter()
        .find(|(k, _)| canonical(k) == wanted)
        .map(|(_, v)| v)
}

/// The region a result is stored in, given the region it is routed to:
/// [`SCRATCH`] when results are not kept and there is one, else `routed`.
pub fn route_target(routed: &str, keep_results: bool, has_scratch: bool) -> &str {
    match !keep_results && has_scratch {
        true => SCRATCH,
        false => routed,
    }
}

/// Cut `text` to about `max_tokens` (four characters a token), marking the
/// cut. Unchanged when it already fits.
pub fn cap_result(text: String, max_tokens: usize) -> String {
    let max_chars = max_tokens * 4;
    if text.len() <= max_chars {
        return text;
    }
    let mut capped = crate::truncate_chars(&text, max_chars);
    capped.push_str("\n[...truncated]");
    capped
}

/// What a region did with a result routed into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    /// The result went in as written.
    Whole,
    /// The region took a prefix; `omitted` characters did not fit.
    Truncated {
        /// How many characters were dropped.
        omitted: usize,
    },
    /// The region refused even a truncated entry; only a marker is there.
    Dropped,
    /// The region refused the write for this reason (an `accepts` list, a
    /// script's `on_write`); nothing was stored.
    Rejected(String),
}

/// Somewhere a routed result can be written.
///
/// [`Region`] is one. `leviath-runtime` implements it over its context
/// window, which adds journaling and custom-region scripts to each write.
pub trait ResultSink {
    /// Write `content`, costing `tokens`. An
    /// [`Error::RegionRefusedWrite`] means the region will not take this
    /// content at any size; any other error means it did not fit.
    fn put(&mut self, content: EntryContent, tokens: usize) -> crate::Result<()>;

    /// The tokens of room left, read after a write did not fit.
    fn room(&self) -> usize;
}

impl ResultSink for Region {
    fn put(&mut self, content: EntryContent, tokens: usize) -> crate::Result<()> {
        self.add_entry(content, tokens)
    }

    fn room(&self) -> usize {
        self.max_tokens.saturating_sub(self.current_tokens)
    }
}

/// Store a result, falling back to a truncated copy and then to a marker.
///
/// A tool result must land somewhere, because the provider needs a
/// `tool_result` for every `tool_use`. So a result that does not fit is cut
/// to the room left (with a note of how much was dropped), and one that does
/// not fit even then is replaced by `[result omitted]`. A refusal is
/// reported as is: retrying a smaller copy would be refused the same way.
pub fn store_result(sink: &mut dyn ResultSink, content: EntryContent, tokens: usize) -> Stored {
    match sink.put(content.clone(), tokens) {
        Ok(()) => return Stored::Whole,
        Err(Error::RegionRefusedWrite { reason, .. }) => return Stored::Rejected(reason),
        Err(_) => {}
    }
    let available = sink.room();
    let (truncated, omitted) = if available > 100 {
        let char_budget = (available - 10) * 4;
        let prefix = crate::truncate_chars(&content, char_budget);
        let omitted = content.len().saturating_sub(prefix.len());
        (
            format!("{}... [truncated, {} chars omitted]", prefix, omitted),
            omitted,
        )
    } else {
        (
            "[tool result truncated - context window full]".to_string(),
            content.len(),
        )
    };
    let trunc_tokens = crate::estimate_tokens(&truncated);
    match sink.put(truncated.into(), trunc_tokens) {
        Ok(()) => return Stored::Truncated { omitted },
        // Refused in the truncated shape (a script re-ran over it): still a
        // refusal, not a budget problem.
        Err(Error::RegionRefusedWrite { reason, .. }) => return Stored::Rejected(reason),
        Err(_) => {}
    }
    let _ = sink.put("[result omitted]".into(), 5);
    Stored::Dropped
}

/// The start of `content` a pointer quotes: [`PREVIEW_CHARS`] characters,
/// with an ellipsis when there was more.
pub fn result_preview(content: &str) -> String {
    let preview: String = content.chars().take(PREVIEW_CHARS).collect();
    match content.len() > preview.len() {
        true => format!("{preview}…"),
        false => preview,
    }
}

/// The text left in the conversation for a result stored in `region`.
///
/// It names the region's heading and says no tool call is needed, because
/// the region is already in the prompt: a model told to "read the region"
/// reaches for `read_file` with the region's name. When the stage does not
/// show the region (`hidden`), or the region kept less than the whole
/// result, it says that instead. `preview` is [`result_preview`] of the
/// result.
pub fn pointer_text(
    region: &str,
    tokens: usize,
    preview: &str,
    hidden: bool,
    stored: &Stored,
) -> String {
    match (hidden, stored) {
        (true, _) => format!(
            "[output stored in context region '{region}' ({tokens} tokens), which this stage does not carry - it is kept for a later stage and cannot be read from here. Preview: {preview}]"
        ),
        (false, Stored::Whole) => format!(
            "[output ({tokens} tokens) is in your context under the '{region}' heading - it is already in this prompt, so no tool call is needed to see it. Preview: {preview}]"
        ),
        (false, Stored::Truncated { omitted }) => format!(
            "[output was too large for context region '{region}': the start of it is in this prompt under that heading and {omitted} characters were dropped. Release what you are finished with (context_delete) before fetching more this size. Preview: {preview}]"
        ),
        (false, Stored::Dropped) => format!(
            "[output could NOT be stored - context region '{region}' is full and refused it, so only this preview survives. Release what you are finished with (context_delete) and fetch it again if you still need it. Preview: {preview}]"
        ),
        (false, Stored::Rejected(reason)) => format!(
            "[output was refused by context region '{region}': {reason}. Nothing was stored there; only this preview survives. Preview: {preview}]"
        ),
    }
}

/// A result routed into a region, and the pointer to leave in its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    /// What the region did with it.
    pub stored: Stored,
    /// The text for the conversation's `tool_result`.
    pub pointer: String,
    /// The pointer's token estimate.
    pub pointer_tokens: usize,
}

/// Store a result in `region` through `sink` and word its pointer.
///
/// `tokens` is the result's cost, which the pointer quotes. `hidden` is
/// whether the current stage leaves the region out of the prompt. Write
/// [`Routed::pointer`] into the conversation as the call's `tool_result`.
pub fn route_into(
    sink: &mut dyn ResultSink,
    region: &str,
    content: EntryContent,
    tokens: usize,
    hidden: bool,
) -> Routed {
    let preview = result_preview(&content);
    let stored = store_result(sink, content, tokens);
    let pointer = pointer_text(region, tokens, &preview, hidden, &stored);
    let pointer_tokens = crate::estimate_tokens(&pointer);
    Routed {
        stored,
        pointer,
        pointer_tokens,
    }
}

#[cfg(test)]
#[path = "tool_routing_tests.rs"]
mod tests;
