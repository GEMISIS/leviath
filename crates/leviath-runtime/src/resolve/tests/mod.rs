//! A fake machine for the resolver, and the tests that drive it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use async_trait::async_trait;

use super::{ResolveMode, Resolved, resolve};
use crate::spec::env::{
    Caller, CodeFiles, CodeUse, LoadedBlueprint, ModelPlan, ResolveEnv, SeedCx, SpawnLimits,
};
use crate::spec::graph::{CodeRef, DependencyDef, RunGraph, Seed, StageDef};
use crate::spec::inputs::{InputDecl, InputSlot, InputType, PathKind, RawInput, RegionBinding};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::{
    BlueprintRef, Digest, McpServerName, MimePattern, ModelRef, ProviderName, RunId, WorkdirPath,
};
use crate::spec::request::{SpawnRequest, SpawnSource};
use crate::spec::run_spec::{SeededContent, ToolDef, ToolSource};

mod attachments;
mod basics;
mod code;
mod launch;
mod outputs;
mod regions;
mod seeds;
mod stages;

/// A machine whose every answer a test can set.
pub(super) struct Fake {
    pub(super) blueprints: BTreeMap<String, LoadedBlueprint>,
    pub(super) limits: SpawnLimits,
    pub(super) workdir: Result<PathBuf, String>,
    pub(super) existing: BTreeSet<String>,
    pub(super) models: BTreeMap<String, Result<ModelPlan, SpawnIssue>>,
    pub(super) tools: BTreeMap<String, Result<Vec<ToolDef>, SpawnIssues>>,
    pub(super) files: BTreeMap<String, Vec<u8>>,
    pub(super) failing_deps: BTreeMap<String, String>,
    pub(super) printed: BTreeSet<String>,
    pub(super) seeds_run: Mutex<Vec<Seed>>,
    pub(super) asked: Mutex<Vec<(String, Option<ModelRef>)>>,
    pub(super) titles: Mutex<Vec<String>>,
}

impl Default for Fake {
    fn default() -> Self {
        Self {
            blueprints: BTreeMap::new(),
            limits: SpawnLimits {
                default_max_depth: 3,
                seed_commands_allowed: true,
                max_attachment_bytes: 1024,
            },
            workdir: Ok(PathBuf::from("/work")),
            existing: BTreeSet::new(),
            models: BTreeMap::new(),
            tools: BTreeMap::new(),
            files: BTreeMap::new(),
            failing_deps: BTreeMap::new(),
            printed: BTreeSet::new(),
            seeds_run: Mutex::new(Vec::new()),
            asked: Mutex::new(Vec::new()),
            titles: Mutex::new(Vec::new()),
        }
    }
}

impl Fake {
    pub(super) fn seeds_run(&self) -> Vec<Seed> {
        self.seeds_run.lock().unwrap().clone()
    }

    pub(super) fn asked(&self) -> Vec<(String, Option<ModelRef>)> {
        self.asked.lock().unwrap().clone()
    }
}

/// A model plan on the fake's usual provider.
pub(super) fn plan(provider: &str, model: &str, window: u32) -> ModelPlan {
    ModelPlan {
        provider: n(provider),
        model: n(model),
        context_window: window,
        fallbacks: Vec::new(),
        notes: Vec::new(),
    }
}

#[async_trait]
impl ResolveEnv for Fake {
    async fn blueprint(&self, reference: &BlueprintRef) -> Result<LoadedBlueprint, SpawnIssue> {
        self.blueprints
            .get(reference.name.as_str())
            .cloned()
            .ok_or_else(|| {
                SpawnIssue::new(
                    SpecPath::root().field("name"),
                    IssueCode::Unknown,
                    "no such blueprint",
                )
                .known(self.blueprints.keys())
            })
    }

    fn limits(&self) -> SpawnLimits {
        self.limits.clone()
    }

    fn new_run_id(&self, title: &str) -> RunId {
        self.titles.lock().unwrap().push(title.to_string());
        RunId::new(format!("{title}-1")).unwrap()
    }

    fn workdir(&self, requested: Option<&Path>) -> Result<PathBuf, String> {
        match requested {
            Some(dir) if dir == Path::new("/nope") => Err("no such directory".to_string()),
            Some(dir) => Ok(dir.to_path_buf()),
            None => self.workdir.clone(),
        }
    }

    fn path_exists(&self, _workdir: &Path, path: &WorkdirPath, _kind: PathKind) -> bool {
        self.existing.contains(path.as_str())
    }

    async fn model(
        &self,
        stage: &StageDef,
        requested: Option<&ModelRef>,
    ) -> Result<ModelPlan, SpawnIssue> {
        self.asked
            .lock()
            .unwrap()
            .push((stage.name.to_string(), requested.cloned()));
        if let Some(answer) = self.models.get(stage.name.as_str()) {
            return answer.clone();
        }
        let chosen = requested.or(stage.model.models.first());
        Ok(ModelPlan {
            provider: chosen
                .and_then(|m| m.provider.clone())
                .unwrap_or_else(|| n("mock")),
            model: chosen.map_or_else(|| n("gpt-mock"), |m| m.model.clone()),
            context_window: 100_000,
            fallbacks: Vec::new(),
            notes: Vec::new(),
        })
    }

    async fn tools(
        &self,
        _graph: &RunGraph,
        stage: &StageDef,
        _code: &CodeFiles,
    ) -> Result<Vec<ToolDef>, SpawnIssues> {
        self.tools
            .get(stage.name.as_str())
            .cloned()
            .unwrap_or(Ok(Vec::new()))
    }

    async fn code(&self, code: &CodeRef, base: Option<&Path>) -> Result<Vec<u8>, String> {
        match code {
            CodeRef::Inline(source) => Ok(source.as_bytes().to_vec()),
            CodeRef::File(file) => {
                assert!(base.is_some(), "a file is only read beside a blueprint");
                self.files
                    .get(file)
                    .cloned()
                    .ok_or_else(|| format!("cannot read '{file}'"))
            }
        }
    }

    fn check_code(&self, code: &[u8], used_as: CodeUse) -> Result<(), String> {
        let text = String::from_utf8_lossy(code);
        match (text.contains("broken"), text.contains("nohook"), used_as) {
            (true, _, _) => Err("does not compile".to_string()),
            (_, true, CodeUse::Hook) => Err("defines no hook".to_string()),
            _ => Ok(()),
        }
    }

    async fn seed(&self, seed: &Seed, cx: SeedCx<'_>) -> Result<SeededContent, String> {
        self.seeds_run.lock().unwrap().push(seed.clone());
        let text = match seed {
            Seed::Literal(text) => text.clone(),
            Seed::Command(command) if command == "fail" => return Err("exit 1".to_string()),
            Seed::Command(command) => format!("ran {command} in {}", cx.workdir.display()),
            Seed::Glob(pattern) => format!("glob {pattern}"),
            Seed::Files(files) => format!("{} files", files.len()),
            Seed::Code(_) => format!("code saw {} inputs", cx.inputs.0.len()),
            Seed::Tools { calls, .. } => format!("{} tool calls", calls.len()),
        };
        Ok(SeededContent {
            text,
            parts: Vec::new(),
        })
    }

    fn sniff(
        &self,
        name: &str,
        _bytes: &[u8],
        declared: Option<&MimePattern>,
    ) -> Result<String, String> {
        if name.ends_with(".bad") {
            return Err("the bytes are not any known type".to_string());
        }
        if name.ends_with(".weird") {
            return Ok("not a type".to_string());
        }
        Ok(match (declared, name.rsplit_once('.')) {
            (Some(declared), _) => declared.to_string(),
            (None, Some((_, "png"))) => "image/png".to_string(),
            _ => "text/plain".to_string(),
        })
    }

    async fn dependency(&self, dependency: &DependencyDef) -> Result<(), String> {
        match self.failing_deps.get(&dependency.name) {
            Some(message) => Err(message.clone()),
            None => Ok(()),
        }
    }

    fn provider_fingerprint(&self, provider: &ProviderName) -> Option<Digest> {
        self.printed
            .contains(provider.as_str())
            .then(|| Digest::of(provider.as_str().as_bytes()))
    }

    fn mcp_fingerprint(&self, server: &McpServerName) -> Option<Digest> {
        self.printed
            .contains(server.as_str())
            .then(|| Digest::of(server.as_str().as_bytes()))
    }
}

/// A checked name, for tests.
pub(super) fn n<T>(text: &str) -> T
where
    T: FromStr,
    T::Err: std::fmt::Debug,
{
    text.parse().unwrap()
}

/// A text input bound to a region.
pub(super) fn text_input(name: &str, region: &str) -> InputDecl {
    InputDecl {
        name: n(name),
        ty: InputType::Text {
            multiline: true,
            min_len: None,
            max_len: None,
        },
        required: false,
        default: None,
        description: None,
        binds: vec![InputSlot::Region(RegionBinding {
            region: n(region),
            template: None,
        })],
    }
}

/// The two-stage test graph, taking a `task` into its `task` region.
pub(super) fn graph() -> RunGraph {
    let mut g = crate::spec::graph::tests::minimal();
    g.inputs.push(text_input("task", "task"));
    g
}

/// A request for `graph`, with a task.
pub(super) fn raw(graph: RunGraph) -> SpawnRequest {
    SpawnRequest::new(SpawnSource::Raw(Box::new(graph)))
        .input("task", RawInput::Text("do the thing".into()))
}

/// Resolve a top-level spawn.
pub(super) async fn spawn(request: &SpawnRequest, env: &Fake) -> Result<Resolved, SpawnIssues> {
    resolve(request, &Caller::TopLevel, env, ResolveMode::Spawn).await
}

/// Each issue as `path code`, for comparing.
pub(super) fn found(issues: &SpawnIssues) -> Vec<String> {
    issues
        .iter()
        .map(|i| format!("{} {:?}", i.path, i.code))
        .collect()
}

/// A built-in tool definition.
pub(super) fn tool(name: &str) -> ToolDef {
    ToolDef {
        name: n(name),
        description: format!("{name} does things"),
        schema: leviath_core::JsonDoc::default(),
        source: ToolSource::Builtin,
    }
}

/// A model reference.
pub(super) fn model(text: &str) -> ModelRef {
    ModelRef::parse(text).unwrap()
}
