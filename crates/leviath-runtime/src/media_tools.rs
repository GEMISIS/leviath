//! The two tools that move bytes between the workdir and a region:
//! `context_attach` and `context_export`.
//!
//! Both need the live window and the run's blob store, so they run inline
//! in the dispatcher beside the other `context_*` tools rather than on the
//! async lane. `context_attach` reads a file the agent (or a shell tool)
//! produced and writes it into a region as a stored part, with a caption
//! and an optional key so a new version replaces the old. `context_export`
//! is the reverse: a stored part, named by file name or hash prefix, written
//! into the workdir where a shell tool can reach it.

use std::path::Path;

use leviath_core::media::{BlobStore, InboundPart, MediaRegistry};

use crate::blob_store::MediaParams;
use crate::components::{ContextWindow, WriteOrigin};
use crate::context_setup::PartSink;

/// Whether `name` is one of the media tools this module answers.
pub(crate) fn is_media_tool(name: &str) -> bool {
    matches!(name, "context_attach" | "context_export")
}

/// What the media tools need besides the window.
pub(crate) struct MediaToolContext<'a> {
    /// The world's store, registry and limits.
    pub media: &'a MediaParams<'a>,
    /// The run the parts belong to.
    pub run_id: &'a str,
    /// The run's working directory, when it has one. Paths resolve inside it.
    pub workdir: Option<&'a Path>,
}

/// Answer one media tool call, as text for the model.
pub(crate) fn handle_media_tool(
    name: &str,
    args: &serde_json::Value,
    window: &mut ContextWindow,
    ctx: &MediaToolContext<'_>,
) -> String {
    let (sources, _) = ctx.media.hydration_inputs();
    let Some((store, registry)) = sources else {
        return "[error] this run has no blob store, so it cannot hold parts".to_string();
    };
    let Some(workdir) = ctx.workdir else {
        return "[error] this run has no working directory to read files from".to_string();
    };
    let sink = PartSink {
        store: store.as_ref(),
        registry: &registry,
        run_id: ctx.run_id,
        max_part_bytes: ctx.media.max_part_bytes(),
    };
    match name {
        "context_attach" => attach(args, window, &sink, workdir),
        _ => export(args, window, store.as_ref(), &registry, ctx.run_id, workdir),
    }
}

/// The string argument under `key`, trimmed, if the model passed one.
fn arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// A workdir path the model named, resolved and confined to the workdir.
fn resolve(requested: &str, workdir: &Path) -> Result<std::path::PathBuf, String> {
    leviath_tools::resolve_within(requested, workdir, leviath_core::resolves_within)
        .map_err(|e| e.to_string())
}

/// `context_attach { region, path, key?, caption?, deliver? }`.
fn attach(
    args: &serde_json::Value,
    window: &mut ContextWindow,
    sink: &PartSink<'_>,
    workdir: &Path,
) -> String {
    let Some(region) = arg(args, "region") else {
        return "[error] missing 'region' argument".to_string();
    };
    let Some(path) = arg(args, "path") else {
        return "[error] missing 'path' argument".to_string();
    };
    if window.get_region(region).is_none() {
        return format!("[error] no region named '{region}' in this context window");
    }
    let full = match resolve(path, workdir) {
        Ok(p) => p,
        Err(e) => return format!("[error] {e}"),
    };
    let data = match std::fs::read(&full) {
        Ok(d) if d.is_empty() => return format!("[error] '{path}' is empty"),
        Ok(d) => d,
        Err(e) => return format!("[error] could not read '{path}': {e}"),
    };
    // A path that read as a file has a final component.
    let name = full
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut inbound = InboundPart::from_bytes(name.clone(), data).in_region(region);
    if let Some(caption) = arg(args, "caption") {
        inbound = inbound.captioned(caption);
    }
    if let Some(t) = arg(args, "type") {
        match leviath_core::media::MediaType::parse(t) {
            Ok(t) => inbound = inbound.typed(t),
            Err(e) => return format!("[error] 'type': {e}"),
        }
    }
    if let Some(d) = arg(args, "deliver") {
        inbound.deliver = match d {
            "native" => Some(leviath_core::media::Delivery::Native),
            "text" => Some(leviath_core::media::Delivery::Text),
            "stand_in" => Some(leviath_core::media::Delivery::StandIn),
            other => {
                return format!(
                    "[error] 'deliver' must be native, text or stand_in, not '{other}'"
                );
            }
        };
    }
    let content = match sink.entry_for(&inbound) {
        Ok(c) => c,
        Err(e) => return format!("[error] {e}"),
    };
    let tokens = sink.tokens_for(&content);
    let key = arg(args, "key");
    // A keyed attach replaces the previous version: the region never holds
    // two of the same sprite.
    if let Some(k) = key
        && let Some(r) = window.get_region_mut(region)
        && r.remove_by_key(k)
    {
        window.current_tokens = window.calculate_tokens();
    }
    let written = window.typed_write_content(
        WriteOrigin::Agent,
        region,
        leviath_core::EntryKind::Text,
        content.clone(),
        tokens,
        None,
    );
    if let Err(e) = written {
        return format!("[error] region '{region}' refused '{name}': {e}");
    }
    if let Some(k) = key
        && let Some(entry) = window
            .get_region_mut(region)
            .and_then(|r| r.content.last_mut())
    {
        entry.key = Some(k.to_string());
    }
    let sha = content
        .stored()
        .next()
        .and_then(|p| p.blob())
        .map(|b| short(&b.sha256).to_string())
        .unwrap_or_default();
    format!(
        "Attached '{name}' to region '{region}'{} as {} ({tokens} tokens, sha256 {sha}). It is in \
         your context under that heading; name it as '{name}' where a tool takes a part.",
        key.map(|k| format!(" under key '{k}'")).unwrap_or_default(),
        content
            .stored()
            .next()
            .map(|p| p.media_type.to_string())
            .unwrap_or_default()
    )
}

/// `context_export { name, path }`: a stored part, by file name or hash
/// prefix, written into the workdir.
fn export(
    args: &serde_json::Value,
    window: &ContextWindow,
    store: &dyn BlobStore,
    registry: &MediaRegistry,
    run_id: &str,
    workdir: &Path,
) -> String {
    let Some(wanted) = arg(args, "name") else {
        return "[error] missing 'name' argument: a part's file name or the start of its sha256"
            .to_string();
    };
    let Some((part_name, blob)) = find_part(window, wanted) else {
        let known: Vec<String> = window
            .regions
            .iter()
            .flat_map(|r| r.content.iter())
            .flat_map(|e| e.content.stored().map(|p| p.stand_in()))
            .collect();
        return match known.is_empty() {
            true => format!("[error] no stored part matches '{wanted}'; this run holds none"),
            false => format!(
                "[error] no stored part matches '{wanted}'. The run holds:\n{}",
                known.join("\n")
            ),
        };
    };
    let target = arg(args, "path")
        .map(str::to_string)
        .or(part_name)
        .unwrap_or_else(|| {
            let ext = registry
                .info(&blob.media_type)
                .extensions
                .first()
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            format!("{}{ext}", short(&blob.sha256))
        });
    let full = match resolve(&target, workdir) {
        Ok(p) => p,
        Err(e) => return format!("[error] {e}"),
    };
    let bytes = match store.read(run_id, &blob.sha256) {
        Ok(b) => b,
        Err(e) => return format!("[error] could not read the stored bytes of '{wanted}': {e}"),
    };
    if let Some(parent) = full.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return format!("[error] could not create '{}': {e}", parent.display());
    }
    match std::fs::write(&full, &bytes) {
        Ok(()) => format!(
            "Wrote {} ({}) to '{target}'.",
            leviath_core::media::human_size(bytes.len() as u64),
            blob.media_type
        ),
        Err(e) => format!("[error] could not write '{target}': {e}"),
    }
}

/// The first twelve characters of a hash: enough to name it, short enough
/// to read.
fn short(sha256: &str) -> &str {
    sha256.get(..12).unwrap_or(sha256)
}

/// The most recent stored part whose name is `wanted` or whose hash starts
/// with it: its name and its reference.
fn find_part(
    window: &ContextWindow,
    wanted: &str,
) -> Option<(Option<String>, leviath_core::media::BlobRef)> {
    let wanted_lower = wanted.to_ascii_lowercase();
    window
        .regions
        .iter()
        .flat_map(|r| r.content.iter())
        .rev()
        .flat_map(|e| e.content.stored())
        .filter_map(|p| p.blob().map(|b| (p.name.clone(), b.clone())))
        .find(|(name, b)| {
            name.as_deref() == Some(wanted)
                || (wanted_lower.len() >= 6 && b.sha256.starts_with(&wanted_lower))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_store::{BlobStoreHandle, MediaLimits, MediaRegistryHandle};
    use bevy_ecs::prelude::World;
    use bevy_ecs::system::SystemState;
    use leviath_core::media::MemoryBlobStore;
    use leviath_core::region::{EntryContent, Region, RegionKind};
    use serde_json::json;
    use std::sync::Arc;

    fn attached_entry(window: &ContextWindow, region: &str) -> EntryContent {
        window.get_region(region).unwrap().content[0]
            .content
            .clone()
    }

    fn window() -> ContextWindow {
        let mut w = ContextWindow::new(100_000);
        w.add_region(Region::new("sprites".into(), RegionKind::Pinned, 10_000));
        let mut art = Region::new("art".into(), RegionKind::Pinned, 10_000);
        art.accepts = vec!["image/*".into()];
        w.add_region(art);
        w.add_region(Region::new("tiny".into(), RegionKind::Pinned, 1));
        w
    }

    // `&mut dyn`, not generic: one instantiation, so the coverage gate sees
    // every branch of this helper run across the calls that share it.
    fn with_world(store: bool, f: &mut dyn FnMut(&MediaParams<'_>)) {
        let mut world = World::new();
        if store {
            world.insert_resource(BlobStoreHandle(Arc::new(MemoryBlobStore::new())));
            world.insert_resource(MediaRegistryHandle::default());
            world.insert_resource(MediaLimits {
                max_part_bytes: 64,
                ..Default::default()
            });
        }
        let mut state: SystemState<MediaParams> = SystemState::new(&mut world);
        let media = state.get(&world).expect("the media params always validate");
        f(&media);
    }

    fn call(
        name: &str,
        args: serde_json::Value,
        window: &mut ContextWindow,
        media: &MediaParams<'_>,
        workdir: Option<&Path>,
    ) -> String {
        handle_media_tool(
            name,
            &args,
            window,
            &MediaToolContext {
                media,
                run_id: "run-1",
                workdir,
            },
        )
    }

    #[test]
    fn names_are_recognised() {
        assert!(is_media_tool("context_attach"));
        assert!(is_media_tool("context_export"));
        assert!(!is_media_tool("context_write"));
    }

    #[test]
    fn attach_then_export_round_trips_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hero.png"), b"\x89PNG\r\n\x1a\nv1").unwrap();
        with_world(true, &mut |media| {
            let mut w = window();
            let out = call(
                "context_attach",
                json!({"region": "sprites", "path": "hero.png", "key": "hero", "caption": "v1"}),
                &mut w,
                media,
                Some(dir.path()),
            );
            assert!(
                out.starts_with(
                    "Attached 'hero.png' to region 'sprites' under key 'hero' as image/png"
                ),
                "{out}"
            );
            let entry = attached_entry(&w, "sprites");
            assert_eq!(entry.parts().len(), 2);
            assert!(entry.as_str().starts_with("v1\n[image/png"));
            assert_eq!(
                w.get_region("sprites").unwrap().content[0].key.as_deref(),
                Some("hero")
            );

            // A second version under the same key replaces the first.
            std::fs::write(dir.path().join("hero.png"), b"\x89PNG\r\n\x1a\nv2-longer").unwrap();
            let out = call(
                "context_attach",
                json!({"region": "sprites", "path": "hero.png", "key": "hero", "type": "image/png", "deliver": "native"}),
                &mut w,
                media,
                Some(dir.path()),
            );
            assert!(out.starts_with("Attached"), "{out}");
            let region = w.get_region("sprites").unwrap();
            assert_eq!(region.content.len(), 1);
            assert_eq!(region.content[0].content.parts().len(), 1);
            assert_eq!(
                region.content[0].content.parts()[0].deliver,
                Some(leviath_core::media::Delivery::Native)
            );
            let sha = region.content[0].content.parts()[0]
                .blob()
                .unwrap()
                .sha256
                .clone();

            // Export by name, by hash prefix, and to a chosen path.
            let out = call(
                "context_export",
                json!({"name": "hero.png", "path": "out/hero-v2.png"}),
                &mut w,
                media,
                Some(dir.path()),
            );
            assert!(
                out.starts_with("Wrote 17 B (image/png) to 'out/hero-v2.png'"),
                "{out}"
            );
            assert_eq!(
                std::fs::read(dir.path().join("out/hero-v2.png")).unwrap(),
                b"\x89PNG\r\n\x1a\nv2-longer"
            );
            let out = call(
                "context_export",
                json!({"name": sha.chars().take(8).collect::<String>()}),
                &mut w,
                media,
                Some(dir.path()),
            );
            assert!(out.contains("to 'hero.png'"), "{out}");
            // A part with no name and no path lands under its hash.
            let mut unnamed =
                w.get_region("sprites").unwrap().content[0].content.parts()[0].clone();
            unnamed.name = None;
            w.get_region_mut("sprites").unwrap().content[0].content =
                EntryContent::from_parts(vec![unnamed]);
            let out = call(
                "context_export",
                json!({"name": sha.chars().take(8).collect::<String>()}),
                &mut w,
                media,
                Some(dir.path()),
            );
            assert!(out.contains(&format!("to '{}.png'", short(&sha))), "{out}");
            // Text-only entries are not stored parts.
            w.get_region_mut("sprites").unwrap().content[0].content = EntryContent::text("x");
            let out = call(
                "context_export",
                json!({"name": sha.chars().take(8).collect::<String>()}),
                &mut w,
                media,
                Some(dir.path()),
            );
            assert!(out.contains("this run holds none"), "{out}");
        });
    }

    #[test]
    fn every_refusal_says_why() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.png"), b"").unwrap();
        std::fs::write(dir.path().join("song.wav"), [1u8, 2, 3]).unwrap();
        std::fs::write(dir.path().join("big.bin"), vec![0; 100]).unwrap();
        std::fs::write(dir.path().join("ok.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        with_world(false, &mut |media| {
            let mut w = window();
            let out = call("context_attach", json!({}), &mut w, media, Some(dir.path()));
            assert!(out.contains("no blob store"), "{out}");
        });
        with_world(true, &mut |media| {
            let mut w = window();
            let wd = Some(dir.path());
            let out = call(
                "context_attach",
                json!({"region": "art", "path": "ok.png"}),
                &mut w,
                media,
                None,
            );
            assert!(out.contains("no working directory"), "{out}");
            let cases = [
                (json!({"path": "ok.png"}), "missing 'region'"),
                (json!({"region": "art"}), "missing 'path'"),
                (
                    json!({"region": "ghost", "path": "ok.png"}),
                    "no region named 'ghost'",
                ),
                (json!({"region": "art", "path": "../outside.png"}), "escape"),
                (
                    json!({"region": "art", "path": "missing.png"}),
                    "could not read",
                ),
                (json!({"region": "art", "path": "empty.png"}), "is empty"),
                (
                    json!({"region": "art", "path": "ok.png", "type": "nope"}),
                    "'type'",
                ),
                (
                    json!({"region": "art", "path": "ok.png", "deliver": "loud"}),
                    "'deliver' must be",
                ),
                (
                    json!({"region": "art", "path": "song.wav"}),
                    "refused 'song.wav'",
                ),
                (json!({"region": "art", "path": "big.bin"}), "ceiling"),
                (
                    json!({"region": "tiny", "path": "ok.png", "deliver": "text"}),
                    "refused 'ok.png'",
                ),
                (
                    json!({"region": "art", "path": "ok.png", "caption": "c"}),
                    "carries text/plain",
                ),
            ];
            for (args, expect) in cases {
                let out = call("context_attach", args.clone(), &mut w, media, wd);
                assert!(out.contains(expect), "{args}: {out}");
            }
            let out = call(
                "context_attach",
                json!({"region": "art", "path": "ok.png", "deliver": "stand_in"}),
                &mut w,
                media,
                wd,
            );
            assert!(out.starts_with("Attached"), "{out}");

            let out = call("context_export", json!({}), &mut w, media, wd);
            assert!(out.contains("missing 'name'"), "{out}");
            let out = call(
                "context_export",
                json!({"name": "nope.png"}),
                &mut w,
                media,
                wd,
            );
            assert!(out.contains("The run holds:\n[image/png"), "{out}");
            let out = call(
                "context_export",
                json!({"name": "ok.png", "path": "../x.png"}),
                &mut w,
                media,
                wd,
            );
            assert!(out.contains("escape"), "{out}");
            let out = call(
                "context_export",
                json!({"name": "ok.png", "path": "a/b/ok.png"}),
                &mut w,
                media,
                wd,
            );
            assert!(out.starts_with("Wrote"), "{out}");
            std::fs::write(dir.path().join("file"), b"x").unwrap();
            let out = call(
                "context_export",
                json!({"name": "ok.png", "path": "file/ok.png"}),
                &mut w,
                media,
                wd,
            );
            assert!(out.contains("could not create"), "{out}");
            std::fs::create_dir_all(dir.path().join("adir")).unwrap();
            let out = call(
                "context_export",
                json!({"name": "ok.png", "path": "adir"}),
                &mut w,
                media,
                wd,
            );
            assert!(out.contains("could not write 'adir'"), "{out}");
            // The bytes gone from the store.
            let sha = w.get_region("art").unwrap().content[0].content.parts()[0]
                .blob()
                .unwrap()
                .sha256
                .clone();
            let broken = ContextWindow::new(10);
            assert!(find_part(&broken, short(&sha)).is_none());
        });
        with_world(true, &mut |media| {
            let mut w = window();
            let mut part = leviath_core::media::Part::stored(leviath_core::media::BlobRef {
                sha256: "0".repeat(64),
                media_type: leviath_core::media::MediaType::parse("image/png").unwrap(),
                size: 1,
                width: None,
                height: None,
                duration_ms: None,
                tokens: 1,
                stand_in: "[image/png, 1 B] lost.png".to_string(),
            });
            part.name = Some("lost.png".to_string());
            w.get_region_mut("art")
                .unwrap()
                .add_entry(EntryContent::from_parts(vec![part]), 1)
                .unwrap();
            let out = call(
                "context_export",
                json!({"name": "lost.png"}),
                &mut w,
                media,
                Some(dir.path()),
            );
            assert!(out.contains("could not read the stored bytes"), "{out}");
        });
    }
}
