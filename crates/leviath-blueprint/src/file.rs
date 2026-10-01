//! The `agent.toml` file: what a blueprint is called, and its run graph.

use leviath_runtime::spec::graph::RunGraph;
use leviath_runtime::spec::names::BlueprintName;
use serde::{Deserialize, Serialize};

/// The name of a blueprint's file inside its directory.
pub const FILE_NAME: &str = "agent.toml";

/// What a blueprint says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlueprintMeta {
    /// The name it is installed and run by.
    pub name: BlueprintName,
    /// Its version, bumped when its graph changes.
    pub version: String,
    /// What it does, in a sentence or two.
    #[serde(default)]
    pub description: Option<String>,
}

/// Only the `[blueprint]` table of a file, for a reader that needs the name
/// of a blueprint whose graph it does not check.
#[derive(Deserialize)]
struct Head {
    blueprint: BlueprintMeta,
}

impl BlueprintMeta {
    /// Read the `[blueprint]` table of an `agent.toml`, whatever its graph
    /// says. An installer names a blueprint's directory by this before it has
    /// any reason to read the rest.
    pub fn read(text: &str) -> Result<Self, String> {
        toml::from_str::<Head>(text)
            .map(|head| head.blueprint)
            .map_err(|e| e.to_string())
    }
}

/// A whole `agent.toml`.
///
/// `[graph]` is a [`RunGraph`] exactly as a raw spawn request carries one.
/// The graph's `title` and `description` may be left out: a graph read from
/// the file takes them from `[blueprint]` (see [`BlueprintFile::run_graph`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlueprintFile {
    /// What the blueprint is called.
    pub blueprint: BlueprintMeta,
    /// The run it describes.
    pub graph: RunGraph,
}

impl BlueprintFile {
    /// Read a blueprint from the text of an `agent.toml`. Unknown keys are
    /// refused, and the message names the line and the key.
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// The run graph, with its title and description filled in from
    /// `[blueprint]` where the graph leaves them out.
    pub fn run_graph(&self) -> RunGraph {
        let mut graph = self.graph.clone();
        if graph.title.is_none() {
            graph.title = Some(self.blueprint.name.to_string());
        }
        if graph.description.is_none() {
            graph.description = self.blueprint.description.clone();
        }
        graph
    }

    /// The file as readable TOML: keys holding their default are left out,
    /// short tables are written inline, and reading the text back gives this
    /// same file.
    pub fn to_toml(&self) -> Result<String, String> {
        crate::write::render(self)
    }
}
