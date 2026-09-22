//! The `upsertYoloProfile` and `deleteYoloProfile` fields, and the input one
//! profile is written from.
//!
//! One table of `yolo.toml` at a time. The file is loaded as a document, the
//! one table is replaced or removed, and the whole document is checked before
//! anything is written, so a save that would leave the set unloadable is
//! refused rather than discovered at the next spawn.

use async_graphql::{ID, InputObject, SimpleObject};

use super::super::super::core::error::ServeError;
use super::super::error::IntoGraphql;
use super::super::types::machine::{YoloHuman, YoloProfile, YoloWaiver};

/// One shell rule, as a write sends it.
#[derive(Debug, InputObject)]
pub(crate) struct ShellRuleWrite {
    /// The leading words, one glob per word: `rm -r*`, `git push*`, `cargo *`.
    pub(crate) command: String,
    /// Globs every remaining word has to satisfy. Left out means any remaining
    /// words; an empty list means none.
    pub(crate) args: Option<Vec<String>>,
}

impl ShellRuleWrite {
    /// The rule this describes, as the file spells it.
    fn into_spec(self) -> crate::yolo::rules::ShellRuleSpec {
        crate::yolo::rules::ShellRuleSpec {
            command: self.command,
            args: self.args,
        }
    }
}

/// The tools a profile names, under each verdict, as a write sends them.
#[derive(Debug, Default, InputObject)]
pub(crate) struct YoloToolRulesWrite {
    /// Runs without asking.
    pub(crate) allow: Option<Vec<String>>,
    /// Goes to a person, as it would with no profile.
    pub(crate) ask: Option<Vec<String>>,
    /// Refused at dispatch; nobody is asked.
    pub(crate) deny: Option<Vec<String>>,
}

impl YoloToolRulesWrite {
    /// The lists this describes. A list left out is an empty one, which is
    /// what the file means by leaving it out.
    fn into_spec(self) -> crate::yolo::rules::ToolListsSpec {
        crate::yolo::rules::ToolListsSpec {
            allow: self.allow.unwrap_or_default(),
            ask: self.ask.unwrap_or_default(),
            deny: self.deny.unwrap_or_default(),
        }
    }
}

/// The shell lines a profile names, under each verdict, as a write sends them.
#[derive(Debug, Default, InputObject)]
pub(crate) struct YoloShellRulesWrite {
    /// Runs without asking.
    pub(crate) allow: Option<Vec<ShellRuleWrite>>,
    /// Goes to a person.
    pub(crate) ask: Option<Vec<ShellRuleWrite>>,
    /// Refused at dispatch.
    pub(crate) deny: Option<Vec<ShellRuleWrite>>,
}

impl YoloShellRulesWrite {
    /// The lists this describes, compiled at load like any other.
    fn into_spec(self) -> crate::yolo::rules::ShellListsSpec {
        /// One verdict's rules.
        fn rules(written: Option<Vec<ShellRuleWrite>>) -> Vec<crate::yolo::rules::ShellRuleSpec> {
            written
                .unwrap_or_default()
                .into_iter()
                .map(ShellRuleWrite::into_spec)
                .collect()
        }
        crate::yolo::rules::ShellListsSpec {
            allow: rules(self.allow),
            ask: rules(self.ask),
            deny: rules(self.deny),
        }
    }
}

/// One yolo profile, as a write sends it.
///
/// The whole profile, because a profile is a grant of permissions: an edit
/// that left half of a previous list behind would describe a set of rules
/// nobody wrote. What a field leaves out is what the file means by leaving it
/// out, which is `AUTO` for the three human knobs and an empty list for the
/// rules.
#[derive(Debug, InputObject)]
pub(crate) struct YoloProfileWrite {
    /// The name `--yolo=<name>` will spell. `default` is reserved for bare
    /// `--yolo`, which reads no file.
    pub(crate) name: String,
    /// What tools with no explicit rule do. The one setting with no default:
    /// it is what the profile is for, so a write says it.
    pub(crate) default: YoloWaiver,
    /// What happens to the run's own questions.
    pub(crate) questions: Option<YoloHuman>,
    /// What happens at blueprint checkpoints.
    pub(crate) checkpoints: Option<YoloHuman>,
    /// What happens at the taint gate.
    pub(crate) gate: Option<YoloHuman>,
    /// The tools it names, under each verdict.
    pub(crate) tool_rules: Option<YoloToolRulesWrite>,
    /// The shell lines it names, under each verdict.
    pub(crate) shell_rules: Option<YoloShellRulesWrite>,
}

impl YoloProfileWrite {
    /// The profile this describes, as the file spells it.
    fn into_spec(self) -> crate::yolo::rules::ProfileSpec {
        /// A human knob, or the `auto` the file defaults it to.
        fn human(asked: Option<YoloHuman>) -> crate::yolo::rules::Human {
            match asked {
                Some(YoloHuman::Ask) => crate::yolo::rules::Human::Ask,
                Some(YoloHuman::Auto) | None => crate::yolo::rules::Human::Auto,
            }
        }
        crate::yolo::rules::ProfileSpec {
            default: match self.default {
                YoloWaiver::Allow => crate::yolo::rules::Waiver::Allow,
                YoloWaiver::Ask => crate::yolo::rules::Waiver::Ask,
            },
            questions: human(self.questions),
            checkpoints: human(self.checkpoints),
            gate: human(self.gate),
            tools: self.tool_rules.unwrap_or_default().into_spec(),
            shell: self.shell_rules.unwrap_or_default().into_spec(),
        }
    }
}

/// Which profile to write.
#[derive(Debug, InputObject)]
pub(crate) struct UpsertYoloProfileRequest {
    /// The profile to write.
    pub(crate) profile: YoloProfileWrite,
}

/// The profile as the file now holds it.
#[derive(SimpleObject)]
pub(crate) struct UpsertYoloProfileResult {
    /// The profile, read back through the file, so what comes back is what a
    /// run will resolve rather than what was sent.
    pub(crate) yolo_profile: YoloProfile,
    /// Whether the profile is new, rather than a replacement of one already
    /// there.
    pub(crate) is_new: bool,
}

/// Which profile to take out.
#[derive(Debug, InputObject)]
pub(crate) struct DeleteYoloProfileRequest {
    /// The profile's name.
    pub(crate) name: String,
}

/// What was taken out.
#[derive(Debug, SimpleObject)]
pub(crate) struct DeleteYoloProfileResult {
    /// The id the profile answered to.
    pub(crate) deleted_id: ID,
}

/// Write one yolo profile.
pub(crate) async fn upsert_yolo_profile(
    request: UpsertYoloProfileRequest,
) -> async_graphql::Result<UpsertYoloProfileResult> {
    let name = request.profile.name.clone();
    let spec = request.profile.into_spec();
    let written = super::super::super::yolo::upsert_profile(&name, &spec).gql()?;
    Ok(UpsertYoloProfileResult {
        // Read back through the file rather than echoed from the request: what
        // a run resolves is what the saved document loads as.
        yolo_profile: YoloProfile::from_profile(&written.profile),
        is_new: written.is_new,
    })
}

/// Remove one yolo profile.
///
/// A name the file has no table for is a miss: the caller named a profile, and
/// there was none to take out.
pub(crate) async fn delete_yolo_profile(
    request: DeleteYoloProfileRequest,
) -> async_graphql::Result<DeleteYoloProfileResult> {
    let removed = super::super::super::yolo::remove_profile(&request.name).gql()?;
    match removed {
        true => Ok(DeleteYoloProfileResult {
            deleted_id: super::super::node::yolo_profile_id(&request.name),
        }),
        false => Err(super::super::error::graphql_error(&ServeError::NotFound(
            format!("no yolo profile named '{}'", request.name),
        ))),
    }
}
