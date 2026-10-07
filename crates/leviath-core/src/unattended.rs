//! How much of a run goes ahead without a person: the one setting `--yolo`,
//! `--yolo=<name>` and a parent's own setting all come down to.

use serde::{Deserialize, Serialize};

use crate::names::{NameError, ProfileName};

/// How much of a run goes ahead without a person.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Unattended {
    /// A person answers every approval and question.
    #[default]
    Off,
    /// Nothing waits for a person: every tool call is approved, the taint
    /// gate is waived, and the run's own questions are answered for it.
    All,
    /// The named yolo profile says which of those still reach a person.
    Profile(ProfileName),
}

impl Unattended {
    /// The setting the `--yolo` flag spells: no flag is [`Self::Off`], the
    /// bare flag (an empty name) is [`Self::All`], and `--yolo=<name>` is
    /// that profile.
    pub fn from_flag(flag: Option<&str>) -> Result<Self, NameError> {
        match flag {
            None => Ok(Self::Off),
            Some("") => Ok(Self::All),
            Some(name) => ProfileName::new(name).map(Self::Profile),
        }
    }

    /// The setting a `yolo` flag and a `yolo_profile` name spell, as a record
    /// written by an earlier release carries them: a profile is that profile
    /// whatever the flag says, and a name that is not a valid profile name
    /// reads as [`Self::Off`], the setting that asks a person.
    pub fn from_keys(yolo: bool, profile: Option<String>) -> Self {
        match (profile, yolo) {
            (Some(name), _) => ProfileName::new(name).map_or(Self::Off, Self::Profile),
            (None, true) => Self::All,
            (None, false) => Self::Off,
        }
    }

    /// Whether anything goes ahead without a person.
    pub fn is_on(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// The named profile, when the run is under one.
    pub fn profile(&self) -> Option<&ProfileName> {
        match self {
            Self::Profile(name) => Some(name),
            Self::Off | Self::All => None,
        }
    }

    /// The less trusting of this setting and `ceiling`: what a child asking
    /// for this gets under a parent holding `ceiling`. A child asking for
    /// [`Self::All`] under a parent's profile runs under the parent's profile.
    pub fn at_most(&self, ceiling: &Self) -> Self {
        match self.rank() <= ceiling.rank() {
            true => self.clone(),
            false => ceiling.clone(),
        }
    }

    /// How much this setting trusts the run, for comparing two settings.
    fn rank(&self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Profile(_) => 1,
            Self::All => 2,
        }
    }
}

/// [`Unattended`] as a run's listing record spells it on the wire: a `yolo`
/// flag, and a `yolo_profile` beside it for a named profile. Used with
/// `#[serde(flatten, with = "...")]`, so the two keys sit at the top level of
/// the record.
///
/// Reading is lenient, because the same record is how a run from an earlier
/// release is read (see [`Unattended::from_keys`]).
pub mod as_yolo_keys {
    use super::Unattended;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    struct Keys {
        #[serde(default)]
        yolo: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        yolo_profile: Option<String>,
    }

    /// Write `unattended` as `yolo` and, for a profile, `yolo_profile`.
    pub fn serialize<S: Serializer>(unattended: &Unattended, s: S) -> Result<S::Ok, S::Error> {
        Keys {
            yolo: unattended.is_on(),
            yolo_profile: unattended.profile().map(|p| p.as_str().to_string()),
        }
        .serialize(s)
    }

    /// Read `yolo` and `yolo_profile` back as one setting.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Unattended, D::Error> {
        Keys::deserialize(d).map(|keys| Unattended::from_keys(keys.yolo, keys.yolo_profile))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str) -> Unattended {
        Unattended::Profile(ProfileName::new(name).unwrap())
    }

    #[test]
    fn the_flag_spells_each_setting() {
        assert_eq!(Unattended::from_flag(None), Ok(Unattended::Off));
        assert_eq!(Unattended::from_flag(Some("")), Ok(Unattended::All));
        assert_eq!(
            Unattended::from_flag(Some("careful")),
            Ok(profile("careful"))
        );
        assert!(Unattended::from_flag(Some(" bad")).is_err());
    }

    #[test]
    fn a_setting_says_whether_it_is_on_and_which_profile() {
        assert!(!Unattended::Off.is_on());
        assert!(Unattended::All.is_on());
        assert!(profile("careful").is_on());
        assert_eq!(Unattended::All.profile(), None);
        assert_eq!(Unattended::Off.profile(), None);
        assert_eq!(
            profile("careful").profile().map(ProfileName::as_str),
            Some("careful")
        );
    }

    #[test]
    fn at_most_takes_the_less_trusting() {
        let p = profile("careful");
        assert_eq!(Unattended::All.at_most(&p), p);
        assert_eq!(p.at_most(&Unattended::All), p);
        assert_eq!(Unattended::All.at_most(&Unattended::Off), Unattended::Off);
        assert_eq!(Unattended::Off.at_most(&Unattended::All), Unattended::Off);
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Record {
        id: u8,
        #[serde(flatten, with = "as_yolo_keys")]
        unattended: Unattended,
    }

    fn read(json: serde_json::Value) -> Unattended {
        serde_json::from_value::<Record>(json).unwrap().unattended
    }

    #[test]
    fn the_yolo_keys_round_trip_every_setting() {
        for (unattended, wire) in [
            (Unattended::Off, serde_json::json!({"id": 1, "yolo": false})),
            (Unattended::All, serde_json::json!({"id": 1, "yolo": true})),
            (
                profile("careful"),
                serde_json::json!({"id": 1, "yolo": true, "yolo_profile": "careful"}),
            ),
        ] {
            let record = Record { id: 1, unattended };
            assert_eq!(serde_json::to_value(&record).unwrap(), wire);
            assert_eq!(read(wire), record.unattended);
        }
    }

    #[test]
    fn the_yolo_keys_read_an_earlier_release_leniently() {
        assert_eq!(read(serde_json::json!({"id": 1})), Unattended::Off);
        assert_eq!(
            read(serde_json::json!({"id": 1, "yolo_profile": "careful"})),
            profile("careful")
        );
        assert_eq!(
            read(serde_json::json!({"id": 1, "yolo": true, "yolo_profile": " bad"})),
            Unattended::Off
        );
    }
}
