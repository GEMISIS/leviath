//! How a part looks to a script, and how a script hands parts back.
//!
//! A script never sees a [`Part`] as Rust holds it. It sees a flat map with
//! the fields a script has a use for (`media_type`, `name`, `sha256`,
//! `size`, `width`, `height`, `duration_ms`, `tokens`, `stand_in`), reads
//! the bytes behind one with `read_part`, and writes new bytes with
//! `write_part`, which stores them and hands the same flat map back. A tool
//! that wants its result to carry parts returns `#{ content: "...", parts:
//! [ <those maps> ] }`; the host resolves each map's `sha256` back to the
//! stored part it wrote.

use leviath_core::media::{Part, PartBody};

/// The flat map a script sees for one part.
pub fn part_summary(part: &Part) -> serde_json::Value {
    match &part.body {
        PartBody::Inline(text) => serde_json::json!({
            "media_type": part.media_type.as_str(),
            "name": part.name,
            "text": text,
            "size": text.len(),
            "tokens": leviath_core::estimate_tokens(text),
        }),
        PartBody::Stored(b) => serde_json::json!({
            "media_type": b.media_type.as_str(),
            "name": part.name,
            "sha256": b.sha256,
            "size": b.size,
            "width": b.width,
            "height": b.height,
            "duration_ms": b.duration_ms,
            "tokens": b.tokens,
            "stand_in": b.stand_in,
            "deliver": part.deliver,
        }),
    }
}

/// The `sha256` a script's part map names, if it names one.
pub fn sha_of(value: &serde_json::Value) -> Option<&str> {
    value.get("sha256").and_then(|v| v.as_str())
}

/// Whether a stored part answers to `wanted`: its file name exactly, or a
/// hash prefix of at least six characters.
pub fn part_matches(part: &Part, wanted: &str) -> bool {
    if part.name.as_deref() == Some(wanted) {
        return true;
    }
    let wanted = wanted.to_ascii_lowercase();
    wanted.len() >= 6 && part.blob().is_some_and(|b| b.sha256.starts_with(&wanted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::media::{Blob, BlobStore, MediaRegistry, MediaType, MemoryBlobStore};

    fn stored() -> Part {
        let reg = MediaRegistry::builtin();
        let blob = Blob::new(
            MediaType::parse("image/png").unwrap(),
            b"\x89PNG\r\n\x1a\nxyz".to_vec(),
        )
        .named("a.png");
        let r = MemoryBlobStore::new().put("r", &blob, &reg).unwrap();
        Part::stored(r).named("a.png")
    }

    #[test]
    fn summaries_carry_what_a_script_needs() {
        let s = part_summary(&stored());
        assert_eq!(s["media_type"], "image/png");
        assert_eq!(s["name"], "a.png");
        assert_eq!(s["size"], 11);
        assert!(s["stand_in"].as_str().unwrap().contains("a.png"));
        assert_eq!(sha_of(&s).unwrap().len(), 64);
        let t = part_summary(&Part::text("hello"));
        assert_eq!(t["text"], "hello");
        assert_eq!(t["size"], 5);
        assert!(sha_of(&t).is_none());
    }

    #[test]
    fn matching_is_by_name_or_hash_prefix() {
        let p = stored();
        let sha = p.blob().unwrap().sha256.clone();
        assert!(part_matches(&p, "a.png"));
        assert!(part_matches(
            &p,
            &sha.chars().take(8).collect::<String>().to_uppercase()
        ));
        assert!(!part_matches(&p, &sha.chars().take(5).collect::<String>()));
        assert!(!part_matches(&p, "b.png"));
        assert!(!part_matches(&Part::text("x"), "abcdef"));
    }
}
