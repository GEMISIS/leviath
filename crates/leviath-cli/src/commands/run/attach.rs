//! Files a user attaches from the command line: `--attach`, a `@path` inside
//! the text, or a `--<region> @file` that is not text.
//!
//! All three end as [`InboundPart`]s on the spawn or message request. The
//! file is read here, on the caller's machine, against the caller's working
//! directory; the daemon never resolves a path a client typed.

use std::path::{Path, PathBuf};

use leviath_core::mime::inline_refs::extract;
use leviath_core::mime::{Delivery, InboundPart, MimeRegistry, MimeType};

/// The registry the CLI reads files with.
///
/// The built-in rows only: the daemon types every part again with the
/// user's `[mime_types]` layered in, and a part leaves here untyped unless
/// the user named a type, so the CLI never has to know more than whether a
/// `--<region> @file` reads as text.
pub fn cli_registry() -> MimeRegistry {
    MimeRegistry::builtin()
}

/// Every `--attach` value as a part, in order.
pub fn attach_all(specs: &[String], cwd: &Path) -> anyhow::Result<Vec<InboundPart>> {
    specs.iter().map(|s| parse_attach(s, cwd)).collect()
}

/// Say, on stderr, which `@path` tokens looked like files but named none.
/// They were left as text, which is right for `@channel` and wrong for a
/// typo, and only the user knows which.
pub fn warn_unresolved(unresolved: &[String]) {
    for token in unresolved {
        eprintln!(
            "warning: '@{token}' looks like a file reference but no such file exists; it was left as text"
        );
    }
}

/// Parse one `--attach` value: `path[:region][:type][:text]`.
///
/// The segments after the path are told apart by shape: `type/subtype` is a
/// mime type, `text` is the delivery, anything else is the region. A path
/// with a drive letter (`C:\a.png`) keeps its colon because a one-letter
/// segment followed by a backslash is not a region name.
pub fn parse_attach(spec: &str, cwd: &Path) -> anyhow::Result<InboundPart> {
    let (path, rest) = split_spec(spec);
    if path.is_empty() {
        anyhow::bail!("--attach needs a path: --attach ./mockup.png[:region][:type][:text]");
    }
    let mut part = read_part(&path, cwd)?;
    for segment in rest {
        match segment.as_str() {
            "text" => part.deliver = Some(Delivery::Text),
            "native" => part.deliver = Some(Delivery::Native),
            "stand_in" => part.deliver = Some(Delivery::StandIn),
            s if s.contains('/') => {
                part.mime_type =
                    Some(MimeType::parse(s).map_err(|e| anyhow::anyhow!("--attach {spec}: {e}"))?);
            }
            s if !s.is_empty() => part.region = Some(s.to_string()),
            _ => {}
        }
    }
    Ok(part)
}

/// The path and the trailing segments of an attach spec.
fn split_spec(spec: &str) -> (String, Vec<String>) {
    let mut segments: Vec<String> = spec.split(':').map(str::to_string).collect();
    // `C:\...` or `C:/...`: a one-letter first segment followed by a path
    // separator is a Windows drive, and belongs to the path.
    if segments.len() > 1 && segments[0].len() == 1 && segments[1].starts_with(['\\', '/']) {
        let drive = segments.remove(0);
        segments[0] = format!("{drive}:{}", segments[0]);
    }
    let path = segments.remove(0);
    (path, segments)
}

/// Read a file into an inbound part, named after the file.
pub fn read_part(path: &str, cwd: &Path) -> anyhow::Result<InboundPart> {
    let full = resolve_against(path, cwd);
    let data = std::fs::read(&full)
        .map_err(|e| anyhow::anyhow!("could not read '{}' to attach it: {e}", full.display()))?;
    if data.is_empty() {
        anyhow::bail!(
            "'{}' is empty, so there is nothing to attach",
            full.display()
        );
    }
    // A path that read as a file has a final component.
    let name = full
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // Untyped on purpose: the daemon's registry decides, and a type set here
    // would count as declared and win over it. Only `:type` declares.
    Ok(InboundPart::from_bytes(name, data))
}

/// `path` as given when absolute, else under `cwd`.
fn resolve_against(path: &str, cwd: &Path) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// The parts a paragraph refers to with `@path`, each bound for `region`.
///
/// Every token that names a readable file under `cwd` becomes a part; the
/// text itself is returned with only `\@` unescaped, so the model reads the
/// same `@name` the stand-in carries. Tokens that look like paths but name
/// nothing are reported so the caller can warn.
pub fn inline_parts(
    text: &str,
    region: Option<&str>,
    cwd: &Path,
) -> anyhow::Result<(String, Vec<InboundPart>, Vec<String>)> {
    let extracted = extract(text, &mut |path| resolve_against(path, cwd).is_file());
    let mut parts = Vec::new();
    for r in &extracted.refs {
        let mut part = read_part(&r.path, cwd)?;
        if let Some(t) = &r.mime_type {
            part.mime_type = Some(t.clone());
        }
        part.region = region.map(str::to_string);
        parts.push(part);
    }
    Ok((extracted.text, parts, extracted.unresolved))
}

/// What a `--<region>` flag value carries once read: text, a file's bytes as
/// a part, or both.
#[derive(Debug, Default, PartialEq)]
pub struct RegionInput {
    /// The text to seed the region with, possibly empty.
    pub text: String,
    /// The parts to write into the region.
    pub parts: Vec<InboundPart>,
    /// `@path` tokens that looked like files but named none.
    pub unresolved: Vec<String>,
}

/// Resolve one `--<region>` value. A whole-value `@file` that reads as text
/// seeds the region with that text, as it always has; one that does not is
/// attached as a part. Literal text is scanned for `@path` tokens.
pub fn read_region_input(
    region: &str,
    raw: &str,
    cwd: &Path,
    registry: &MimeRegistry,
) -> anyhow::Result<RegionInput> {
    if let Some(path) = raw.strip_prefix('@')
        && !path.contains(char::is_whitespace)
    {
        let full = resolve_against(path, cwd);
        let data = std::fs::read(&full)
            .map_err(|e| anyhow::anyhow!("Failed to read region file '{}': {}", path, e))?;
        let mime_type = registry.resolve(None, Some(path), &data);
        if registry.info(&mime_type).text
            && let Ok(text) = std::str::from_utf8(&data)
        {
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                anyhow::bail!("Region file '{}' is empty.", path);
            }
            return Ok(RegionInput {
                text: trimmed,
                ..Default::default()
            });
        }
        let name = full
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        return Ok(RegionInput {
            text: String::new(),
            parts: vec![InboundPart::from_bytes(name, data).in_region(region)],
            unresolved: Vec::new(),
        });
    }
    let (text, parts, unresolved) = inline_parts(raw, Some(region), cwd)?;
    Ok(RegionInput {
        text,
        parts,
        unresolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        b"\x89PNG\r\n\x1a\nbody".to_vec()
    }

    #[test]
    fn attach_specs_parse_by_shape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.png"), png()).unwrap();
        std::fs::write(dir.path().join("scene.bin"), [0u8, 1, 2]).unwrap();
        let part = parse_attach("m.png", dir.path()).unwrap();
        assert_eq!(part.name, "m.png");
        assert_eq!(part.mime_type, None);
        assert!(part.region.is_none());
        let part = parse_attach("m.png:mockups", dir.path()).unwrap();
        assert_eq!(part.region.as_deref(), Some("mockups"));
        let part = parse_attach("scene.bin:art:model/gltf-binary:text", dir.path()).unwrap();
        assert_eq!(part.region.as_deref(), Some("art"));
        assert_eq!(
            part.mime_type.as_ref().unwrap().as_str(),
            "model/gltf-binary"
        );
        assert_eq!(part.deliver, Some(Delivery::Text));
        let part = parse_attach("m.png::native", dir.path()).unwrap();
        assert_eq!(part.deliver, Some(Delivery::Native));
        assert!(part.region.is_none());
        let part = parse_attach("m.png:stand_in", dir.path()).unwrap();
        assert_eq!(part.deliver, Some(Delivery::StandIn));
        let abs = dir.path().join("m.png").to_string_lossy().to_string();
        assert!(parse_attach(&abs, Path::new("/nowhere")).is_ok());
        assert!(parse_attach("", dir.path()).is_err());
        assert!(parse_attach("missing.png", dir.path()).is_err());
        assert!(
            parse_attach("m.png:bad type/x", dir.path())
                .unwrap_err()
                .to_string()
                .contains("m.png:bad type/x")
        );
        std::fs::write(dir.path().join("empty.png"), b"").unwrap();
        assert!(parse_attach("empty.png", dir.path()).is_err());
        assert_eq!(
            split_spec("C:\\pics\\a.png:art"),
            ("C:\\pics\\a.png".to_string(), vec!["art".to_string()])
        );
        assert_eq!(split_spec("c:/a.png"), ("c:/a.png".to_string(), vec![]));
    }

    #[test]
    fn inline_references_become_parts_and_the_text_keeps_its_tokens() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hero.png"), png()).unwrap();
        let (text, parts, unresolved) = inline_parts(
            "edit @hero.png and \\@literal, not @missing.png or @channel",
            None,
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            text,
            "edit @hero.png and @literal, not @missing.png or @channel"
        );
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "hero.png");
        assert!(parts[0].region.is_none());
        assert_eq!(unresolved, vec!["missing.png"]);
        let (_, parts, _) = inline_parts("@hero.png:image/webp", Some("art"), dir.path()).unwrap();
        assert_eq!(parts[0].mime_type.as_ref().unwrap().as_str(), "image/webp");
        assert_eq!(parts[0].region.as_deref(), Some("art"));
    }

    #[test]
    fn region_values_read_text_or_attach_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "  # notes  ").unwrap();
        std::fs::write(dir.path().join("m.png"), png()).unwrap();
        std::fs::write(dir.path().join("blank.md"), "  ").unwrap();
        let reg = MimeRegistry::builtin();
        let text = read_region_input("brief", "@notes.md", dir.path(), &reg).unwrap();
        assert_eq!(text.text, "# notes");
        assert!(text.parts.is_empty());
        let file = read_region_input("art", "@m.png", dir.path(), &reg).unwrap();
        assert_eq!(file.text, "");
        assert_eq!(file.parts[0].region.as_deref(), Some("art"));
        assert!(read_region_input("brief", "@blank.md", dir.path(), &reg).is_err());
        assert!(read_region_input("brief", "@nope.md", dir.path(), &reg).is_err());
        let literal = read_region_input("brief", "see @m.png here", dir.path(), &reg).unwrap();
        assert_eq!(literal.text, "see @m.png here");
        assert_eq!(literal.parts.len(), 1);
        let spaced = read_region_input("brief", "@ not a file", dir.path(), &reg).unwrap();
        assert_eq!(spaced.text, "@ not a file");
        std::fs::write(dir.path().join("empty.png"), b"").unwrap();
        let err = read_region_input("brief", "see @empty.png", dir.path(), &reg).unwrap_err();
        assert!(err.to_string().contains("nothing to attach"), "{err}");
    }
}
