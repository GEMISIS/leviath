//! Building a [`SpawnRequest`] from the task-and-flags shape the front doors
//! take today: a blueprint named by path or installed name, a task, text for
//! named regions, attached files, a model, and the launch flags.
//!
//! Each front door (`lev run`, the HTTP API, GraphQL, the dashboard, the agent
//! client, `lev doctor`, the `spawn_agent` tool) fills a [`TaskLaunch`] in one
//! function of its own and turns it into a request here, so the request each
//! sends is the same whatever the door.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use leviath_runtime::spec::graph::OutputDef;
use leviath_runtime::spec::inputs::RawInput;
use leviath_runtime::spec::launch::{Callback, Delivery, LaunchRequest, Secret, Unattended};
use leviath_runtime::spec::names::{
    BlueprintName, BlueprintPath, BlueprintRef, HttpUrl, ModelRef, ProfileName, ToolName,
};
use leviath_runtime::spec::request::{Attachment, Bytes, SpawnRequest, SpawnSource};

/// A run asked for as a task and flags.
#[derive(Debug, Clone, Default)]
pub(crate) struct TaskLaunch {
    /// The blueprint: an installed name, or a path to a manifest or the
    /// directory holding one.
    pub blueprint: String,
    /// The task. Sent as the `task` input when it says anything.
    pub task: String,
    /// Text for named regions, each sent as the input of that name.
    pub regions: HashMap<String, String>,
    /// Files sent with the run.
    pub parts: Vec<leviath_core::mime::InboundPart>,
    /// The model every stage that allows one runs on.
    pub model: Option<String>,
    /// Where the run's tools work. Made absolute against the current
    /// directory when relative.
    pub workdir: Option<String>,
    /// Run without a person.
    pub unattended: bool,
    /// The yolo profile that says which parts of unattended a person keeps.
    /// Ignored for an attended run.
    pub profile: Option<String>,
    /// Tools approved for this run without asking.
    pub allow: Vec<String>,
    /// How deep the run's tree of child runs may grow.
    pub max_depth: Option<usize>,
    /// Refuse the blueprint's shell-command seeds.
    pub no_seed_commands: bool,
    /// The shape the caller wants the final output in.
    pub output: Option<leviath_core::output::OutputSpec>,
    /// Record every request sent to the model.
    pub capture_model_input: bool,
    /// The caller's labels for the run.
    pub metadata: HashMap<String, String>,
    /// A webhook to call when the run ends.
    pub callback_url: Option<String>,
    /// The secret the webhook body is signed with.
    pub callback_secret: Option<String>,
}

/// What a blueprint string names. An installed blueprint is named by its
/// name, and so is a path into the installed blueprints; any other path to a
/// manifest, or to the directory holding one, names that directory, which
/// the daemon reads the blueprint and the files beside it from.
pub(crate) fn blueprint_source(blueprint: &str) -> Result<SpawnSource, String> {
    // A bare name, not a path that happens to point into the agents dir.
    let bare = !blueprint.contains(['/', '\\']);
    let installed = leviath_core::paths::agents_dir().filter(|dir| {
        bare && dir
            .join(blueprint)
            .join(leviath_blueprint::FILE_NAME)
            .is_file()
    });
    if installed.is_some()
        && let Ok(reference) = BlueprintRef::parse(blueprint)
    {
        return Ok(SpawnSource::Blueprint(reference));
    }
    let manifest =
        crate::commands::run::locate::find_blueprint(blueprint).map_err(|e| e.to_string())?;
    let manifest = std::fs::canonicalize(&manifest).unwrap_or(manifest);
    let dir = manifest.parent().map(Path::to_path_buf).unwrap_or_default();
    // A path into the installed blueprints names the installed blueprint.
    let agents = leviath_core::paths::agents_dir().and_then(|d| std::fs::canonicalize(d).ok());
    let named = match agents.as_deref() == dir.parent() {
        true => dir.file_name().map(|n| n.to_string_lossy().into_owned()),
        false => None,
    };
    let source = match named {
        Some(name) => BlueprintName::new(name.as_str())
            .map(|name| SpawnSource::Blueprint(BlueprintRef { name, digest: None })),
        None => BlueprintPath::new(dir.to_string_lossy()).map(SpawnSource::BlueprintFile),
    };
    source.map_err(|e| format!("blueprint '{blueprint}': {e}"))
}

/// A file sent with a run, as the request carries it.
pub(crate) fn attachment(part: leviath_core::mime::InboundPart) -> Attachment {
    Attachment {
        name: part.name,
        mime_type: part
            .mime_type
            .and_then(|t| leviath_runtime::spec::names::MimePattern::new(t.as_str()).ok()),
        region: part
            .region
            .and_then(|r| leviath_runtime::spec::names::RegionName::new(r.as_str()).ok()),
        deliver: part.deliver,
        caption: part.caption,
        data: Bytes(part.data),
    }
}

/// `dir` made absolute against the current directory.
fn absolute(dir: &str) -> PathBuf {
    let path = PathBuf::from(dir);
    match path.is_absolute() {
        true => path,
        false => std::env::current_dir().unwrap_or_default().join(path),
    }
}

impl TaskLaunch {
    /// The request this launch asks for. `Err` names the flag that does not
    /// read.
    pub(crate) fn into_request(self) -> Result<SpawnRequest, String> {
        let source = blueprint_source(&self.blueprint)?;
        self.into_request_for(source)
    }

    /// The request this launch asks for, of `source`.
    pub(crate) fn into_request_for(self, source: SpawnSource) -> Result<SpawnRequest, String> {
        let mut request = SpawnRequest::new(source);
        if !self.task.trim().is_empty() {
            request = request.input("task", RawInput::Text(self.task));
        }
        for (name, text) in self.regions {
            request = request.input(name, RawInput::Text(text));
        }
        request.attachments = self.parts.into_iter().map(attachment).collect();
        request.model = self
            .model
            .filter(|m| !m.trim().is_empty())
            .map(|m| ModelRef::parse(&m).map_err(|e| format!("model '{m}': {e}")))
            .transpose()?;
        request.output = self
            .output
            .as_ref()
            .map(OutputDef::from_output_spec)
            .transpose()
            .map_err(|issues| issues.to_string())?;
        request.workdir = self.workdir.as_deref().map(absolute);
        // A profile says which parts of an unattended run a person keeps; an
        // attended run keeps them all, so it ignores one.
        let unattended = match (self.unattended, self.profile) {
            (false, _) => Unattended::Off,
            (true, Some(profile)) if !profile.is_empty() => Unattended::Profile(
                ProfileName::new(profile.as_str())
                    .map_err(|e| format!("yolo profile '{profile}': {e}"))?,
            ),
            (true, _) => Unattended::All,
        };
        request.launch = LaunchRequest {
            unattended,
            allow: self
                .allow
                .iter()
                .map(|t| ToolName::new(t.as_str()).map_err(|e| format!("allow '{t}': {e}")))
                .collect::<Result<_, _>>()?,
            max_depth: self.max_depth.map(|d| u8::try_from(d).unwrap_or(u8::MAX)),
            seed_commands: !self.no_seed_commands,
            capture_model_input: self.capture_model_input,
        };
        let callback = self
            .callback_url
            .map(|url| {
                Ok::<_, String>(Callback {
                    url: HttpUrl::new(url.as_str())
                        .map_err(|e| format!("callback url '{url}': {e}"))?,
                    secret: self.callback_secret.map(Secret::new),
                })
            })
            .transpose()?;
        request.delivery = Delivery {
            callback,
            metadata: self.metadata.into_iter().collect(),
        };
        Ok(request)
    }
}

#[cfg(test)]
#[path = "requests_tests.rs"]
mod tests;
