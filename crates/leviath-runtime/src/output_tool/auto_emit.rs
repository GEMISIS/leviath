//! Emitting a stage's produced parts as the run's final output, with no
//! `submit_output` call and so no text model.
//!
//! A stage whose model *makes* files - a 3D generator, an image model - has
//! already done its job when the reply's parts land in the regions its
//! `output_routing` names. The old way to hand those back as the run's answer
//! was a second stage: a text model that reads the produced part and calls
//! `submit_output` naming it. That made a pure "bytes in, bytes out" pipeline -
//! a picture to a mesh, a mesh to an animation - depend on a text provider it
//! otherwise never needed.
//!
//! This is the other way. When an output stage declares artifacts and the parts
//! it routed satisfy them, the run records those parts as its final output
//! directly: no tool call, no text turn, no second provider. A stage that
//! declares no artifacts, or one whose `required` artifact no produced part
//! matches, is left to the `submit_output` path as before.

use leviath_core::mime::Part;
use leviath_core::output::{Artifact, ArtifactSpec, FinalOutput, OutputSpec};

use crate::components::ContextWindow;

/// Build a [`FinalOutput`] from the parts a stage routed, when they satisfy the
/// stage's declared artifacts, and mirror it into the `final_output` region the
/// way a submission would. `None` when the stage declares no artifacts, routes
/// nothing, or a `required` artifact has no matching produced part - in which
/// case the caller falls back to nudging for a `submit_output` call.
///
/// The produced parts are read only from the regions the stage's
/// `output_routing` names, never the whole window: an input mesh in some other
/// region must not be mistaken for the animated one this stage made.
pub(crate) fn try_emit(
    stage: &leviath_core::blueprint::Stage,
    spec: Option<&OutputSpec>,
    now: i64,
    window: &mut ContextWindow,
) -> Option<FinalOutput> {
    let specs = spec.map(|s| s.artifacts.as_slice()).unwrap_or_default();
    if specs.is_empty() {
        return None;
    }
    let targets: Vec<&str> = stage.output_routing.values().map(String::as_str).collect();
    if targets.is_empty() {
        return None;
    }
    let produced = window.stored_parts_in(&targets);
    let records = match_artifacts(specs, &produced)?;
    let content = describe(&records);
    let output = FinalOutput::new(
        content.as_str(),
        spec.and_then(|s| s.format.clone()),
        stage.name.clone(),
        now,
    )
    .with_artifacts(records);
    // The parts already live in their routed region, so mirror only the
    // one-line answer: re-storing them here would duplicate the entry.
    super::mirror_into_region(window, &output.content, Vec::new(), None);
    Some(output)
}

/// Match each declared artifact to a produced part by type, each part used at
/// most once and in declaration order. `None` if any `required` artifact goes
/// unmatched, or nothing matched at all (an output stage that emitted no part
/// still owes its answer through `submit_output`).
fn match_artifacts(specs: &[ArtifactSpec], produced: &[Part]) -> Option<Vec<Artifact>> {
    let mut used = vec![false; produced.len()];
    let mut records = Vec::new();
    for spec in specs {
        // Fetch the blob and test the type together, so a part with no blob (an
        // inline part that should never reach here) is simply skipped rather
        // than matched on its type and then found to have no bytes.
        let matched = produced.iter().enumerate().find_map(|(i, part)| {
            let blob = part.blob()?;
            (!used[i] && blob.mime_type.matches(&spec.mime_type)).then_some((i, part, blob))
        });
        match matched {
            Some((i, part, blob)) => {
                used[i] = true;
                records.push(Artifact {
                    name: spec.name.clone(),
                    // Falls back to the declared name when the produced part is
                    // unnamed, so the record always has something to call it.
                    path: part.name.clone().unwrap_or_else(|| spec.name.clone()),
                    mime_type: blob.mime_type.clone(),
                    size: blob.size,
                    sha256: blob.sha256.clone(),
                });
            }
            None if spec.required => return None,
            None => {}
        }
    }
    (!records.is_empty()).then_some(records)
}

/// A one-line description of what the run produced, for the answer's text.
///
/// The stage handed back files, not prose, so the answer says what they are,
/// in the shape `submit_output` acknowledges them.
fn describe(records: &[Artifact]) -> String {
    let listed: Vec<String> = records.iter().map(Artifact::short_label).collect();
    format!("Produced {}.", listed.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::blueprint::{ModelConfig, Stage};
    use leviath_core::mime::{Blob, MimeRegistry, MimeType};
    use leviath_core::region::{Region, RegionKind};

    fn spec_with(artifacts: Vec<ArtifactSpec>) -> OutputSpec {
        OutputSpec {
            artifacts,
            ..OutputSpec::default()
        }
    }

    fn artifact_spec(name: &str, mime: &str, required: bool) -> ArtifactSpec {
        ArtifactSpec {
            name: name.to_string(),
            mime_type: mime.to_string(),
            required,
            description: None,
        }
    }

    fn stage_routing(rules: &[(&str, &str)]) -> Stage {
        let mut stage = Stage::new(
            "build".to_string(),
            ModelConfig::new("meshy".to_string(), "image-to-3d".to_string()),
        );
        for (pattern, region) in rules {
            stage
                .output_routing
                .insert((*pattern).to_string(), (*region).to_string());
        }
        stage
    }

    /// A window carrying one stored part of `mime` named `name` in `region`,
    /// plus a `final_output` region for the mirror to land in.
    fn window_with(region: &str, mime: &str, name: &str) -> ContextWindow {
        let mut window = ContextWindow::new(100_000);
        window.add_region(Region::new(region.to_string(), RegionKind::Pinned, 100_000));
        window.add_region(Region::new(
            crate::output_tool::FINAL_OUTPUT_REGION.to_string(),
            RegionKind::Pinned,
            crate::output_tool::FINAL_OUTPUT_REGION_TOKENS,
        ));
        let reg = MimeRegistry::builtin();
        let blob = Blob::new(MimeType::parse(mime).unwrap(), vec![1, 2, 3, 4]).named(name);
        let part = Part::stored(blob.describe(&reg)).named(name);
        let content = leviath_core::region::EntryContent::from_parts(vec![part]);
        let tokens = content.tokens(None);
        window
            .add_content_entry(
                leviath_core::ContextCause::ProducedPart,
                region,
                leviath_core::EntryKind::Text,
                content,
                tokens,
            )
            .unwrap();
        window
    }

    #[test]
    fn a_routed_part_satisfying_a_declared_artifact_becomes_the_output() {
        let stage = stage_routing(&[("model/*", "model")]);
        let spec = spec_with(vec![artifact_spec("model", "model/gltf-binary", true)]);
        let mut window = window_with("model", "model/gltf-binary", "hero.glb");
        let output = try_emit(&stage, Some(&spec), 42, &mut window).expect("should emit");
        assert_eq!(output.stage, "build");
        assert_eq!(output.artifacts.len(), 1);
        assert_eq!(output.artifacts[0].name, "model");
        assert_eq!(output.artifacts[0].path, "hero.glb");
        assert_eq!(output.artifacts[0].mime_type.as_str(), "model/gltf-binary");
        assert!(!output.artifacts[0].sha256.is_empty());
        assert!(output.content.contains("model"));
        // The one-line answer landed in the mirror region.
        let mirror = window
            .get_region(crate::output_tool::FINAL_OUTPUT_REGION)
            .unwrap();
        assert!(!mirror.content.is_empty());
    }

    #[test]
    fn a_stage_that_declares_no_artifacts_does_not_emit() {
        let stage = stage_routing(&[("model/*", "model")]);
        let mut window = window_with("model", "model/gltf-binary", "hero.glb");
        assert!(try_emit(&stage, Some(&spec_with(vec![])), 42, &mut window).is_none());
        assert!(try_emit(&stage, None, 42, &mut window).is_none());
    }

    #[test]
    fn a_stage_that_routes_nowhere_does_not_emit() {
        let stage = stage_routing(&[]);
        let spec = spec_with(vec![artifact_spec("model", "model/*", true)]);
        let mut window = window_with("model", "model/gltf-binary", "hero.glb");
        assert!(try_emit(&stage, Some(&spec), 42, &mut window).is_none());
    }

    #[test]
    fn a_required_artifact_with_no_matching_part_falls_back() {
        let stage = stage_routing(&[("model/*", "model")]);
        // The region holds an image, but the required artifact is a mesh.
        let spec = spec_with(vec![artifact_spec("model", "model/gltf-binary", true)]);
        let mut window = window_with("model", "image/png", "preview.png");
        assert!(try_emit(&stage, Some(&spec), 42, &mut window).is_none());
    }

    #[test]
    fn an_optional_artifact_with_no_match_is_skipped_not_fatal() {
        let stage = stage_routing(&[("model/*", "model")]);
        let spec = spec_with(vec![
            artifact_spec("model", "model/gltf-binary", true),
            artifact_spec("extra", "audio/*", false),
        ]);
        let mut window = window_with("model", "model/gltf-binary", "hero.glb");
        let output = try_emit(&stage, Some(&spec), 42, &mut window).expect("should emit");
        assert_eq!(
            output.artifacts.len(),
            1,
            "only the mesh, not the missing audio"
        );
        assert_eq!(output.artifacts[0].name, "model");
    }

    #[test]
    fn nothing_matched_at_all_does_not_emit() {
        let stage = stage_routing(&[("image/*", "model")]);
        // Only optional artifacts, none matching what was routed.
        let spec = spec_with(vec![artifact_spec("cover", "video/*", false)]);
        let mut window = window_with("model", "image/png", "cover.png");
        assert!(try_emit(&stage, Some(&spec), 42, &mut window).is_none());
    }

    /// A stored part with no name, for the declared-name fallback.
    fn unnamed_stored(mime: &str) -> Part {
        let reg = MimeRegistry::builtin();
        let blob = Blob::new(MimeType::parse(mime).unwrap(), vec![1, 2, 3]);
        Part::stored(blob.describe(&reg))
    }

    #[test]
    fn a_part_with_no_blob_is_skipped_not_matched() {
        // An inline part should never reach here, but if one did its type must
        // not stand in for a stored artifact it cannot supply.
        let specs = vec![artifact_spec("notes", "text/*", true)];
        let produced = vec![Part::text("just words")];
        assert!(match_artifacts(&specs, &produced).is_none());
    }

    #[test]
    fn an_unnamed_part_falls_back_to_the_declared_name() {
        let specs = vec![artifact_spec("model", "model/*", true)];
        let produced = vec![unnamed_stored("model/gltf-binary")];
        let records = match_artifacts(&specs, &produced).expect("matches on type");
        assert_eq!(records[0].name, "model");
        assert_eq!(
            records[0].path, "model",
            "no part name, so the declared name"
        );
    }

    #[test]
    fn two_declared_artifacts_take_two_distinct_parts() {
        let mut stage = stage_routing(&[("model/*", "model")]);
        stage
            .output_routing
            .insert("image/*".to_string(), "preview".to_string());
        let spec = spec_with(vec![
            artifact_spec("model", "model/gltf-binary", true),
            artifact_spec("preview", "image/*", true),
        ]);
        let mut window = window_with("model", "model/gltf-binary", "hero.glb");
        // Add the preview region and a second part.
        window.add_region(Region::new(
            "preview".to_string(),
            RegionKind::Pinned,
            100_000,
        ));
        let reg = MimeRegistry::builtin();
        let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![9, 9]).named("look.png");
        let part = Part::stored(blob.describe(&reg)).named("look.png");
        let content = leviath_core::region::EntryContent::from_parts(vec![part]);
        let tokens = content.tokens(None);
        window
            .add_content_entry(
                leviath_core::ContextCause::ProducedPart,
                "preview",
                leviath_core::EntryKind::Text,
                content,
                tokens,
            )
            .unwrap();
        let output = try_emit(&stage, Some(&spec), 42, &mut window).expect("should emit");
        assert_eq!(output.artifacts.len(), 2);
        let names: Vec<&str> = output.artifacts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["model", "preview"]);
    }
}
