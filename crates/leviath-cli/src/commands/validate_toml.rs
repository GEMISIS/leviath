//! `lev validate` for an `agent.toml` blueprint.
//!
//! An `agent.toml` is a run graph with a name, read by `leviath-blueprint`.
//! Validating one reads it the way a spawn would and checks that its graph
//! holds together: every stage, region and input it names is declared, and
//! every setting is in range. Every problem is printed, one per line with its
//! path. `lev blueprint migrate` writes one from an `agent.leviath`.

use std::path::{Path, PathBuf};

use leviath_blueprint::BlueprintError;
use leviath_runtime::spec::env::LoadedBlueprint;
use leviath_runtime::spec::inputs::InputDecl;

/// The `agent.toml` `path` names, when it names one: the file itself, or a
/// directory holding one and no `agent.leviath`, which keeps meaning the
/// manifest beside it.
pub(crate) fn blueprint_file(path: &Path) -> Option<PathBuf> {
    let toml = |p: &Path| {
        p.file_name()
            .is_some_and(|n| n == leviath_blueprint::FILE_NAME)
            || p.extension().is_some_and(|e| e == "toml")
    };
    match path.is_dir() {
        true => Some(path.join(leviath_blueprint::FILE_NAME)).filter(|file| {
            file.is_file() && !path.join(leviath_core::files::MANIFEST_FILENAME).exists()
        }),
        false => Some(path.to_path_buf()).filter(|p| toml(p)),
    }
}

/// What a valid blueprint is, as `--json` reports it.
#[derive(Debug, serde::Serialize)]
struct Report<'a> {
    name: &'a str,
    version: &'a str,
    stages: Vec<&'a str>,
    inputs: &'a [InputDecl],
}

/// Validate the blueprint in `file`: what to print when it holds together,
/// or every problem with it, one per line.
pub(crate) fn validate(file: &Path, json: bool) -> anyhow::Result<String> {
    let loaded = leviath_blueprint::validate(file).map_err(|e| anyhow::anyhow!(failure(&e)))?;
    Ok(report(&loaded, json))
}

/// A blueprint that did not validate, said so a person can fix it.
fn failure(error: &BlueprintError) -> String {
    match error {
        BlueprintError::Graph { path, issues } => {
            let mut lines = vec![format!(
                "✗ {} has {} problem(s):",
                path.display(),
                issues.len()
            )];
            lines.extend(issues.iter().map(|issue| format!("  {issue}")));
            lines.join("\n")
        }
        other => format!("✗ {other}"),
    }
}

/// A valid blueprint's report.
fn report(loaded: &LoadedBlueprint, json: bool) -> String {
    let graph = &loaded.graph;
    let summary = Report {
        name: loaded.reference.name.as_str(),
        version: &loaded.version,
        stages: graph.stages.iter().map(|s| s.name.as_str()).collect(),
        inputs: &graph.inputs,
    };
    if json {
        return serde_json::to_string_pretty(&summary).expect("a report serializes");
    }
    let mut lines = vec![
        format!("✓ {} {} is valid", summary.name, summary.version),
        format!("  stages: {}", summary.stages.join(", ")),
    ];
    match graph.inputs.is_empty() {
        true => lines.push("  inputs: none".to_string()),
        false => {
            lines.push("  inputs:".to_string());
            for decl in &graph.inputs {
                let required = match decl.required {
                    true => ", required",
                    false => "",
                };
                lines.push(format!(
                    "    {}: {}{required}",
                    decl.name,
                    decl.ty.describe()
                ));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `agent.toml` written from the coder manifest, in `dir`.
    fn written(dir: &Path) -> PathBuf {
        let file = dir.join(leviath_blueprint::FILE_NAME);
        let text = leviath_blueprint::migrate(&crate::test_support::inline_coder_manifest())
            .expect("the coder manifest converts");
        std::fs::write(&file, text).unwrap();
        file
    }

    #[test]
    fn a_path_names_a_toml_blueprint_by_its_file_or_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = written(dir.path());
        assert_eq!(blueprint_file(dir.path()), Some(file.clone()));
        assert_eq!(blueprint_file(&file), Some(file.clone()));
        assert_eq!(
            blueprint_file(&dir.path().join("other.toml")),
            Some(dir.path().join("other.toml"))
        );
        // A directory with a manifest beside it keeps meaning the manifest,
        // and anything else is not a toml blueprint.
        std::fs::write(dir.path().join(leviath_core::files::MANIFEST_FILENAME), "").unwrap();
        assert_eq!(blueprint_file(dir.path()), None);
        assert_eq!(
            blueprint_file(&dir.path().join(leviath_core::files::MANIFEST_FILENAME)),
            None
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(blueprint_file(empty.path()), None);
    }

    #[test]
    fn a_valid_blueprint_reports_its_stages_and_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let file = written(dir.path());
        let said = validate(&file, false).unwrap();
        assert!(said.starts_with("✓ coder"), "{said}");
        assert!(
            said.contains("stages: analyze, implement, review"),
            "{said}"
        );
        assert!(said.contains("    task: text"), "{said}");
        let json: serde_json::Value =
            serde_json::from_str(&validate(&file, true).unwrap()).unwrap();
        assert_eq!(json["name"], "coder");
        assert_eq!(json["inputs"][0]["name"], "task");

        assert!(said.lines().any(|l| l == "    task: text"), "{said}");
        let text = std::fs::read_to_string(&file).unwrap();
        let mut parsed: leviath_blueprint::BlueprintFile =
            leviath_blueprint::BlueprintFile::parse(&text).unwrap();
        parsed.graph.inputs[0].required = true;
        std::fs::write(&file, parsed.to_toml().unwrap()).unwrap();
        let said = validate(&file, false).unwrap();
        assert!(said.contains("    task: text, required"), "{said}");
        // A graph with no inputs says so.
        parsed.graph.inputs.clear();
        parsed
            .graph
            .layout
            .regions
            .retain(|r| r.name.as_str() != "task");
        std::fs::write(&file, parsed.to_toml().unwrap()).unwrap();
        let said = validate(&file, false).unwrap();
        assert!(said.contains("inputs: none"), "{said}");
    }

    /// `lev validate` takes an `agent.toml` (or its directory) and answers
    /// for it, without the manifest checks.
    #[tokio::test]
    async fn validate_takes_a_toml_blueprint() {
        let dir = tempfile::tempdir().unwrap();
        written(dir.path());
        let args = |json| super::super::ValidateArgs {
            path: dir.path().to_string_lossy().into_owned(),
            deny_warnings: false,
            json,
            graph: false,
            width: 120,
        };
        super::super::execute(args(false)).await.unwrap();
        super::super::execute(args(true)).await.unwrap();
        std::fs::write(dir.path().join(leviath_blueprint::FILE_NAME), "nope").unwrap();
        assert!(super::super::execute(args(false)).await.is_err());
    }

    #[test]
    fn every_problem_with_a_blueprint_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        let file = written(dir.path());
        let text = std::fs::read_to_string(&file).unwrap().replacen(
            "entry = \"analyze\"",
            "entry = \"nowhere\"",
            1,
        );
        std::fs::write(&file, text).unwrap();
        let err = validate(&file, false).unwrap_err().to_string();
        assert!(err.contains("problem(s):"), "{err}");
        assert!(err.contains("nowhere"), "{err}");

        std::fs::write(&file, "[blueprint]\nname = 3\n").unwrap();
        let err = validate(&file, false).unwrap_err().to_string();
        assert!(err.contains("is not a valid blueprint"), "{err}");
    }
}
