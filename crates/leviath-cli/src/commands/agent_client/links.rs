//! `resource_link` blocks in a prompt: a host names a file by URI rather
//! than sending its bytes. A `file://` link that resolves inside the
//! session's working directory is read here and rides the prompt as a part,
//! the way an attached file would; any other link stays a name in the text,
//! since the agent has no way to read a host's file by reference.

use std::path::{Path, PathBuf};

use leviath_agent_client::ContentBlock;
use leviath_core::media::{InboundPart, MediaType};

/// The most a linked file may weigh before it is left as a name: the
/// daemon's default part ceiling, so the bridge never reads what the run
/// would refuse.
const MAX_LINK_BYTES: u64 = 32 * 1024 * 1024;

/// The parts a prompt's `file://` links yield, and the URIs that yielded
/// them, so the text can say which links were followed.
pub(super) fn link_parts(blocks: &[ContentBlock], cwd: &str) -> (Vec<InboundPart>, Vec<String>) {
    link_parts_under(blocks, cwd, MAX_LINK_BYTES)
}

/// [`link_parts`] with the size ceiling in hand.
fn link_parts_under(
    blocks: &[ContentBlock],
    cwd: &str,
    cap: u64,
) -> (Vec<InboundPart>, Vec<String>) {
    let mut parts: Vec<InboundPart> = Vec::new();
    let mut fetched: Vec<String> = Vec::new();
    let Ok(root) = Path::new(cwd).canonicalize() else {
        return (parts, fetched);
    };
    for block in blocks.iter().filter(|b| b.kind == "resource_link") {
        let Some(uri) = block.uri.as_deref() else {
            continue;
        };
        let Some((path, bytes)) = read_inside(uri, &root, cap) else {
            continue;
        };
        // The host's name for it, or the file's own.
        let name = block
            .name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });
        let mut part = InboundPart::from_bytes(name, bytes);
        part.media_type = block
            .mime_type
            .as_deref()
            .and_then(|m| MediaType::parse(m).ok());
        parts.push(part);
        fetched.push(uri.to_string());
    }
    (parts, fetched)
}

/// The file a `file://` URI names and its bytes, when the file sits under
/// `root`, is not empty and weighs at most `cap`. Anything else (another
/// scheme, a URI that is not one, a missing file, a directory, a path that
/// escapes the root) is `None`, and the link stays a name.
fn read_inside(uri: &str, root: &Path, cap: u64) -> Option<(PathBuf, Vec<u8>)> {
    let url = url::Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    let path = url.to_file_path().ok()?.canonicalize().ok()?;
    if !path.starts_with(root) {
        return None;
    }
    // A directory, or a file over the ceiling, is left as a name.
    std::fs::metadata(&path)
        .ok()
        .filter(|m| m.is_file() && m.len() <= cap)?;
    match std::fs::read(&path) {
        Ok(bytes) if !bytes.is_empty() => Some((path, bytes)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::result::export::file_url;

    fn link(uri: &str, name: &str, mime: &str) -> ContentBlock {
        ContentBlock::resource_link(uri, name, mime)
    }

    /// A link inside the working directory is read and named as the host
    /// named it; the file's own name serves when the host gave none.
    #[test]
    fn a_link_inside_the_working_directory_becomes_a_part() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub dir")).unwrap();
        let inside = dir.path().join("sub dir").join("plan.pdf");
        std::fs::write(&inside, b"%PDF-1.7 plan").unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let mut unnamed = link(&file_url(&inside), "", "not a type");
        unnamed.name = None;
        let blocks = vec![
            ContentBlock::text("read it"),
            link(&file_url(&inside), "the plan", "application/pdf"),
            unnamed,
        ];
        let (parts, fetched) = link_parts(&blocks, &cwd);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[0].name, "the plan");
        assert_eq!(parts[0].data, b"%PDF-1.7 plan");
        assert_eq!(
            parts[0].media_type.as_ref().map(|t| t.as_str()),
            Some("application/pdf")
        );
        assert_eq!(parts[1].name, "plan.pdf");
        assert!(
            parts[1].media_type.is_none(),
            "an unparsable type is left to the daemon"
        );
        assert_eq!(fetched, vec![file_url(&inside); 2]);
    }

    /// Every way a link stays a name: no URI, another scheme, not a URI at
    /// all, a host in the URI (no file path on Unix), a path with no drive
    /// (no file path on Windows), a missing file, a directory, an empty
    /// file, one over the ceiling, one outside the working directory, and a
    /// working directory that is not there.
    #[test]
    fn a_link_that_cannot_be_read_stays_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let big = dir.path().join("big.bin");
        std::fs::write(&big, [7u8; 64]).unwrap();
        let empty = dir.path().join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        let outside = elsewhere.path().join("secret.txt");
        std::fs::write(&outside, b"no").unwrap();
        let mut no_uri = link("file:///x", "x", "");
        no_uri.uri = None;
        let blocks = vec![
            no_uri,
            link("https://example.com/a.png", "a.png", "image/png"),
            link("::not a uri::", "x", ""),
            link("file://server/share/a.txt", "a.txt", ""),
            link("file:///nodrive/a.txt", "a.txt", ""),
            link(&file_url(&dir.path().join("missing.txt")), "m", ""),
            link(&file_url(dir.path()), "dir", ""),
            link(&file_url(&empty), "empty", ""),
            link(&file_url(&big), "big", ""),
            link(&file_url(&outside), "secret", ""),
        ];
        let (parts, fetched) = link_parts_under(&blocks, &cwd, 16);
        assert!(parts.is_empty(), "{parts:?}");
        assert!(fetched.is_empty());
        // Under the real ceiling the small file is fine.
        let (parts, _) = link_parts(&[link(&file_url(&big), "big", "")], &cwd);
        assert_eq!(parts.len(), 1);
        // No working directory, nothing read.
        let gone = dir.path().join("gone").to_string_lossy().to_string();
        let (parts, _) = link_parts(&[link(&file_url(&big), "big", "")], &gone);
        assert!(parts.is_empty());
    }
}
