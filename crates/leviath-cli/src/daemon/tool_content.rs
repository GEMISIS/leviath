//! What a tool result or a person's answer becomes in a region: its text,
//! then every file that came with it as a stored part, typed by the run's
//! registry and named as the sender named it. Shared by the MCP path and
//! the interaction path of the tool lane, which differ only in where the
//! bytes came from.

use leviath_core::region::EntryContent;

/// An MCP result as the region will hold it: the server's text, then every
/// binary block it returned as a stored part. A failed call is text alone,
/// prefixed as the seed reader expects; a binary block that cannot be stored
/// (no store, over the ceiling) is described in the text instead of dropped.
pub(super) fn mcp_content(
    tool: &str,
    result: anyhow::Result<leviath_mcp::execution::ExecutionResult>,
    media: Option<&leviath_tools::ToolMedia>,
) -> EntryContent {
    let blobs = match &result {
        Ok(r) if r.success => r.blobs.clone(),
        _ => Vec::new(),
    };
    let text = super::seed_tool::mcp_text(result);
    let attached = blobs
        .into_iter()
        .map(|blob| Attached {
            declared: Some(blob.media_type.to_string()),
            name: blob.name,
            data: blob.bytes,
            deliver: None,
        })
        .collect();
    with_attached(tool, text, attached, media)
}

/// A person's answer as the region will hold it: the text, then every file
/// attached to it as a stored part, typed by the registry (the sender's
/// declaration first) and named as the sender named it.
pub(super) fn answer_content(
    tool: &str,
    text: String,
    attached: Vec<leviath_core::media::InboundPart>,
    media: Option<&leviath_tools::ToolMedia>,
) -> EntryContent {
    let attached = attached
        .into_iter()
        .map(|part| Attached {
            declared: part.media_type.map(|t| t.to_string()),
            name: Some(part.name),
            data: part.data,
            deliver: part.deliver,
        })
        .collect();
    with_attached(tool, text, attached, media)
}

/// Bytes on their way into a region beside some text: an MCP block, or a
/// file a person attached to an answer.
pub(super) struct Attached {
    /// The type the sender declared, when it did; the registry sniffs one
    /// otherwise, and corrects a declaration it cannot parse.
    pub(super) declared: Option<String>,
    /// The name the sender gave the bytes, when it did.
    pub(super) name: Option<String>,
    /// The bytes.
    pub(super) data: Vec<u8>,
    /// How the part should reach a model, when the sender had a preference.
    pub(super) deliver: Option<leviath_core::media::Delivery>,
}

/// `text`, then each of `attached` as a stored part. Nothing to attach is
/// text alone; a file that cannot be stored (no store, over the ceiling)
/// is described in the text instead of dropped.
pub(super) fn with_attached(
    tool: &str,
    text: String,
    attached: Vec<Attached>,
    media: Option<&leviath_tools::ToolMedia>,
) -> EntryContent {
    if attached.is_empty() {
        return text.into();
    }
    let mut parts = vec![leviath_core::media::Part::text(text)];
    for (i, item) in attached.into_iter().enumerate() {
        let size = leviath_core::media::human_size(item.data.len() as u64);
        let Some(media) = media else {
            let what = match (&item.declared, &item.name) {
                (Some(declared), _) => format!("{declared} block of {size}"),
                (None, Some(name)) => format!("'{name}' ({size})"),
                (None, None) => format!("a file of {size}"),
            };
            parts.push(leviath_core::media::Part::text(format!(
                "[{what} dropped: this run has no blob store]"
            )));
            continue;
        };
        let media_type = media.type_of(item.declared.as_deref(), item.name.as_deref(), &item.data);
        let name = item
            .name
            .unwrap_or_else(|| media.name_for(&format!("{tool}-{}", i + 1), &media_type));
        let blob = leviath_core::media::Blob::new(media_type, item.data).named(name);
        match media.store(blob) {
            Ok(part) => parts.push(match item.deliver {
                Some(deliver) => part.delivered(deliver),
                None => part,
            }),
            Err(e) => parts.push(leviath_core::media::Part::text(format!(
                "[block dropped: {e}]"
            ))),
        }
    }
    EntryContent::from_parts(parts)
}
