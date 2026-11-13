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

use leviath_core::mime::{BlobStore, InboundPart, MimeRegistry};

use crate::blob_store::MimeParams;
use crate::components::{ContextWindow, Pushed, WriteOrigin};
use crate::context_setup::PartSink;

/// Whether `name` is one of the mime tools this module answers.
pub(crate) fn is_mime_tool(name: &str) -> bool {
    matches!(name, "context_attach" | "context_export")
}

/// What the mime tools need besides the window.
pub(crate) struct MimeToolContext<'a> {
    /// The world's store, registry and limits.
    pub mime: &'a MimeParams<'a, 'a>,
    /// The run's entity, which picks its registry.
    pub entity: bevy_ecs::entity::Entity,
    /// The run the parts belong to.
    pub run_id: &'a str,
    /// The run's working directory, when it has one. Paths resolve inside it.
    pub workdir: Option<&'a Path>,
    /// What the stage lets this tool be handed (`tool_accepts`), when it
    /// limits it: a part outside the list is refused by name.
    pub tool_limit: Option<&'a [String]>,
    /// The write paths in force at the stage, which `context_export` writes
    /// are held to. `None` limits nothing beyond the workdir.
    pub write_fence: Option<&'a leviath_core::write_paths::WriteFence>,
}

/// Answer one mime tool call, as text for the model.
pub(crate) fn handle_mime_tool(
    name: &str,
    args: &serde_json::Value,
    window: &mut ContextWindow,
    ctx: &MimeToolContext<'_>,
) -> String {
    let (sources, _) = ctx.mime.hydration_inputs(ctx.entity);
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
        max_part_bytes: ctx.mime.max_part_bytes(),
        inline_text_bytes: ctx.mime.inline_text_bytes(),
    };
    match name {
        "context_attach" => attach(args, window, &sink, workdir),
        _ => match export(args, window, store.as_ref(), &registry, workdir, ctx) {
            Ok((out, (path, sha256))) => {
                // The file is the run's working copy of the part from here on.
                window.mark_placed(&sha256, &path);
                out
            }
            Err(refusal) => refusal,
        },
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
        match leviath_core::mime::MimeType::parse(t) {
            Ok(t) => inbound = inbound.typed(t),
            Err(e) => return format!("[error] 'type': {e}"),
        }
    }
    if let Some(d) = arg(args, "deliver") {
        inbound.deliver = match leviath_core::mime::Delivery::from_arg(d) {
            Ok(d) => Some(d),
            Err(e) => return format!("[error] {e}"),
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
    let before = window.begin_change(region);
    if let Some(k) = key
        && let Some(r) = window.get_region_mut(region)
        && r.remove_by_key(k)
    {
        window.current_tokens = window.calculate_tokens();
        // Recorded apart from the write that follows: the release happens
        // outside it, and a history showing only the arrival would say the
        // region grew when it held steady.
        window.commit_change(
            leviath_core::ContextCause::ProducedPart,
            before,
            Pushed::Nothing,
        );
    }
    let written = window.typed_write_content(
        crate::components::TypedWrite {
            cause: Some(leviath_core::ContextCause::ProducedPart),
            origin: WriteOrigin::Agent,
            region,
            kind: leviath_core::EntryKind::Text,
            taint: None,
        },
        content.clone(),
        tokens,
    );
    if let Err(e) = written {
        return format!("[error] region '{region}' refused '{name}': {e}");
    }
    window.key_last_entry(region, key);
    // A copy of the file as it is now: the file at a path stays the newer of
    // the two, so the copy never answers a name a file is at.
    window.mark_file_copy(region, key);
    let sha = content
        .stored()
        .next()
        .and_then(|p| p.blob())
        .map(|b| b.short_sha().to_string())
        .unwrap_or_default();
    format!(
        "Attached '{name}' to region '{region}'{} as {} ({tokens} tokens, sha256 {sha}). It is in \
         your context under that heading; name it as '{name}' where a tool takes a part.",
        key.map(|k| format!(" under key '{k}'")).unwrap_or_default(),
        content
            .stored()
            .next()
            .map(|p| p.mime_type.to_string())
            .unwrap_or_default()
    )
}

/// `context_export { name, path }`: a stored part, by file name or hash
/// prefix, written into the workdir. A part outside the stage's limit for
/// this tool is refused by name, so the model learns the limit rather than
/// a missing file. Answers with the text for the model and where the file
/// was written (a [`placed_key`](crate::components::placed_key), with the
/// digest of what was written); a refusal is the error, and wrote nothing.
fn export(
    args: &serde_json::Value,
    window: &ContextWindow,
    store: &dyn BlobStore,
    registry: &MimeRegistry,
    workdir: &Path,
    ctx: &MimeToolContext<'_>,
) -> Result<(String, (String, String)), String> {
    let (run_id, limit, write_fence) = (ctx.run_id, ctx.tool_limit, ctx.write_fence);
    let Some(wanted) = arg(args, "name") else {
        return Err(
            "[error] missing 'name' argument: a part's file name or the start of its sha256"
                .to_string(),
        );
    };
    let Some((part_name, blob)) = find_part(window, wanted) else {
        let known: Vec<String> = window
            .regions
            .iter()
            .flat_map(|r| r.content.iter())
            .flat_map(|e| e.content.stored().map(|p| p.stand_in()))
            .collect();
        return Err(match known.is_empty() {
            true => format!("[error] no stored part matches '{wanted}'; this run holds none"),
            false => format!(
                "[error] no stored part matches '{wanted}'. The run holds:\n{}",
                known.join("\n")
            ),
        });
    };
    if let Some(limit) = limit
        && !blob.mime_type.matches_any(limit)
    {
        return Err(format!(
            "[error] '{wanted}' is {}; at this stage context_export may be handed only {}",
            blob.mime_type,
            limit.join(", ")
        ));
    }
    let target = arg(args, "path")
        .map(str::to_string)
        .or(part_name)
        .unwrap_or_else(|| {
            let ext = registry
                .info(&blob.mime_type)
                .extensions
                .first()
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            format!("{}{ext}", blob.short_sha())
        });
    let full = resolve(&target, workdir).map_err(|e| format!("[error] {e}"))?;
    // The stage's write paths hold this write as they hold `write_file`'s.
    if let Some(fence) = write_fence
        && let Err(refused) = fence.check(&target, workdir)
    {
        return Err(fence.refusal("context_export", &refused));
    }
    let bytes = store
        .read(run_id, &blob.sha256)
        .map_err(|e| format!("[error] could not read the stored bytes of '{wanted}': {e}"))?;
    if let Some(parent) = full.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Err(format!(
            "[error] could not create '{}': {e}",
            parent.display()
        ));
    }
    std::fs::write(&full, &bytes)
        .map_err(|e| format!("[error] could not write '{target}': {e}"))?;
    Ok((
        format!(
            "Wrote {} ({}) to '{target}'.",
            leviath_core::mime::human_size(bytes.len() as u64),
            blob.mime_type
        ),
        (
            crate::components::placed_key(&full, workdir),
            blob.sha256.clone(),
        ),
    ))
}

/// The stored part `wanted` names: its name and its reference.
fn find_part(
    window: &ContextWindow,
    wanted: &str,
) -> Option<(Option<String>, leviath_core::mime::BlobRef)> {
    let part = window.find_stored_part(wanted)?;
    part.blob().map(|blob| (part.name.clone(), blob.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_store::{BlobStoreHandle, MimeLimits, MimeRegistryHandle};
    use bevy_ecs::entity::Entity;
    use bevy_ecs::prelude::World;
    use bevy_ecs::system::SystemState;
    use leviath_core::mime::MemoryBlobStore;
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
    fn with_world(store: bool, f: &mut dyn FnMut(&MimeParams<'_, '_>, Entity)) {
        let mut world = World::new();
        let entity = world.spawn(()).id();
        if store {
            world.insert_resource(BlobStoreHandle(Arc::new(MemoryBlobStore::new())));
            world.insert_resource(MimeRegistryHandle::default());
            world.insert_resource(MimeLimits {
                max_part_bytes: 64,
                ..Default::default()
            });
        }
        let mut state: SystemState<MimeParams> = SystemState::new(&mut world);
        let mime = state.get(&world).expect("the mime params always validate");
        f(&mime, entity);
    }

    fn call(
        name: &str,
        args: serde_json::Value,
        window: &mut ContextWindow,
        mime: &MimeParams<'_, '_>,
        entity: Entity,
        workdir: Option<&Path>,
    ) -> String {
        handle_mime_tool(
            name,
            &args,
            window,
            &MimeToolContext {
                write_fence: None,
                mime,
                entity,
                run_id: "run-1",
                workdir,
                tool_limit: None,
            },
        )
    }

    /// A stage that limits `context_export` to some types keeps the rest
    /// out of its reach, by name rather than silently.
    #[test]
    fn an_export_outside_the_stages_limit_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hero.png"), b"\x89PNG\r\n\x1a\nv1").unwrap();
        with_world(true, &mut |mime, entity| {
            let mut w = window();
            let out = call(
                "context_attach",
                json!({"region": "sprites", "path": "hero.png"}),
                &mut w,
                mime,
                entity,
                Some(dir.path()),
            );
            assert!(out.starts_with("Attached"), "{out}");
            let limit = ["audio/*".to_string()];
            let out = handle_mime_tool(
                "context_export",
                &json!({"name": "hero.png"}),
                &mut w,
                &MimeToolContext {
                    write_fence: None,
                    mime,
                    entity,
                    run_id: "run-1",
                    workdir: Some(dir.path()),
                    tool_limit: Some(&limit),
                },
            );
            assert_eq!(
                out,
                "[error] 'hero.png' is image/png; at this stage context_export may be handed only audio/*"
            );
            let limit = ["image/*".to_string()];
            let out = handle_mime_tool(
                "context_export",
                &json!({"name": "hero.png", "path": "out.png"}),
                &mut w,
                &MimeToolContext {
                    write_fence: None,
                    mime,
                    entity,
                    run_id: "run-1",
                    workdir: Some(dir.path()),
                    tool_limit: Some(&limit),
                },
            );
            assert!(out.starts_with("Wrote"), "{out}");
        });
    }

    /// `context_export` writes a file, so the stage's write paths hold it as
    /// they hold `write_file`: outside them it is refused and nothing lands.
    #[test]
    fn an_export_outside_the_write_paths_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/payload.txt"), b"any bytes").unwrap();
        let fence = leviath_core::write_paths::WriteFence::new(&[vec!["docs/**".to_string()]]);
        with_world(true, &mut |mime, entity| {
            let mut w = window();
            let out = call(
                "context_attach",
                json!({"region": "sprites", "path": "docs/payload.txt"}),
                &mut w,
                mime,
                entity,
                Some(dir.path()),
            );
            assert!(out.starts_with("Attached"), "{out}");
            let export = |path: &str, w: &mut ContextWindow| {
                handle_mime_tool(
                    "context_export",
                    &json!({"name": "payload.txt", "path": path}),
                    w,
                    &MimeToolContext {
                        write_fence: Some(&fence),
                        mime,
                        entity,
                        run_id: "run-1",
                        workdir: Some(dir.path()),
                        tool_limit: None,
                    },
                )
            };
            let refused = export("src/App.tsx", &mut w);
            assert_eq!(
                refused,
                "[blocked] context_export refused: src/App.tsx is outside this stage's write \
                 paths (docs/**). Write inside them instead."
            );
            let landed = dir.path().join("src/App.tsx").exists();
            assert!(!landed, "a refused export wrote anyway");
            let wrote = export("docs/copy.txt", &mut w);
            assert!(wrote.starts_with("Wrote"), "{wrote}");
        });
    }

    /// Submit `path` as an artifact through the real `submit_output`
    /// handler, over the world's own store.
    fn submit(
        w: &mut ContextWindow,
        mime: &MimeParams<'_, '_>,
        entity: Entity,
        dir: &Path,
        path: &str,
    ) -> leviath_core::output::Artifact {
        let (sources, _) = mime.hydration_inputs(entity);
        let (store, registry) = sources.expect("a store");
        let sink = PartSink {
            store: store.as_ref(),
            registry: &registry,
            run_id: "run-1",
            max_part_bytes: 1 << 20,
            inline_text_bytes: 1024,
        };
        let (ack, output) = crate::output_tool::handle_output_tool(
            &json!({ "content": "done", "artifacts": [path] }),
            &crate::output_tool::OutputContext {
                write_fence: None,
                tool_verdict: None,
                given: &[],
                spec: None,
                validators: None,
                stage: "draw",
                stage_names: &[],
                workdir: Some(dir),
                sink: Some(&sink),
                overwrite_artifacts: false,
            },
            0,
            w,
        );
        output.expect(&ack).artifacts[0].clone()
    }

    /// Issue 1134, through the tools themselves: a stage attaches
    /// `art/hero.png`, rewrites it, and submits the path. The attach marks
    /// its entry a copy of the file, keyed or not, so the rewritten file is
    /// what is recorded and nothing is written beside it.
    #[test]
    fn an_attached_file_rewritten_since_is_recorded_as_rewritten() {
        const REWRITTEN: &[u8] = b"\x89PNG\r\n\x1a\nthe rewrite";
        for key in [None, Some("hero")] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("art")).unwrap();
            let hero = dir.path().join("art/hero.png");
            std::fs::write(&hero, b"\x89PNG\r\n\x1a\nv1").unwrap();
            with_world(true, &mut |mime, entity| {
                let mut w = window();
                w.add_region(Region::new(
                    crate::output_tool::FINAL_OUTPUT_REGION.into(),
                    RegionKind::Pinned,
                    10_000,
                ));
                // Attached twice under a key: the second version replaces
                // the first and is a copy too.
                for _ in 0..2 {
                    let out = call(
                        "context_attach",
                        json!({"region": "sprites", "path": "art/hero.png", "key": key}),
                        &mut w,
                        mime,
                        entity,
                        Some(dir.path()),
                    );
                    assert!(out.starts_with("Attached"), "{out}");
                }
                let sprites = w.get_region("sprites").unwrap();
                assert_eq!(sprites.content.len(), if key.is_some() { 1 } else { 2 });
                assert!(sprites.content.iter().all(|e| e.is_file_copy()));

                std::fs::write(&hero, REWRITTEN).unwrap();
                let artifact = submit(&mut w, mime, entity, dir.path(), "art/hero.png");
                assert_eq!(artifact.path, "art/hero.png");
                assert_eq!(artifact.sha256, leviath_core::mime::sha256_hex(REWRITTEN));
                assert_eq!(std::fs::read(&hero).unwrap(), REWRITTEN);
                assert_eq!(
                    std::fs::read_dir(dir.path().join("art")).unwrap().count(),
                    1
                );
            });
        }
    }

    /// The same through `context_export`: a part the run made is exported,
    /// the file is edited, and the path is submitted. The export records
    /// where it wrote, with the path's dots folded, so the edited file is
    /// what is recorded. A refused export records nothing.
    #[test]
    fn an_exported_part_edited_since_is_recorded_as_edited() {
        const EDITED: &[u8] = b"\x89PNG\r\n\x1a\nedited after the export";
        let dir = tempfile::tempdir().unwrap();
        with_world(true, &mut |mime, entity| {
            let (sources, _) = mime.hydration_inputs(entity);
            let (store, registry) = sources.expect("a store");
            let blob = leviath_core::mime::Blob::new(
                leviath_core::mime::MimeType::parse("image/png").unwrap(),
                b"\x89PNG\r\n\x1a\ndrawn".to_vec(),
            )
            .named("hero.png");
            let reference = store.put("run-1", &blob, &registry).unwrap();
            let sha = reference.sha256.clone();
            let mut w = window();
            w.add_region(Region::new(
                crate::output_tool::FINAL_OUTPUT_REGION.into(),
                RegionKind::Pinned,
                10_000,
            ));
            let part = leviath_core::mime::Part::stored(reference).named("hero.png");
            w.get_region_mut("sprites")
                .unwrap()
                .add_entry(EntryContent::from_parts(vec![part]), 1)
                .unwrap();
            let placed = |w: &ContextWindow| w.get_region("sprites").unwrap().content[0].placed();

            let refused = call(
                "context_export",
                json!({"name": "hero.png", "path": "../hero.png"}),
                &mut w,
                mime,
                entity,
                Some(dir.path()),
            );
            assert!(refused.starts_with("[error]"), "{refused}");
            assert!(placed(&w).is_empty());

            let out = call(
                "context_export",
                json!({"name": "hero.png", "path": "./out/../out/hero.png"}),
                &mut w,
                mime,
                entity,
                Some(dir.path()),
            );
            assert!(out.starts_with("Wrote"), "{out}");
            assert_eq!(placed(&w), [("out/hero.png".to_string(), sha)]);

            std::fs::write(dir.path().join("out/hero.png"), EDITED).unwrap();
            let artifact = submit(&mut w, mime, entity, dir.path(), "out/hero.png");
            assert_eq!(artifact.path, "out/hero.png");
            assert_eq!(artifact.sha256, leviath_core::mime::sha256_hex(EDITED));
            assert_eq!(
                std::fs::read_dir(dir.path().join("out")).unwrap().count(),
                1
            );
        });
    }

    #[test]
    fn names_are_recognised() {
        assert!(is_mime_tool("context_attach"));
        assert!(is_mime_tool("context_export"));
        assert!(!is_mime_tool("context_write"));
    }

    #[test]
    fn attach_then_export_round_trips_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hero.png"), b"\x89PNG\r\n\x1a\nv1").unwrap();
        with_world(true, &mut |mime, entity| {
            let mut w = window();
            let out = call(
                "context_attach",
                json!({"region": "sprites", "path": "hero.png", "key": "hero", "caption": "v1"}),
                &mut w,
                mime,
                entity,
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
                mime,
                entity,
                Some(dir.path()),
            );
            assert!(out.starts_with("Attached"), "{out}");
            let region = w.get_region("sprites").unwrap();
            assert_eq!(region.content.len(), 1);
            assert_eq!(region.content[0].content.parts().len(), 1);
            assert_eq!(
                region.content[0].content.parts()[0].deliver,
                Some(leviath_core::mime::Delivery::Native)
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
                mime,
                entity,
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
                mime,
                entity,
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
                mime,
                entity,
                Some(dir.path()),
            );
            let short = sha.get(..12).unwrap_or(&sha);
            assert!(out.contains(&format!("to '{short}.png'")), "{out}");
            // Text-only entries are not stored parts.
            w.get_region_mut("sprites").unwrap().content[0].content = EntryContent::text("x");
            let out = call(
                "context_export",
                json!({"name": sha.chars().take(8).collect::<String>()}),
                &mut w,
                mime,
                entity,
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
        with_world(false, &mut |mime, entity| {
            let mut w = window();
            let out = call(
                "context_attach",
                json!({}),
                &mut w,
                mime,
                entity,
                Some(dir.path()),
            );
            assert!(out.contains("no blob store"), "{out}");
        });
        with_world(true, &mut |mime, entity| {
            let mut w = window();
            let wd = Some(dir.path());
            let out = call(
                "context_attach",
                json!({"region": "art", "path": "ok.png"}),
                &mut w,
                mime,
                entity,
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
            ];
            for (args, expect) in cases {
                let out = call("context_attach", args.clone(), &mut w, mime, entity, wd);
                assert!(out.contains(expect), "{args}: {out}");
            }
            let out = call(
                "context_attach",
                json!({"region": "art", "path": "ok.png", "deliver": "stand_in"}),
                &mut w,
                mime,
                entity,
                wd,
            );
            assert!(out.starts_with("Attached"), "{out}");
            // A caption travels with the media: an image region takes the pair
            // (the caption is text/plain and is not gated by `accepts`).
            let out = call(
                "context_attach",
                json!({"region": "art", "path": "ok.png", "caption": "a note"}),
                &mut w,
                mime,
                entity,
                wd,
            );
            assert!(out.starts_with("Attached"), "captioned attach: {out}");

            let out = call("context_export", json!({}), &mut w, mime, entity, wd);
            assert!(out.contains("missing 'name'"), "{out}");
            let out = call(
                "context_export",
                json!({"name": "nope.png"}),
                &mut w,
                mime,
                entity,
                wd,
            );
            assert!(out.contains("The run holds:\n[image/png"), "{out}");
            let out = call(
                "context_export",
                json!({"name": "ok.png", "path": "../x.png"}),
                &mut w,
                mime,
                entity,
                wd,
            );
            assert!(out.contains("escape"), "{out}");
            let out = call(
                "context_export",
                json!({"name": "ok.png", "path": "a/b/ok.png"}),
                &mut w,
                mime,
                entity,
                wd,
            );
            assert!(out.starts_with("Wrote"), "{out}");
            std::fs::write(dir.path().join("file"), b"x").unwrap();
            let out = call(
                "context_export",
                json!({"name": "ok.png", "path": "file/ok.png"}),
                &mut w,
                mime,
                entity,
                wd,
            );
            assert!(out.contains("could not create"), "{out}");
            std::fs::create_dir_all(dir.path().join("adir")).unwrap();
            let out = call(
                "context_export",
                json!({"name": "ok.png", "path": "adir"}),
                &mut w,
                mime,
                entity,
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
            assert!(find_part(&broken, sha.get(..12).unwrap_or(&sha)).is_none());
        });
        with_world(true, &mut |mime, entity| {
            let mut w = window();
            let mut part = leviath_core::mime::Part::stored(leviath_core::mime::BlobRef {
                sha256: "0".repeat(64),
                mime_type: leviath_core::mime::MimeType::parse("image/png").unwrap(),
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
                mime,
                entity,
                Some(dir.path()),
            );
            assert!(out.contains("could not read the stored bytes"), "{out}");
        });
    }
}
