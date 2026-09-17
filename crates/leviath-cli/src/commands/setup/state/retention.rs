//! The Defaults screen's data retention rows: the zero data retention switch,
//! one agreement toggle per chosen contract provider, and the file upload
//! switch, and reading them back out of the form.
//!
//! The rows are found by label rather than index: the switch sits after the
//! Bedrock region row, which comes and goes with the Bedrock selection, so
//! its index moves between visits while its label does not.

use super::{Field, FieldValue, Wizard};

impl Wizard {
    /// The zero data retention row's label, which is how the row is found.
    pub(super) const ZERO_RETENTION_LABEL: &'static str = "Zero data retention (ZDR)";

    /// The file upload row's label.
    pub(super) const FILE_UPLOADS_LABEL: &'static str = "Upload media to provider file storage";

    /// The providers that settle retention by contract, with the label and
    /// help of the agreement row each gets on the Defaults screen when it
    /// is chosen. The id is what `zero_retention_agreements` holds.
    pub(super) const AGREEMENT_PROVIDERS: &'static [(&'static str, &'static str, &'static str)] = &[
        (
            "anthropic",
            "ZDR agreement with Anthropic",
            "Your organisation holds a zero data retention agreement with Anthropic, whose \
             API otherwise keeps prompts and replies up to 30 days for trust and safety. \
             No API can read a contract, so this is taken at your word: with it, \
             Anthropic's models count as keeping nothing when zero data retention is on \
             (Claude Fable 5 and Mythos 5 keep 30 days regardless).",
        ),
        (
            "openai",
            "ZDR agreement with OpenAI",
            "Your organisation holds a Zero Data Retention agreement with OpenAI, whose \
             API otherwise keeps prompts and replies up to 30 days for abuse monitoring. \
             No API can read a contract, so this is taken at your word: with it, \
             OpenAI's models count as keeping nothing when zero data retention is on.",
        ),
        (
            "google",
            "ZDR agreement with Google",
            "Google has granted your project zero data retention on the Gemini API, which \
             otherwise keeps prompts 55 days for abuse monitoring on the paid tier. No \
             API can read that, so this is taken at your word: with it, Gemini models \
             count as keeping nothing when zero data retention is on.",
        ),
    ];

    /// Append the switch and the agreement rows to the Defaults form, with
    /// the values read off the previous form (or the config) by the caller
    /// before it replaced the form.
    pub(super) fn push_retention_fields(
        &mut self,
        zero: bool,
        agreements: &[String],
        uploads: bool,
    ) {
        self.defaults.push(Field {
            label: Self::ZERO_RETENTION_LABEL,
            help: "Ask every provider to keep nothing of your prompts and replies once a \
                   reply is returned (zero data retention, ZDR). Bedrock's account mode \
                   is set to none, OpenAI is sent store=false, OpenRouter routes only to \
                   endpoints with a zero-retention policy, and a stage whose model cannot \
                   give it is refused rather than run. Some models keep data regardless \
                   (Claude Fable 5 and Mythos 5 keep 30 days for safety review; OpenAI's \
                   models on Bedrock never allow mode none). `lev providers retention` \
                   says what each provider keeps.",
            value: FieldValue::Bool(zero),
        });
        for (id, label, help) in Self::AGREEMENT_PROVIDERS {
            if !self.provider_selected(id) {
                continue;
            }
            self.defaults.push(Field {
                label,
                help,
                value: FieldValue::Bool(agreements.iter().any(|a| a == id)),
            });
        }
        self.defaults.push(Field {
            label: Self::FILE_UPLOADS_LABEL,
            help: "Put a large image, PDF, clip or recording in the provider's own file \
                   storage once and name it by id on every later request, rather than \
                   sending its bytes each turn (Anthropic, xAI, Grok and Meta). A run's \
                   uploads are deleted when it finishes, and each expires at the provider \
                   after a day in any case. Zero data retention turns uploads off whatever \
                   this says, since an upload is data the provider keeps.",
            value: FieldValue::Bool(uploads),
        });
    }

    /// Whether the row with `id` is selected.
    fn provider_selected(&self, id: &str) -> bool {
        self.providers
            .iter()
            .any(|row| row.selected && row.provider.id == id)
    }

    /// A toggle on the Defaults screen, by label; `None` while the form
    /// does not hold it.
    fn field_bool(&self, label: &str) -> Option<bool> {
        self.defaults.iter().find_map(|f| match f.value {
            FieldValue::Bool(b) if f.label == label => Some(b),
            _ => None,
        })
    }

    /// Whether zero data retention is asked for: the switch while the form
    /// holds it, else what the config says.
    pub(super) fn current_zero_retention(&self) -> bool {
        self.field_bool(Self::ZERO_RETENTION_LABEL)
            .unwrap_or(self.base.providers.zero_retention)
    }

    /// Whether parts are uploaded to provider file storage: the switch while
    /// the form holds it, else what the config says.
    pub(super) fn current_file_uploads(&self) -> bool {
        self.field_bool(Self::FILE_UPLOADS_LABEL)
            .unwrap_or(self.base.providers.file_uploads)
    }

    /// The providers an agreement is declared with: each offered row's
    /// toggle while the form holds it, else what the config says for that
    /// provider. A name the wizard does not offer (a script provider's)
    /// stays as the config wrote it.
    pub(super) fn current_agreements(&self) -> Vec<String> {
        let declared = &self.base.providers.zero_retention_agreements;
        let offered = |name: &str| {
            Self::AGREEMENT_PROVIDERS
                .iter()
                .any(|(id, _, _)| *id == name)
        };
        let mut out: Vec<String> = declared.iter().filter(|a| !offered(a)).cloned().collect();
        for (id, label, _) in Self::AGREEMENT_PROVIDERS {
            let held = self
                .field_bool(label)
                .unwrap_or_else(|| declared.iter().any(|a| a == id));
            if held {
                out.push(id.to_string());
            }
        }
        out
    }
}
