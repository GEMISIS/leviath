//! The wire shape of a file a run produced.

use serde::{Deserialize, Serialize};

/// One file a run produced, as the API reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactResp {
    /// The name the stage declared, or the file name.
    pub name: String,
    /// The file, relative to the run's working directory.
    pub path: String,
    /// The file's mime type.
    pub mime_type: String,
    /// Size in bytes.
    pub size: u64,
    /// The sha256 the run's blob store holds it under; empty when it was too
    /// large to store.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
}

impl From<leviath_core::output::Artifact> for ArtifactResp {
    fn from(a: leviath_core::output::Artifact) -> Self {
        Self {
            name: a.name,
            path: a.path,
            mime_type: a.mime_type.to_string(),
            size: a.size,
            sha256: a.sha256,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_artifact_reports_its_type_as_text_and_omits_an_empty_hash() {
        let resp = ArtifactResp::from(leviath_core::output::Artifact::from_path("a/b.csv"));
        assert_eq!(resp.name, "b.csv");
        assert_eq!(resp.mime_type, "application/octet-stream");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("sha256"), "{json}");
    }
}
