//! Where the boxes were left.
//!
//! An arrangement dragged into shape on the editor's canvas is worth keeping,
//! but it is not part of the agent: The Lair keeps it in the browser, keyed by
//! agent name, and this keeps it in one small file per home the same way.
//! Positions are world coordinates on the canvas; a stage without one is laid
//! out by the layered layout as usual.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Stage name to `(x, y)` on the canvas.
pub type Positions = BTreeMap<String, (f64, f64)>;

/// The saved arrangements, keyed by agent name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayoutStore {
    path: Option<PathBuf>,
    layouts: BTreeMap<String, Positions>,
}

impl LayoutStore {
    /// The file under the data directory: `dash/graph-layouts.json`.
    pub fn default_path() -> Option<PathBuf> {
        leviath_core::paths::data_dir().map(|d| d.join("dash").join("graph-layouts.json"))
    }

    /// Read the store at `path`; a missing or unreadable file is an empty
    /// store that will write there on the next save.
    pub fn open(path: PathBuf) -> Self {
        let layouts = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            layouts,
        }
    }

    /// A store that never touches disk.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// The file this store writes to, when it has one.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The saved positions of an agent's stages.
    pub fn positions(&self, agent: &str) -> Option<&Positions> {
        self.layouts.get(agent)
    }

    /// Remember an agent's arrangement (in memory; `save` writes it).
    pub fn set(&mut self, agent: &str, positions: Positions) {
        if positions.is_empty() {
            self.layouts.remove(agent);
        } else {
            self.layouts.insert(agent.to_string(), positions);
        }
    }

    /// Drop an agent's arrangement (a deleted or reset agent).
    pub fn forget(&mut self, agent: &str) {
        self.layouts.remove(agent);
    }

    /// Carry an arrangement over to a new name (a cloned agent).
    pub fn copy(&mut self, from: &str, to: &str) {
        if let Some(p) = self.layouts.get(from).cloned() {
            self.layouts.insert(to.to_string(), p);
        }
    }

    /// Write the store, creating its directory. A store without a path does
    /// nothing.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        // A relative file name has an empty parent, which `create_dir_all`
        // treats as already there.
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("")))?;
        let text =
            serde_json::to_string_pretty(&self.layouts).expect("a map of numbers serializes");
        std::fs::write(path, text)
    }
}
