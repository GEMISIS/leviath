//! The embed spawner: turns a [`SpawnArgs`] into a live agent, the way the
//! daemon's spawner does, but from plain values instead of the CLI's config.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bevy_ecs::entity::Entity;

use super::BasicToolService;
use crate::host::SpawnArgs;
use crate::persistence::{RunMetadata, RunOutcomeFlags, TokenTotals};
use crate::pipeline::{
    ModelDefaults, PersistWatermark, Providers, resolve_stages, spawn_agent_seeded,
};
use crate::world::PipelineWorld;

/// Blueprints handed to [`AgentWorld::spawn`](super::AgentWorld::spawn)
/// inline (parsed TOML or a constructed value), parked here until the
/// spawner picks them up by their `inline:<run_id>` pseudo-path. Keeps the
/// wire-level [`SpawnArgs`] untouched.
pub(crate) type StagedBlueprints = Arc<Mutex<HashMap<String, leviath_core::Blueprint>>>;

/// Everything the embed spawner closure needs, captured once at build time.
pub(crate) struct EmbedSpawner {
    /// Registers per-agent tool state when the world runs the default
    /// service; `None` when the embedder installed a custom [`ToolService`]
    /// (which then sees agents through `exec_for` on its own terms).
    pub basic_tools: Option<Arc<BasicToolService>>,
    pub defaults: ModelDefaults,
    /// Global end of the system-prompt hint cascade, from
    /// [`AgentWorldBuilder::prompt_hints`](crate::embed::AgentWorldBuilder::prompt_hints).
    pub hints: leviath_core::config::PromptHints,
    pub staged: StagedBlueprints,
}

impl EmbedSpawner {
    /// Spawn one agent into `world` from `args`. Mirrors the daemon's
    /// `build_agent` minus its config-driven policy layers (sandboxes, MCP,
    /// script tools, taint, seed commands).
    pub(crate) fn spawn(
        &self,
        world: &mut PipelineWorld,
        args: &SpawnArgs,
    ) -> Result<Entity, String> {
        // The working directory must exist before anything is built over it,
        // or every tool call fails with a message naming the shell rather
        // than the directory.
        if !std::fs::metadata(&args.workdir).is_ok_and(|m| m.is_dir()) {
            return Err(format!(
                "workspace '{}' does not exist or is not a directory",
                args.workdir
            ));
        }

        // The blueprint: staged inline by `AgentWorld::spawn`, or a manifest
        // path to load.
        let blueprint = match args.blueprint_path.strip_prefix("inline:") {
            Some(key) => self
                .staged
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(key)
                .ok_or_else(|| format!("no staged blueprint under '{key}'"))?,
            None => {
                let content = std::fs::read_to_string(&args.blueprint_path)
                    .map_err(|e| format!("read manifest '{}': {e}", args.blueprint_path))?;
                let blueprint = leviath_core::manifest::parse_manifest(&content)
                    .map_err(|e| format!("parse manifest: {e}"))?;
                blueprint
                    .validate()
                    .map_err(|e| format!("invalid blueprint: {e}"))?;
                blueprint
            }
        };

        let seeds = resolve_embedded_seeds(&blueprint, args)?;

        // Resolve stages against the world's providers, over the tool set the
        // default service can execute. A custom service keeps the same
        // advertised set in v1; richer advertisement is part of the service
        // seam, not the spawner.
        let workdir = PathBuf::from(&args.workdir);
        let all_tool_defs = BasicToolService::tool_defs(&workdir);
        let stages = {
            let registry = &world
                .world()
                .get_resource::<Providers>()
                .expect("Providers resource present in a PipelineWorld")
                .0;
            resolve_stages(
                &blueprint,
                args.model.as_deref(),
                &self.defaults,
                registry,
                &all_tool_defs,
                args.yolo,
                args.output.as_ref(),
            )?
        };

        let agent_name = blueprint.name.clone();
        let num_stages = blueprint.stages.len();
        // Fixed for the run, so it is answered while the blueprint is in hand
        // (issue #192). Embedders get the same treatment as daemon runs.
        let outcome_flags = RunOutcomeFlags::for_blueprint(&blueprint);
        let model_label = stages
            .first()
            .map(|s| format!("{}/{}", s.provider_name, s.model));

        let entity = spawn_agent_seeded(
            world.world_mut(),
            args.run_id.clone(),
            blueprint,
            &seeds,
            stages,
            self.hints,
            // The default nudge policy; blueprints override per stage/agent.
            leviath_core::NudgeConfig::default(),
            HashMap::new(),
        )?;

        // Metadata + counters, so events, persistence, and status all work.
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        world.world_mut().entity_mut(entity).insert((
            RunMetadata {
                run_id: args.run_id.clone(),
                agent_name,
                agent_path: args.blueprint_path.clone(),
                task: args.task.clone(),
                model: model_label,
                workdir: args.workdir.clone(),
                num_stages,
                started_at,
                parent_run_id: args.parent_run_id.clone(),
                metadata: args.metadata.clone(),
                callback_url: None,
                callback_secret: None,
                title: None,
                unattended: args.yolo,
                // The embedded spawner has no user config to grant against, so
                // there is nothing to report; `[read_paths]` enforcement is the
                // host's, through the tool context it supplies.
                read_paths: None,
                output_request: args.output.clone(),
            },
            TokenTotals::default(),
            PersistWatermark::default(),
            outcome_flags,
        ));

        if let Some(tools) = &self.basic_tools {
            tools.register(entity, &args.run_id, workdir);
        }
        Ok(entity)
    }
}

/// Resolve the blueprint's region seeds from the spawn request. Embedded
/// worlds resolve `caller_input` and `literal` seeds; the file, glob, Rhai,
/// and command kinds are daemon policy (workdir scans and spawn-time
/// execution) and are skipped here - a hard error only when the region is
/// `required`, so an unused discovery nicety never sinks a run.
fn resolve_embedded_seeds(
    blueprint: &leviath_core::Blueprint,
    args: &SpawnArgs,
) -> Result<HashMap<String, String>, String> {
    use leviath_core::layout::RegionSeed;

    let mut caller: HashMap<&str, &str> = HashMap::new();
    caller.insert("task", &args.task);
    for (k, v) in &args.regions {
        caller.insert(k, v);
    }

    let mut seeds = HashMap::new();
    for region in &blueprint.context_layout.regions {
        let Some(seed) = &region.seed else { continue };
        match seed {
            RegionSeed::CallerInput { name } => {
                let value = caller.get(name.as_str()).copied().unwrap_or("");
                if value.trim().is_empty() {
                    if region.required {
                        return Err(region.required_message.clone().unwrap_or_else(|| {
                            format!(
                                "required region '{}' was not provided; supply it in \
                                 SpawnSpec::regions under the key '{name}'",
                                region.name
                            )
                        }));
                    }
                    continue;
                }
                seeds.insert(region.name.clone(), value.to_string());
            }
            RegionSeed::Literal { text } => {
                seeds.insert(region.name.clone(), text.clone());
            }
            other => {
                if region.required {
                    return Err(format!(
                        "required region '{}' uses a seed kind ({}) embedded worlds \
                         do not resolve; provide the content via SpawnSpec::regions",
                        region.name,
                        seed_kind_name(other)
                    ));
                }
            }
        }
    }
    Ok(seeds)
}

/// A short label for the unsupported seed kinds, for error messages.
fn seed_kind_name(seed: &leviath_core::layout::RegionSeed) -> &'static str {
    use leviath_core::layout::RegionSeed;
    match seed {
        RegionSeed::CallerInput { .. } => "caller_input",
        RegionSeed::Literal { .. } => "literal",
        RegionSeed::Glob { .. } => "glob",
        RegionSeed::Files { .. } => "files",
        RegionSeed::Rhai { .. } => "rhai",
        RegionSeed::Command { .. } => "command",
    }
}

/// Mint a run id: `<stem>-<unix-secs>-<counter>`. The per-process counter
/// keeps ids unique even when several spawns land in the same second.
pub(crate) fn mint_run_id(stem: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem: String = stem
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() {
            true => c.to_ascii_lowercase(),
            false => '-',
        })
        .collect();
    let stem = match stem.is_empty() {
        true => "agent".to_string(),
        false => stem,
    };
    format!("{stem}-{secs}-{n:04x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::layout::{ContextLayout, RegionDefinition, RegionSeed};
    use leviath_core::region::RegionKind;

    fn region(name: &str, seed: Option<RegionSeed>, required: bool) -> RegionDefinition {
        let mut r = RegionDefinition::new(name.to_string(), RegionKind::Pinned, 1000);
        r.seed = seed;
        r.required = required;
        r
    }

    fn bp(regions: Vec<RegionDefinition>) -> leviath_core::Blueprint {
        let s = leviath_core::Stage::new(
            "s".to_string(),
            leviath_core::blueprint::ModelConfig::new("mock".to_string(), "m".to_string()),
        );
        leviath_core::Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![s],
            ContextLayout::new(regions, 10_000),
        )
    }

    fn args(task: &str, regions: &[(&str, &str)]) -> SpawnArgs {
        SpawnArgs {
            task: task.to_string(),
            regions: regions
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn seeds_fill_task_literal_and_named_regions() {
        let bp = bp(vec![
            region(
                "task",
                Some(RegionSeed::CallerInput {
                    name: "task".to_string(),
                }),
                true,
            ),
            region(
                "criteria",
                Some(RegionSeed::CallerInput {
                    name: "criteria".to_string(),
                }),
                false,
            ),
            region(
                "guide",
                Some(RegionSeed::Literal {
                    text: "be kind".to_string(),
                }),
                false,
            ),
            region("plain", None, false),
        ]);
        let seeds =
            resolve_embedded_seeds(&bp, &args("do the thing", &[("criteria", "fast")])).unwrap();
        assert_eq!(seeds["task"], "do the thing");
        assert_eq!(seeds["criteria"], "fast");
        assert_eq!(seeds["guide"], "be kind");
        assert!(!seeds.contains_key("plain"));
    }

    #[test]
    fn missing_optional_caller_input_is_left_empty() {
        let bp = bp(vec![region(
            "notes",
            Some(RegionSeed::CallerInput {
                name: "notes".to_string(),
            }),
            false,
        )]);
        let seeds = resolve_embedded_seeds(&bp, &args("t", &[])).unwrap();
        assert!(seeds.is_empty());
    }

    #[test]
    fn missing_required_caller_input_fails_before_any_inference() {
        let bp = bp(vec![region(
            "spec",
            Some(RegionSeed::CallerInput {
                name: "spec".to_string(),
            }),
            true,
        )]);
        let err = resolve_embedded_seeds(&bp, &args("t", &[])).unwrap_err();
        assert!(err.contains("required region 'spec'"));
        assert!(err.contains("'spec'"));
    }

    #[test]
    fn required_message_overrides_the_default_error() {
        let mut r = region(
            "spec",
            Some(RegionSeed::CallerInput {
                name: "spec".to_string(),
            }),
            true,
        );
        r.required_message = Some("bring a spec".to_string());
        let err = resolve_embedded_seeds(&bp(vec![r]), &args("t", &[])).unwrap_err();
        assert_eq!(err, "bring a spec");
    }

    #[test]
    fn unsupported_seed_kinds_skip_unless_required() {
        let optional = bp(vec![region(
            "ctx",
            Some(RegionSeed::Glob {
                pattern: "*.md".to_string(),
            }),
            false,
        )]);
        assert!(
            resolve_embedded_seeds(&optional, &args("t", &[]))
                .unwrap()
                .is_empty()
        );

        let required = bp(vec![region(
            "ctx",
            Some(RegionSeed::Files {
                paths: vec!["a.md".to_string()],
            }),
            true,
        )]);
        let err = resolve_embedded_seeds(&required, &args("t", &[])).unwrap_err();
        assert!(err.contains("seed kind (files)"));
    }

    #[test]
    fn seed_kind_names_cover_every_variant() {
        use leviath_core::layout::RegionSeed;
        let cases = [
            (
                RegionSeed::CallerInput {
                    name: "n".to_string(),
                },
                "caller_input",
            ),
            (
                RegionSeed::Literal {
                    text: "t".to_string(),
                },
                "literal",
            ),
            (
                RegionSeed::Glob {
                    pattern: "p".to_string(),
                },
                "glob",
            ),
            (RegionSeed::Files { paths: vec![] }, "files"),
            (
                RegionSeed::Rhai {
                    script: "s".to_string(),
                },
                "rhai",
            ),
        ];
        for (seed, want) in cases {
            assert_eq!(seed_kind_name(&seed), want);
        }
    }

    #[test]
    fn seed_kind_names_include_command() {
        assert_eq!(
            seed_kind_name(&RegionSeed::Command {
                command: "ls".to_string(),
            }),
            "command"
        );
    }

    /// A provider that never answers - enough to be *registered*, which is all
    /// stage resolution asks of it.
    struct StubProvider;

    #[async_trait::async_trait]
    impl leviath_providers::Provider for StubProvider {
        async fn infer(
            &self,
            _request: leviath_providers::InferenceRequest,
        ) -> Result<leviath_providers::InferenceResponse, leviath_providers::ProviderError>
        {
            Err(leviath_providers::ProviderError::ApiError(
                "stub".to_string(),
            ))
        }
        async fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len()
        }
        fn max_context_tokens(&self, _model: &str) -> usize {
            8192
        }
        fn name(&self) -> &str {
            "mock"
        }
        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    #[tokio::test]
    async fn stub_provider_metadata_is_exercised() {
        use leviath_providers::Provider as _;
        let p = StubProvider;
        assert_eq!(p.name(), "mock");
        assert_eq!(p.count_tokens("abc", "m").await, 3);
        assert_eq!(p.max_context_tokens("m"), 8192);
        let _ = p.capabilities("m");
        assert!(
            p.infer(leviath_providers::InferenceRequest {
                system: vec![],
                messages: vec![],
                model: "m".to_string(),
                max_tokens: 1,
                temperature: 0.0,
                tools: vec![],
                extra: serde_json::Value::Null,
                request_timeout_secs: None,
            })
            .await
            .is_err()
        );
    }

    /// A world whose registry has the `mock` provider these manifests name, so
    /// a spawn gets past stage resolution to whatever it is actually testing.
    fn pipeline_world() -> crate::world::PipelineWorld {
        let mut registry = crate::providers::ProviderRegistry::new();
        registry.register("mock".to_string(), Arc::new(StubProvider));
        crate::world::PipelineWorld::new(
            registry,
            Arc::new(super::super::BasicToolService::new(
                crate::interaction_hub::InteractionHub::new(),
            )),
            crate::inference_pool::InferencePoolConfig::new(),
            1,
            None,
            tokio::runtime::Handle::current(),
        )
    }

    fn spawner() -> EmbedSpawner {
        EmbedSpawner {
            basic_tools: None,
            defaults: ModelDefaults::default(),
            hints: leviath_core::config::PromptHints::default(),
            staged: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[tokio::test]
    async fn a_missing_staged_blueprint_is_a_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut world = pipeline_world();
        let err = spawner()
            .spawn(
                &mut world,
                &SpawnArgs {
                    run_id: "r".to_string(),
                    blueprint_path: "inline:ghost".to_string(),
                    workdir: dir.path().to_string_lossy().into_owned(),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.contains("no staged blueprint"));
    }

    #[tokio::test]
    async fn a_manifest_that_parses_but_fails_validation_is_a_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        // Valid TOML whose entry_stage names no stage: parses, fails validate.
        let manifest = r#"[agent]
name = "x"
version = "0.0.0"
description = "d"
entry_stage = "ghost"

[stages.only]
mode = "autonomous"
model = { provider = "mock", model = "m" }
description = "Only stage"

[context.regions]
conversation = { kind = "sliding_window", max_items = 10, max_tokens = 2000 }
"#;
        let path = dir.path().join("invalid.leviath");
        std::fs::write(&path, manifest).unwrap();
        let mut world = pipeline_world();
        let err = spawner()
            .spawn(
                &mut world,
                &SpawnArgs {
                    run_id: "r".to_string(),
                    blueprint_path: path.to_string_lossy().into_owned(),
                    workdir: dir.path().to_string_lossy().into_owned(),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.contains("invalid blueprint"));
    }

    #[tokio::test]
    async fn a_stage_with_no_registered_provider_is_a_spawn_error() {
        // Issue #190: an embedder that never registered the provider a stage
        // names used to get a live agent that could not take a single turn.
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"[agent]
name = "ghostly"
version = "0.0.0"
description = "d"

[stages.only]
mode = "autonomous"
model = { provider = "ghost", model = "m" }
description = "Only stage"

[context.regions]
conversation = { kind = "sliding_window", max_items = 10, max_tokens = 2000 }
"#;
        let path = dir.path().join("ghostly.leviath");
        std::fs::write(&path, manifest).unwrap();
        let mut world = pipeline_world();
        let err = spawner()
            .spawn(
                &mut world,
                &SpawnArgs {
                    run_id: "r".to_string(),
                    blueprint_path: path.to_string_lossy().into_owned(),
                    workdir: dir.path().to_string_lossy().into_owned(),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.contains("no usable provider"), "got: {err}");
        assert!(err.contains("ghost"), "got: {err}");
    }

    #[tokio::test]
    async fn an_unspawnable_blueprint_surfaces_the_seeded_spawn_error() {
        // A pinned region far too small for the stage's system prompt: the
        // world-level spawn (spawn_agent_seeded) rejects it.
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"[agent]
name = "tiny"
version = "0.0.0"
description = "Tiny region."

[stages.only]
mode = "autonomous"
model = { provider = "mock", model = "m" }
description = "Only stage"
system_prompt = "This stage instruction text is far larger than the one-token region it must be pinned into, so applying the stage context fails."

[context.regions]
system = { kind = "pinned", max_tokens = 1 }
"#;
        let path = dir.path().join("tiny.leviath");
        std::fs::write(&path, manifest).unwrap();
        let mut world = pipeline_world();
        let result = spawner().spawn(
            &mut world,
            &SpawnArgs {
                run_id: "r".to_string(),
                blueprint_path: path.to_string_lossy().into_owned(),
                workdir: dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn minted_run_ids_are_sanitized_and_unique() {
        let a = mint_run_id("My Coder!");
        let b = mint_run_id("My Coder!");
        assert!(a.starts_with("my-coder-"));
        assert_ne!(a, b);
        assert!(mint_run_id("").starts_with("agent-"));
    }
}
