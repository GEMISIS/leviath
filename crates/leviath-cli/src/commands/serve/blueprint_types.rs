//! The wire shape of a blueprint's per-stage typed-content routing.
//!
//! In a file of its own rather than beside the rest of the blueprint response
//! types so `types.rs` stays under its production-line cap.

use serde::Serialize;

/// One mime pattern routed to a region, in a [`StageRoutingInfo`].
#[derive(Debug, Serialize)]
pub(super) struct RoutePair {
    /// The mime pattern the model's produced parts are matched against
    /// (`image/*`, `application/pdf`, `*/*`).
    pub(super) pattern: String,
    /// The region a matching part is written to.
    pub(super) region: String,
}

/// A stage's typed-content routing, so a console need not parse the manifest
/// to show or check it. Only stages that route produced parts or reset a
/// region on entry appear; a stage with neither is left out.
#[derive(Debug, Serialize)]
pub(super) struct StageRoutingInfo {
    /// The stage's name.
    pub(super) stage: String,
    /// `output_routing`: where the model's produced parts go, by mime pattern,
    /// ordered by pattern. Empty (and omitted) when the stage routes none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) output_routing: Vec<RoutePair>,
    /// `context.reset`: the regions the stage empties on entry. Empty (and
    /// omitted) when it resets none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) context_reset: Vec<String>,
}

/// One blueprint, with the manifest text behind it.
///
/// The detail route only. A listing that carried one of these per blueprint
/// would send every manifest on the machine to answer "what agents are there",
/// which is why this is a separate shape rather than an extra field on
/// [`BlueprintInfo`](super::types::BlueprintInfo).
///
/// Flattened, so the detail route's JSON is the info's own fields plus
/// `manifest`, and a client that reads only those is unaffected.
#[derive(Debug, Serialize)]
pub(super) struct BlueprintDetail {
    #[serde(flatten)]
    pub(super) info: super::types::BlueprintInfo,
    /// The blueprint's context regions.
    ///
    /// On the detail route rather than the listing, for the same reason the
    /// manifest is: answering "what agents are there" should not cost every
    /// region of every agent on the machine.
    pub(super) regions: Vec<super::types::RegionInfo>,
    /// The blueprint's fan-out stages, with their limits as the daemon will
    /// apply them. Empty for a blueprint that never fans out.
    pub(super) fan_outs: Vec<super::types::FanOutInfo>,
    /// The stages that route produced parts (`output_routing`) or reset a
    /// region on entry (`context.reset`); empty when the blueprint does neither.
    pub(super) stage_routing: Vec<StageRoutingInfo>,
    /// What the agent declares it needs before it runs (`[[dependencies]]`),
    /// so a console can show them and warn before a spawn that would fail the
    /// dependency gate. Empty for a blueprint that declares none.
    pub(super) dependencies: Vec<DependencyInfo>,
    /// The manifest exactly as it is on disk.
    ///
    /// Without this a console has no way to read what it is editing: naming
    /// the file in `path` is not the same as being able to open it, since the
    /// browser cannot, and the fallbacks it is left with (a draft in local
    /// storage, or a copy bundled at build time) are both disconnected from
    /// the file the daemon actually runs.
    pub(super) manifest: String,
}

/// One declared dependency, so the detail route carries what an agent needs
/// without a console parsing the manifest. The kind-specific fields appear only
/// for the kind that has them.
#[derive(Debug, Serialize)]
pub(super) struct DependencyInfo {
    /// The dependency's name.
    pub(super) name: String,
    /// Its kind: `mcp_server`, `env`, `binary` or `script`.
    pub(super) kind: &'static str,
    /// Whether an unmet result blocks the run.
    pub(super) required: bool,
    /// How to satisfy it when missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) remedy: Option<String>,
    /// Why the agent needs it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    /// kind=mcp_server: the server name that must be configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) server: Option<String>,
    /// kind=mcp_server: the env secrets the server needs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) env: Vec<String>,
    /// kind=env: the variable that must be set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) var: Option<String>,
    /// kind=binary: the program that must be on PATH.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) command: Option<String>,
    /// kind=script: the Rhai check script's path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) check: Option<String>,
    /// Whether the blueprint declares how to install it.
    pub(super) installable: bool,
}

/// The declared dependencies of a blueprint, in declaration order, so the
/// detail route carries them structured. Empty for a blueprint that declares
/// none.
pub(super) fn dependency_infos(blueprint: &leviath_core::Blueprint) -> Vec<DependencyInfo> {
    use leviath_core::blueprint::DependencyKind;
    blueprint
        .dependencies
        .iter()
        .map(|dep| {
            let mut info = DependencyInfo {
                name: dep.name.clone(),
                kind: dep.kind.tag(),
                required: dep.required,
                remedy: dep.remedy.clone(),
                description: dep.description.clone(),
                server: None,
                env: Vec::new(),
                var: None,
                command: None,
                check: None,
                installable: dep.install.is_some(),
            };
            match &dep.kind {
                DependencyKind::McpServer { server, env } => {
                    info.server = Some(server.clone());
                    info.env = env.clone();
                }
                DependencyKind::Env { var } => info.var = Some(var.clone()),
                DependencyKind::Binary { command } => info.command = Some(command.clone()),
                DependencyKind::Script { check } => info.check = Some(check.clone()),
            }
            info
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_infos_carries_every_kind() {
        let bp = leviath_core::manifest::parse_manifest(
            "[agent]\nname = \"a\"\n\n\
             [[dependencies]]\nname = \"m\"\nkind = \"mcp_server\"\nserver = \"meshy\"\n\
             env = [\"K\"]\nremedy = \"r\"\ndescription = \"d\"\n\
             [dependencies.install.server]\nurl = \"https://x\"\n\n\
             [[dependencies]]\nname = \"e\"\nkind = \"env\"\nvar = \"V\"\n\n\
             [[dependencies]]\nname = \"b\"\nkind = \"binary\"\ncommand = \"c\"\n\n\
             [[dependencies]]\nname = \"s\"\nkind = \"script\"\ncheck = \"chk.rhai\"\n",
        )
        .unwrap();
        let infos = dependency_infos(&bp);
        assert_eq!(infos.len(), 4);
        assert_eq!(infos[0].kind, "mcp_server");
        assert_eq!(infos[0].server.as_deref(), Some("meshy"));
        assert_eq!(infos[0].env, vec!["K".to_string()]);
        assert_eq!(infos[0].remedy.as_deref(), Some("r"));
        assert_eq!(infos[0].description.as_deref(), Some("d"));
        assert!(infos[0].required);
        assert!(infos[0].installable);
        assert_eq!(infos[1].var.as_deref(), Some("V"));
        assert_eq!(infos[2].command.as_deref(), Some("c"));
        assert_eq!(infos[3].check.as_deref(), Some("chk.rhai"));
        assert!(!infos[3].installable);
        // Exercise the wire serialization the detail route relies on.
        let json = serde_json::to_string(&infos).expect("serializes");
        assert!(json.contains("\"kind\":\"mcp_server\""), "{json}");
        assert!(json.contains("\"installable\":true"), "{json}");
        // An agent with no dependencies yields an empty list.
        let none = leviath_core::manifest::parse_manifest("[agent]\nname = \"n\"\n").unwrap();
        assert!(dependency_infos(&none).is_empty());
    }
}
