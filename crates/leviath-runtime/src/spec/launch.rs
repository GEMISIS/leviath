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

use super::names::{HttpUrl, RunId, SecretRef, StageName, ToolName};

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
}

impl Default for LaunchRequest {
    fn default() -> Self {
        Self {
            unattended: Unattended::Off,
            allow: Vec::new(),
            max_depth: None,
            seed_commands: true,
            capture_model_input: false,
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
        }
    }
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
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
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
}

impl DeliveryPlan {
    /// `delivery` for the run `run`: each secret it carries given a new
    /// reference, and the secrets themselves under those references, for the
    /// host to keep in its secret store.
    pub fn of(delivery: &Delivery, run: &RunId) -> (Self, Vec<(SecretRef, Secret)>) {
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
        };
        let (plan, kept) = DeliveryPlan::of(&asked, &run);
        let reference = plan.callback_secret().unwrap().clone();
        assert_eq!(reference.run(), Some("r-1"));
        assert_eq!(kept, vec![(reference, Secret::new("hunter2"))]);
        assert_eq!(plan.metadata["team"], "a");
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
        };
        let (plan, kept) = DeliveryPlan::of(&unsigned, &run);
        assert!(plan.callback.is_some() && plan.callback_secret().is_none() && kept.is_empty());
        let (plan, kept) = DeliveryPlan::of(&Delivery::default(), &run);
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
    }
}
