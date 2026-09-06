//! The model choosers: what the picker offers, what choosing means, and the
//! two model fields at the end of the advanced screen.
//!
//! Split out of the main state file for size. Everything here is about a
//! single model id and the provider it pairs with; the provider priority
//! itself lives in `priority`.

use super::limits::LIMITS_FIXED;
use super::{Field, FieldValue, Picker, PickerOption, Step, Wizard};

impl Wizard {
    /// What choosing one of these values actually decides.
    ///
    /// Written against `leviath_runtime::pipeline::resolve`, which is the code
    /// that reads them. The line about a blueprint winning is the one worth
    /// having: a user who sets a default and then watches an agent run on
    /// something else has been told, by every other tool, that a default is a
    /// default.
    pub(super) fn picker_explanation(&self, field: usize) -> Vec<&'static str> {
        if self.step == Step::Defaults && field == Self::PROVIDER_FIELD {
            vec![
                "Where your runs go by default. A stage that lists this provider among",
                "its models is served by it first, ahead of the blueprint's own order.",
                "",
                "This field does nothing on its own. Until you also set an override",
                "model on the advanced screen, the only thing it can do is reorder a",
                "list that already names this provider, so on a machine keyed for one",
                "provider that no blueprint mentions, every stage still goes elsewhere.",
                "",
                "It never overrides a blueprint that pins its provider: a stage with",
                "allow_user_default = false ignores this, and so does a provider you",
                "have not given a credential.",
            ]
        } else if self.step == Step::Limits && field == Self::FALLBACK_FIELD {
            vec![
                "The model a stage falls back to when none of the models it names is",
                "configured on this machine. It is asked of your default provider, and",
                "this name is never sent to a different provider.",
                "",
                "It sits behind everything a blueprint names and is never moved ahead",
                "of it, so a stage that can run what it asked for is not touched. Only",
                "a stage none of whose models you have configured lands here.",
                "",
                "Leaving it unset means such a stage fails at spawn with 'no usable",
                "provider' rather than running on something the author did not name.",
            ]
        } else {
            vec![
                "The model your default provider is asked for. The two travel together:",
                "this name is never sent to a different provider, so an OpenAI model is",
                "never asked of Anthropic.",
                "",
                "Setting it overrides every blueprint: the pair is offered to every",
                "stage that allows a user default and moves to the front of the models",
                "that stage lists, so cheap stages pay this model's price too.",
                "",
                "Leaving it unset, the usual choice, means each blueprint uses the",
                "models it names. The fallback model below is the gentler setting: it",
                "only carries a stage none of whose models you have configured.",
            ]
        }
    }

    /// Every model id any provider reported, deduplicated, for the picker.
    pub(crate) fn discovered_models(&self) -> Vec<String> {
        let mut models: Vec<String> = self
            .providers
            .iter()
            .filter(|r| r.selected)
            .flat_map(|r| r.outcome.models().iter().cloned())
            .collect();
        let selected = self.selected_endpoint_names();
        models.extend(
            self.endpoints
                .iter()
                .filter(|e| selected.contains(&e.name))
                .flat_map(|e| e.model_choices()),
        );
        models.sort();
        models.dedup();
        models
    }

    /// The two model fields at the end of the advanced screen, (re)built from
    /// what verification reported. A choice already made there survives the
    /// rebuild; before one is made, the override shows what the config holds
    /// or what an endpoint entry at the head of the priority picked for
    /// itself, and the fallback shows what the config holds.
    pub(in crate::commands::setup) fn rebuild_advanced_models(&mut self) {
        let mut models = vec![Self::NO_DEFAULT_MODEL.to_string()];
        models.extend(self.discovered_models());
        // The head of the priority, or the configured provider before the
        // form exists: the one the override pairs with.
        let head = self.current_default_provider();
        let override_now = self
            .chosen_model(Self::OVERRIDE_FIELD)
            .unwrap_or_else(|| {
                self.base
                    .override_model
                    .clone()
                    .or_else(|| self.endpoint_default_model(&head))
            })
            .unwrap_or_else(|| Self::NO_DEFAULT_MODEL.to_string());
        let fallback_now = self
            .chosen_model(Self::FALLBACK_FIELD)
            .unwrap_or_else(|| self.base.fallback_model.clone())
            .unwrap_or_else(|| Self::NO_DEFAULT_MODEL.to_string());
        let field = |label: &'static str, help: &'static str, current: String| {
            let mut options = models.clone();
            if !options.contains(&current) {
                options.push(current.clone());
            }
            let index = options
                .iter()
                .position(|m| *m == current)
                .unwrap_or_default();
            Field {
                label,
                help,
                value: FieldValue::Choice { options, index },
            }
        };
        self.limits.truncate(LIMITS_FIXED);
        self.limits.push(field(
            "Override model",
            "One model every stage that allows a user default starts on, ahead of what \
             its blueprint names, paired with your default provider. Leave it unset to \
             let each blueprint decide. Listed from what your providers reported.",
            override_now,
        ));
        self.limits.push(field(
            "Fallback model",
            "The model a stage falls back to when none of the models it names is \
             configured here, on your default provider. Never moves a stage off a \
             model its blueprint names.",
            fallback_now,
        ));
    }

    /// The model chosen on the advanced screen's field at `index`: `None`
    /// while the field has not been built, `Some(None)` for an explicit
    /// "(each blueprint decides)", `Some(Some(model))` for a pick.
    pub(super) fn chosen_model(&self, index: usize) -> Option<Option<String>> {
        match self.limits.get(index).map(|f| &f.value) {
            Some(FieldValue::Choice { options, index }) => Some(
                options
                    .get(*index)
                    .cloned()
                    .filter(|m| m != Self::NO_DEFAULT_MODEL),
            ),
            _ => None,
        }
    }

    /// Open the chooser for the Defaults field the cursor is on.
    ///
    /// The options come from the caller because it has already matched on the
    /// field's kind: re-reading them here would add a shape this cannot be in.
    pub(in crate::commands::setup) fn open_picker(
        &mut self,
        title: &'static str,
        options: Vec<String>,
        index: usize,
    ) {
        let field = self.cursor;
        let options = options
            .into_iter()
            .map(|value| {
                let detail = if field == Self::PROVIDER_FIELD {
                    self.provider_detail(&value)
                } else {
                    self.model_detail(&value)
                };
                PickerOption { value, detail }
            })
            .collect();
        self.picker_field = field;
        // Opening on the current value rather than at the top: the list is
        // long, and "where am I now" is the first thing you look for.
        self.picker = Some(Picker::new(
            title,
            self.picker_explanation(field)
                .into_iter()
                .map(str::to_string)
                .collect(),
            options,
            index,
        ));
    }

    /// What a provider id is, for the chooser's second column.
    pub(super) fn provider_detail(&self, id: &str) -> String {
        // Entries before rows: an entry is named after its preset by default,
        // and the preset row is never itself a choice here.
        if let Some(entry) = self.endpoints.iter().find(|e| e.name == id) {
            let preset = self
                .providers
                .iter()
                .find(|r| r.provider.id == entry.preset)
                .map_or(entry.preset, |r| r.provider.display);
            return format!("{preset} at {}", entry.base_url);
        }
        if let Some(row) = self.providers.iter().find(|r| r.provider.id == id) {
            return row.provider.display.to_string();
        }
        // A provider that is configured but not in the catalog: it came
        // from the config file, so it is still a legitimate choice.
        "from your config".to_string()
    }

    /// Which providers reported a model, so the row says where it came from.
    fn model_detail(&self, model: &str) -> String {
        // The first row is the absence of a model, not a model, so the
        // question "who reported it" does not apply to it.
        if model == Self::NO_DEFAULT_MODEL {
            return "no default; every blueprint uses the models it names".to_string();
        }
        let reported: Vec<&str> = self
            .providers
            .iter()
            .filter(|r| r.selected && r.outcome.models().iter().any(|m| m == model))
            .map(|r| r.provider.display)
            .collect();
        if reported.is_empty() {
            return "not reported by a provider you selected".to_string();
        }
        format!("reported by {}", reported.join(", "))
    }

    /// Take the chooser's answer (an index into its options), writing it
    /// back into the field it came from.
    pub(in crate::commands::setup) fn commit_picker(&mut self, chosen: usize) {
        // The chooser's options were built from this field's, one for one and
        // in order, so the option index *is* the field's index. Indexing rather
        // than looking up: the field is where it was when the chooser opened,
        // and nothing rebuilds the form while one is on screen.
        let field = self.picker_field;
        if let Some(fields) = self.fields_mut()
            && let Some(f) = fields.get_mut(field)
        {
            f.value.set_index(chosen);
        }
        self.dirty = true;
        // The provider field is an ordered priority now and opens the reorder
        // modal, not the chooser, so the fields this commits are the two model
        // choices on the advanced screen - the concurrency-follows-provider
        // adjustment lives in `commit_reorder`.
    }
}
