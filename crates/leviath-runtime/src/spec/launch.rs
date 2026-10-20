//! How a run is launched: how much it may do unattended, where it runs, and
//! who hears when it ends.
//!
//! [`LaunchRequest`] is what a caller asks for. [`LaunchPolicy`] is what the
//! run gets. A top-level run gets what it asked for, filled in from the
//! operator's defaults. A child run's request is narrowed against its
//! parent's policy with [`LaunchPolicy::narrow`], so no run can start a child
//! that is trusted with more than it was. [`Placement`] is set by the host
//! alone; nothing a caller writes reaches it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::names::{Digest, HttpUrl, IdempotencyKey, RunId, SecretRef, StageName, ToolName};

pub use leviath_core::unattended::Unattended;

/// What a caller asks for when launching a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    /// How much goes ahead without a person.
    #[serde(default)]
    pub unattended: Unattended,
    /// Tools approved for this run without asking.
    #[serde(default)]
    pub allow: Vec<ToolName>,
    /// How deep the run's tree of child runs may grow. `None` takes the
    /// graph's `max_child_depth`.
    #[serde(default)]
    pub max_depth: Option<u8>,
    /// Whether region seeds that run a shell command may run at spawn.
    #[serde(default = "leviath_core::default_true")]
    pub seed_commands: bool,
    /// Whether to record every request sent to the model, for debugging.
    #[serde(default)]
    pub capture_model_input: bool,
    /// Globs, relative to the workdir, that every write tool call of the run
    /// must match, on top of any its graph and stages set. Empty adds no
    /// limit. A child run or fan-out worker of this run inherits them.
    #[serde(default)]
    pub write_paths: Vec<String>,
}

impl Default for LaunchRequest {
    fn default() -> Self {
        Self {
            unattended: Unattended::Off,
            allow: Vec::new(),
            max_depth: None,
            seed_commands: true,
            capture_model_input: false,
            write_paths: Vec::new(),
        }
    }
}

/// What a run is trusted with, once decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LaunchPolicy {
    /// How much goes ahead without a person.
    pub unattended: Unattended,
    /// Tools approved without asking.
    pub allow: Vec<ToolName>,
    /// How many more levels of child runs this run may start.
    pub max_depth: u8,
    /// Whether region seeds that run a shell command may run.
    pub seed_commands: bool,
    /// Whether every model request is recorded.
    pub capture_model_input: bool,
    /// Write-path layers imposed from outside the run's own graph: the
    /// request's `write_paths`, and the limits in force where the run that
    /// started this one was when it started it. A write must match a glob of
    /// every layer (see [`leviath_core::write_paths`]).
    pub write_paths: Vec<Vec<String>>,
}

impl LaunchPolicy {
    /// The policy for a top-level run: the request as given, with the depth
    /// filled in from `default_depth` when the request leaves it open, and
    /// seed commands off when the operator has turned them off.
    pub fn top_level(
        request: &LaunchRequest,
        default_depth: u8,
        seed_commands_allowed: bool,
    ) -> Self {
        Self {
            unattended: request.unattended.clone(),
            allow: request.allow.clone(),
            max_depth: request.max_depth.unwrap_or(default_depth),
            seed_commands: request.seed_commands && seed_commands_allowed,
            capture_model_input: request.capture_model_input,
            write_paths: layers(Vec::new(), &request.write_paths),
        }
    }

    /// The policy for a child run of `parent`: what the child asked for, but
    /// never more than the parent has.
    ///
    /// - Unattended: the less trusting of the two. A child asking for `All`
    ///   under a parent's profile runs under the parent's profile.
    /// - Allow: only tools both approve.
    /// - Depth: at most one less than the parent's.
    /// - Seed commands: only when both allow them.
    /// - Capturing model input only records more; the child's own ask stands.
    /// - Write paths: every layer the parent had, plus the child's own ask.
    pub fn narrow(request: &LaunchRequest, parent: &LaunchPolicy) -> Self {
        let unattended = request.unattended.at_most(&parent.unattended);
        let headroom = parent.max_depth.saturating_sub(1);
        Self {
            unattended,
            allow: request
                .allow
                .iter()
                .filter(|t| parent.allow.contains(t))
                .cloned()
                .collect(),
            max_depth: request.max_depth.map_or(headroom, |d| d.min(headroom)),
            seed_commands: request.seed_commands && parent.seed_commands,
            capture_model_input: request.capture_model_input,
            write_paths: layers(parent.write_paths.clone(), &request.write_paths),
        }
    }
}

/// `outer` with `asked` added as one more layer, when it names any globs.
fn layers(mut outer: Vec<Vec<String>>, asked: &[String]) -> Vec<Vec<String>> {
    if !asked.is_empty() {
        outer.push(asked.to_vec());
    }
    outer
}

/// Where a run sits: its workdir and its place in a tree of runs. Set by the
/// host; a request cannot name any of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Placement {
    /// The directory tools work in. Absolute.
    pub workdir: PathBuf,
    /// The run that started this one, when one did.
    pub parent: Option<RunId>,
    /// How far below the top of its tree this run is. A top-level run is 0.
    pub depth: u8,
    /// For a fan-out worker, the stage of the parent's graph it runs.
    pub worker_stage: Option<StageName>,
    /// For a fan-out worker, the id of the work item it runs: what a restart
    /// matches a worker its parent never recorded against.
    pub work_item: Option<String>,
}

/// A secret that must never reach a log line. `Debug` and `Display` print
/// `[redacted]`. Serialization keeps the value, so a caller's request can
/// carry it to the daemon; a resolved run never holds one, only the
/// [`SecretRef`] it was kept under in the machine's secret store.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself, for the one place that uses it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// A webhook called when the run finishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Callback {
    /// Where to POST.
    pub url: HttpUrl,
    /// The shared secret the body is signed with (HMAC-SHA256), when there is one.
    #[serde(default)]
    pub secret: Option<Secret>,
}

/// Who hears about the run, and the caller's own labels for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    /// The webhook to call when the run finishes.
    #[serde(default)]
    pub callback: Option<Callback>,
    /// The caller's labels, carried through untouched and shown with the run.
    /// One is also read: `leviath.context_scope` names the scope whose
    /// shared region (`shared_intents`) the run takes part in.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// The caller's name for this spawn. A request sent again with the same
    /// key and nothing else changed is answered with the run the first one
    /// started, for as long as that run's record exists, and starts no
    /// second run. The same key with a different request is refused.
    /// Written only when set, so a daemon that predates the key reads a
    /// request without one exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
}

/// A spawn's idempotency key as a resolved run keeps it: the key, and the
/// digest of the request that came with it, which is how a request sent
/// again is told apart from a different one under the same key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Idempotency {
    /// The key.
    pub key: IdempotencyKey,
    /// The digest of the request, its key left out (see
    /// [`SpawnRequest::idempotency`](super::request::SpawnRequest::idempotency)).
    pub request: Digest,
}

/// A webhook as a resolved run holds it: the signing secret by the
/// reference it is kept under in the machine's secret store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallbackPlan {
    /// Where to POST.
    pub url: HttpUrl,
    /// Where the secret the body is signed with is kept, when there is one.
    /// Not called `secret`: it is the name of one, and a reader that looks
    /// for secrets by their key (a bug report's scrubber, say) must not take
    /// it for the secret itself.
    pub signed_with: Option<SecretRef>,
}

/// Who hears about a resolved run, and the caller's labels for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeliveryPlan {
    /// The webhook to call when the run finishes.
    pub callback: Option<CallbackPlan>,
    /// The caller's labels.
    pub metadata: BTreeMap<String, String>,
    /// The key the run was spawned under, when it was given one. Kept here
    /// so a daemon that restarts finds every key still held in the records
    /// it already reads.
    pub idempotency: Option<Idempotency>,
}

impl DeliveryPlan {
    /// `delivery` for the run `run`, spawned under `idempotency`: each secret
    /// it carries given a new reference, and the secrets themselves under
    /// those references, for the host to keep in its secret store.
    pub fn of(
        delivery: &Delivery,
        idempotency: Option<Idempotency>,
        run: &RunId,
    ) -> (Self, Vec<(SecretRef, Secret)>) {
        let mut kept = Vec::new();
        let callback = delivery.callback.as_ref().map(|callback| CallbackPlan {
            url: callback.url.clone(),
            signed_with: callback.secret.as_ref().map(|secret| {
                let reference = crate::secret_store::mint(run);
                kept.push((reference.clone(), secret.clone()));
                reference
            }),
        });
        let plan = Self {
            callback,
            metadata: delivery.metadata.clone(),
            idempotency,
        };
        (plan, kept)
    }

    /// Where the webhook's signing secret is kept, when it has one.
    pub fn callback_secret(&self) -> Option<&SecretRef> {
        self.callback.as_ref().and_then(|c| c.signed_with.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::names::ProfileName;

    fn tool(s: &str) -> ToolName {
        ToolName::new(s).unwrap()
    }

    fn profile() -> Unattended {
        Unattended::Profile(ProfileName::new("safe").unwrap())
    }

    fn policy(unattended: Unattended, allow: &[&str], depth: u8, seeds: bool) -> LaunchPolicy {
        LaunchPolicy {
            unattended,
            allow: allow.iter().map(|t| tool(t)).collect(),
            max_depth: depth,
            seed_commands: seeds,
            capture_model_input: false,
            write_paths: Vec::new(),
        }
    }

    #[test]
    fn a_top_level_run_gets_what_it_asked_for() {
        let req = LaunchRequest {
            unattended: Unattended::All,
            allow: vec![tool("bash")],
            ..LaunchRequest::default()
        };
        let p = LaunchPolicy::top_level(&req, 3, true);
        assert_eq!(p, policy(Unattended::All, &["bash"], 3, true));
        let pinned = LaunchRequest {
            max_depth: Some(1),
            ..LaunchRequest::default()
        };
        assert_eq!(LaunchPolicy::top_level(&pinned, 3, false).max_depth, 1);
        assert!(!LaunchPolicy::top_level(&pinned, 3, false).seed_commands);
    }

    #[test]
    fn a_child_never_gets_more_than_its_parent() {
        let parent = policy(profile(), &["bash", "read_file"], 2, false);
        let greedy = LaunchRequest {
            unattended: Unattended::All,
            allow: vec![tool("bash"), tool("write_file")],
            max_depth: Some(9),
            seed_commands: true,
            capture_model_input: true,
            write_paths: Vec::new(),
        };
        let child = LaunchPolicy::narrow(&greedy, &parent);
        assert_eq!(child.unattended, profile());
        assert_eq!(child.allow, vec![tool("bash")]);
        assert_eq!(child.max_depth, 1);
        assert!(!child.seed_commands);
        assert!(child.capture_model_input);
    }

    #[test]
    fn a_child_may_ask_for_less() {
        let parent = policy(Unattended::All, &["bash"], 3, true);
        let modest = LaunchRequest {
            max_depth: Some(0),
            ..LaunchRequest::default()
        };
        let child = LaunchPolicy::narrow(&modest, &parent);
        assert_eq!(child.unattended, Unattended::Off);
        assert!(child.allow.is_empty());
        assert_eq!(child.max_depth, 0);
        assert!(child.seed_commands);
        let open = LaunchPolicy::narrow(
            &LaunchRequest::default(),
            &policy(Unattended::Off, &[], 0, true),
        );
        assert_eq!(open.max_depth, 0, "depth never goes below zero");
    }

    /// Write paths only ever add layers: a top-level run takes its request's
    /// as one, and a child keeps every layer its parent had and adds its own.
    #[test]
    fn write_paths_only_ever_add_layers() {
        let globs = |list: &[&str]| list.iter().map(|g| g.to_string()).collect::<Vec<_>>();
        let asked = LaunchRequest {
            write_paths: globs(&["docs/**"]),
            ..LaunchRequest::default()
        };
        let top = LaunchPolicy::top_level(&asked, 3, true);
        assert_eq!(top.write_paths, vec![globs(&["docs/**"])]);
        let open = LaunchPolicy::top_level(&LaunchRequest::default(), 3, true);
        assert!(open.write_paths.is_empty());

        let narrower = LaunchRequest {
            write_paths: globs(&["docs/panels/**"]),
            ..LaunchRequest::default()
        };
        let child = LaunchPolicy::narrow(&narrower, &top);
        assert_eq!(
            child.write_paths,
            vec![globs(&["docs/**"]), globs(&["docs/panels/**"])]
        );
        let silent = LaunchPolicy::narrow(&LaunchRequest::default(), &top);
        assert_eq!(
            silent.write_paths, top.write_paths,
            "a child cannot drop one"
        );
    }

    #[test]
    fn a_secret_never_prints() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?} {s}"), "[redacted] [redacted]");
        assert_eq!(s.expose(), "hunter2");
        let cb = Callback {
            url: HttpUrl::new("https://x.dev/hook").unwrap(),
            secret: Some(s),
        };
        assert!(!format!("{cb:?}").contains("hunter2"));
        let bin = postcard::to_stdvec(&cb).unwrap();
        assert_eq!(postcard::from_bytes::<Callback>(&bin).unwrap(), cb);
    }

    /// A resolved delivery holds a reference where the request held the
    /// secret, and hands the secret back beside it for the store.
    #[test]
    fn a_planned_delivery_holds_a_reference_and_never_the_secret() {
        let run = RunId::new("r-1").unwrap();
        let asked = Delivery {
            callback: Some(Callback {
                url: HttpUrl::new("https://x.dev/hook").unwrap(),
                secret: Some(Secret::new("hunter2")),
            }),
            metadata: [("team".to_string(), "a".to_string())].into(),
            idempotency_key: None,
        };
        let keyed = Idempotency {
            key: IdempotencyKey::new("task-1").unwrap(),
            request: Digest::of(b"request"),
        };
        let (plan, kept) = DeliveryPlan::of(&asked, Some(keyed.clone()), &run);
        let reference = plan.callback_secret().unwrap().clone();
        assert_eq!(reference.run(), Some("r-1"));
        assert_eq!(kept, vec![(reference, Secret::new("hunter2"))]);
        assert_eq!(plan.metadata["team"], "a");
        assert_eq!(plan.idempotency, Some(keyed));
        let bin = postcard::to_stdvec(&plan).unwrap();
        assert!(!String::from_utf8_lossy(&bin).contains("hunter2"));
        assert_eq!(postcard::from_bytes::<DeliveryPlan>(&bin).unwrap(), plan);
        // A webhook with no secret, and no webhook at all, keep nothing.
        let unsigned = Delivery {
            callback: Some(Callback {
                url: HttpUrl::new("https://x.dev/hook").unwrap(),
                secret: None,
            }),
            metadata: BTreeMap::new(),
            idempotency_key: None,
        };
        let (plan, kept) = DeliveryPlan::of(&unsigned, None, &run);
        assert!(plan.callback.is_some() && plan.callback_secret().is_none() && kept.is_empty());
        let (plan, kept) = DeliveryPlan::of(&Delivery::default(), None, &run);
        assert_eq!((plan, kept.len()), (DeliveryPlan::default(), 0));
    }

    #[test]
    fn requests_default_and_refuse_unknown_keys() {
        let req: LaunchRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req, LaunchRequest::default());
        assert!(req.seed_commands);
        let err = serde_json::from_str::<LaunchRequest>(r#"{"yolo": true}"#).unwrap_err();
        assert!(err.to_string().contains("unknown field `yolo`"), "{err}");
        let with_profile: LaunchRequest =
            serde_json::from_str(r#"{"unattended": {"profile": "safe"}}"#).unwrap();
        assert_eq!(with_profile.unattended, profile());
        let d: Delivery = serde_json::from_str(r#"{"metadata": {"team": "a"}}"#).unwrap();
        assert_eq!(d.metadata["team"], "a");
        assert_eq!(d.idempotency_key, None);
    }

    /// A delivery without a key is written without the field, so a daemon
    /// that predates it reads the request as before; one with a key carries
    /// it, and a key that is not one is refused on the way in.
    #[test]
    fn an_idempotency_key_is_written_only_when_set() {
        let none = serde_json::to_value(Delivery::default()).unwrap();
        assert_eq!(none, serde_json::json!({"callback": null, "metadata": {}}));
        let keyed: Delivery = serde_json::from_str(r#"{"idempotency_key": "pw:task-7"}"#).unwrap();
        assert_eq!(
            keyed.idempotency_key.as_ref().unwrap().as_str(),
            "pw:task-7"
        );
        assert_eq!(
            serde_json::to_value(&keyed).unwrap()["idempotency_key"],
            "pw:task-7"
        );
        let err = serde_json::from_str::<Delivery>(r#"{"idempotency_key": "a b"}"#).unwrap_err();
        assert!(err.to_string().contains("idempotency key"), "{err}");
    }
}
