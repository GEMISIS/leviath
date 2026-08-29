//! OpenAI-compatible endpoints in the wizard: llama.cpp, LM Studio, and
//! anything else that speaks the API.
//!
//! The rest of the provider table is one row per provider with one credential
//! each, and a `[model_providers.<name>]` endpoint is neither: a machine can
//! run two llama.cpp servers on two ports, and each needs a name, an address,
//! maybe a key, maybe headers. So the pick-list rows for these are presets,
//! and picking one adds an *entry* the credential screen then edits as a small
//! form. Several entries can sit under one preset, and the screen offers add
//! and remove beside the fields.
//!
//! Everything here is data and transitions on [`Wizard`]; drawing lives in
//! `render`, keys in `input`.

use super::*;
use crate::config::{ModelProviderConfig, ModelProviderKind};

/// One `[model_providers.<name>]` endpoint, as the wizard edits it.
#[derive(Debug, Clone)]
pub struct EndpointRow {
    /// The pick-list row this entry sits under, by catalogue id.
    pub preset: &'static str,
    /// The table key, and the name a blueprint writes before the slash.
    pub name: String,
    /// Where the server listens, including any path prefix.
    pub base_url: String,
    /// Bearer token, or empty for a server that wants none.
    pub api_key: String,
    /// Extra headers as typed: `Name: value` pairs separated by semicolons.
    pub headers: String,
    /// Model ids typed by hand, comma-separated, for a server that will not
    /// list its own. Written to `models` and used only when detection fails.
    pub models: String,
    /// The model this entry should be asked for by default, picked from what
    /// detection found or from the hand-typed list.
    pub default_model: Option<String>,
    /// What the last check concluded.
    pub outcome: Outcome,
    /// A check is in flight.
    pub checking: bool,
}

/// The rows one entry occupies on the credential screen, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointField {
    /// The table key.
    Name,
    /// The server address.
    BaseUrl,
    /// The bearer token.
    ApiKey,
    /// Extra headers.
    Headers,
    /// Hand-typed model ids.
    Models,
    /// The default model, cycled rather than typed.
    DefaultModel,
    /// Run the check.
    Verify,
    /// Take the entry out.
    Remove,
}

impl EndpointField {
    /// Every field, in screen order.
    pub const ALL: [EndpointField; 8] = [
        EndpointField::Name,
        EndpointField::BaseUrl,
        EndpointField::ApiKey,
        EndpointField::Headers,
        EndpointField::Models,
        EndpointField::DefaultModel,
        EndpointField::Verify,
        EndpointField::Remove,
    ];

    /// The label in front of the value.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::BaseUrl => "Base URL",
            Self::ApiKey => "API key",
            Self::Headers => "Headers",
            Self::Models => "Models",
            Self::DefaultModel => "Default model",
            Self::Verify => "Check this endpoint",
            Self::Remove => "Remove this endpoint",
        }
    }

    /// One line under the value saying what it is for.
    pub(crate) fn help(self) -> &'static str {
        match self {
            Self::Name => {
                "What blueprints call this provider, as in name/model. Letters, digits, - and _."
            }
            Self::BaseUrl => "Where the server listens, including the /v1 prefix.",
            Self::ApiKey => "Sent as a bearer token. Leave empty for a server that wants none.",
            Self::Headers => "Extra headers as Name: value, several separated by semicolons.",
            Self::Models => {
                "Only for a server that does not list its models: ids, comma-separated."
            }
            Self::DefaultModel => "Left and right to choose from what the check found.",
            Self::Verify | Self::Remove => "",
        }
    }

    /// Whether Enter opens a text editor on this row.
    pub(crate) fn is_text(self) -> bool {
        matches!(
            self,
            Self::Name | Self::BaseUrl | Self::ApiKey | Self::Headers | Self::Models
        )
    }
}

/// Where the cursor sits on an endpoint preset's credential screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointCursor {
    /// On a field of the entry at this index into `Wizard::endpoints`.
    Field(usize, EndpointField),
    /// On the "add another" row after the last entry.
    Add,
}

/// The rows a name may be made of, so a table key is also a path-safe name.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `Name: value; Other: value` as pairs, in order. A piece with no colon is
/// skipped rather than refused: the screen shows what was typed, and a stray
/// semicolon should not lose the rest.
pub(crate) fn parse_headers(text: &str) -> Vec<(String, String)> {
    text.split(';')
        .filter_map(|piece| {
            let (name, value) = piece.split_once(':')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// The inverse of [`parse_headers`], for an entry loaded from the config.
fn format_headers(headers: &std::collections::BTreeMap<String, String>) -> String {
    headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Comma-separated ids, trimmed, empties dropped.
pub(crate) fn parse_models(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

impl EndpointRow {
    /// A fresh entry under `preset`, named uniquely against `taken`.
    fn fresh(preset: &Provider, taken: &[String]) -> Self {
        let mut name = preset.id.to_string();
        let mut n = 1;
        while taken.contains(&name) {
            n += 1;
            name = format!("{}-{n}", preset.id);
        }
        Self {
            preset: preset.id,
            name,
            base_url: preset.preset_url.unwrap_or_default().to_string(),
            api_key: String::new(),
            headers: String::new(),
            models: String::new(),
            default_model: None,
            outcome: Outcome::Skipped,
            checking: false,
        }
    }

    /// An entry read back out of a config file.
    fn from_config(name: &str, entry: &ModelProviderConfig) -> Self {
        Self {
            preset: catalog::preset_for(name, entry),
            name: name.to_string(),
            base_url: entry.base_url.clone().unwrap_or_default(),
            api_key: entry.api_key.clone().unwrap_or_default(),
            headers: entry
                .headers
                .as_ref()
                .map(format_headers)
                .unwrap_or_default(),
            models: entry.models.as_deref().unwrap_or_default().join(", "),
            default_model: None,
            outcome: Outcome::Skipped,
            checking: false,
        }
    }

    /// The ids the default model may be chosen from: what the check found,
    /// or what was typed when it found nothing.
    pub(crate) fn model_choices(&self) -> Vec<String> {
        let detected = self.outcome.models();
        if detected.is_empty() {
            parse_models(&self.models)
        } else {
            detected.to_vec()
        }
    }

    /// The config entry this row writes, on top of `existing` when the
    /// config already had one under this name, so a `rate_limit` or `serves`
    /// set by hand survives a pass through the wizard.
    fn to_config(&self, existing: Option<&ModelProviderConfig>) -> ModelProviderConfig {
        let mut entry = existing.cloned().unwrap_or_default();
        entry.kind = Some(ModelProviderKind::OpenaiCompatible);
        entry.script = None;
        entry.base_url = Some(self.base_url.trim().to_string());
        entry.api_key = Some(self.api_key.trim().to_string()).filter(|k| !k.is_empty());
        let headers: std::collections::BTreeMap<String, String> =
            parse_headers(&self.headers).into_iter().collect();
        entry.headers = (!headers.is_empty()).then_some(headers);
        let models = parse_models(&self.models);
        entry.models = (!models.is_empty()).then_some(models);
        entry
    }
}

impl Wizard {
    /// The endpoint entries read out of `base`, in name order.
    pub(super) fn endpoints_from_config(base: &Config) -> Vec<EndpointRow> {
        let mut rows: Vec<EndpointRow> = base
            .model_providers
            .iter()
            .filter(|(_, entry)| entry.is_endpoint())
            .map(|(name, entry)| EndpointRow::from_config(name, entry))
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Indices into `self.endpoints` of the entries under `preset`.
    pub(crate) fn endpoints_under(&self, preset: &str) -> Vec<usize> {
        self.endpoints
            .iter()
            .enumerate()
            .filter(|(_, e)| e.preset == preset)
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether the provider row at `index` is an endpoint preset.
    pub(crate) fn is_endpoint_preset(&self, index: usize) -> bool {
        self.providers
            .get(index)
            .is_some_and(|row| row.provider.credential == Credential::Endpoint)
    }

    /// The names every entry currently holds.
    fn endpoint_names(&self) -> Vec<String> {
        self.endpoints.iter().map(|e| e.name.clone()).collect()
    }

    /// Add a fresh entry under the preset at provider row `index`, and select
    /// that row. Returns the new entry's index.
    pub(crate) fn add_endpoint(&mut self, index: usize) -> Option<usize> {
        let preset = self.providers.get(index)?.provider.clone();
        if preset.credential != Credential::Endpoint {
            return None;
        }
        let taken = self.endpoint_names();
        self.endpoints.push(EndpointRow::fresh(&preset, &taken));
        self.providers[index].selected = true;
        self.dirty = true;
        Some(self.endpoints.len() - 1)
    }

    /// Take the entry at `entry` out. When it was the last one under its
    /// preset, the preset's row is deselected too.
    pub(crate) fn remove_endpoint(&mut self, entry: usize) {
        if entry >= self.endpoints.len() {
            return;
        }
        let preset = self.endpoints.remove(entry).preset;
        self.dirty = true;
        if self.endpoints_under(preset).is_empty()
            && let Some(row) = self.providers.iter_mut().find(|r| r.provider.id == preset)
        {
            row.selected = false;
        }
    }

    /// Take every entry under `preset` out, for a deselected preset row.
    pub(crate) fn remove_endpoints_under(&mut self, preset: &str) {
        self.endpoints.retain(|e| e.preset != preset);
    }

    /// How many cursor rows the credential screen has for the preset at
    /// provider row `index`: every entry's fields, then the add row.
    pub(crate) fn endpoint_row_count(&self, index: usize) -> usize {
        let preset = self.providers[index].provider.id;
        self.endpoints_under(preset).len() * EndpointField::ALL.len() + 1
    }

    /// What the cursor is on, for the preset at provider row `index`.
    pub(crate) fn endpoint_cursor(&self, index: usize) -> Option<EndpointCursor> {
        let preset = self.providers.get(index)?.provider.id;
        let entries = self.endpoints_under(preset);
        let per_entry = EndpointField::ALL.len();
        let cursor = self.cursor;
        if cursor == entries.len() * per_entry {
            return Some(EndpointCursor::Add);
        }
        let entry = *entries.get(cursor / per_entry)?;
        Some(EndpointCursor::Field(
            entry,
            EndpointField::ALL[cursor % per_entry],
        ))
    }

    /// Open the text editor on `field` of the entry at `entry`.
    pub(crate) fn open_endpoint_editor(&mut self, entry: usize, field: EndpointField) {
        let Some(row) = self.endpoints.get(entry) else {
            return;
        };
        let value = match field {
            EndpointField::Name => row.name.clone(),
            EndpointField::BaseUrl => row.base_url.clone(),
            EndpointField::ApiKey => row.api_key.clone(),
            EndpointField::Headers => row.headers.clone(),
            EndpointField::Models => row.models.clone(),
            EndpointField::DefaultModel | EndpointField::Verify | EndpointField::Remove => {
                return;
            }
        };
        self.edit = Some(Edit {
            target: EditTarget::Endpoint { entry, field },
            line: crate::tui::widgets::line_edit::LineEdit::new(
                value,
                field == EndpointField::ApiKey,
            ),
        });
    }

    /// Write a committed text edit back into the entry. A name that is empty,
    /// taken by another entry, or taken by a built-in provider is refused
    /// with a message and the old name kept: the name is a table key and a
    /// registry name, and either collision would make the entry unreachable.
    pub(super) fn commit_endpoint_edit(&mut self, entry: usize, field: EndpointField, text: &str) {
        let text = text.trim().to_string();
        let taken = self.endpoint_names();
        let Some(row) = self.endpoints.get_mut(entry) else {
            return;
        };
        match field {
            EndpointField::Name => {
                // A preset's id is a fine entry name (it is the default one);
                // a real provider's id would shadow it in the registry.
                let built_in = catalog::providers()
                    .iter()
                    .any(|p| p.id == text && p.credential != Credential::Endpoint);
                let other = taken
                    .iter()
                    .enumerate()
                    .any(|(i, name)| i != entry && *name == text);
                if !is_valid_name(&text) {
                    self.message = Some(
                        "A name is letters, digits, - and _ (it becomes a table key).".to_string(),
                    );
                } else if built_in || other {
                    self.message = Some(format!("The name '{text}' is already taken."));
                } else {
                    row.name = text;
                }
            }
            EndpointField::BaseUrl => row.base_url = text,
            EndpointField::ApiKey => row.api_key = text,
            EndpointField::Headers => row.headers = text,
            EndpointField::Models => {
                row.models = text;
                // The choices may have changed under the pick.
                if let Some(chosen) = &row.default_model
                    && !row.model_choices().contains(chosen)
                {
                    row.default_model = None;
                }
            }
            EndpointField::DefaultModel | EndpointField::Verify | EndpointField::Remove => {}
        }
        // Anything that changes what is sent stales the last check.
        if matches!(
            field,
            EndpointField::BaseUrl | EndpointField::ApiKey | EndpointField::Headers
        ) {
            row.outcome = Outcome::Skipped;
        }
    }

    /// Move the entry's default model through its choices.
    pub(crate) fn cycle_endpoint_model(&mut self, entry: usize, delta: isize) {
        let Some(row) = self.endpoints.get_mut(entry) else {
            return;
        };
        let choices = row.model_choices();
        if choices.is_empty() {
            self.message = Some("Check the endpoint first, or type its models.".to_string());
            return;
        }
        let current = row
            .default_model
            .as_ref()
            .and_then(|m| choices.iter().position(|c| c == m));
        let next = match current {
            Some(i) => (i as isize + delta).rem_euclid(choices.len() as isize) as usize,
            None if delta < 0 => choices.len() - 1,
            None => 0,
        };
        row.default_model = Some(choices[next].clone());
        self.dirty = true;
    }

    /// Ask the background verifier about the entry at `entry`: a one-provider
    /// registry that lists the endpoint's models, exactly as the built-ins are
    /// checked. Nothing is sent for an entry with no address.
    pub(crate) fn request_endpoint_verification(&mut self, entry: usize) {
        let Some(row) = self.endpoints.get_mut(entry) else {
            return;
        };
        if row.base_url.trim().is_empty() {
            row.outcome = Outcome::Failed {
                message: "no base URL".to_string(),
            };
            return;
        }
        row.checking = true;
        let mut creds = leviath_runtime::provider_creds::ProviderCreds::openai_compatible(
            row.name.clone(),
            row.base_url.trim(),
            Some(row.api_key.trim().to_string()).filter(|k| !k.is_empty()),
            parse_headers(&row.headers),
            None,
            Vec::new(),
        );
        creds.request_timeout_secs = Some(20);
        let _ = self.verify_tx.send(VerifyRequest {
            provider_id: row.name.clone(),
            creds,
        });
    }

    /// Ask about every entry under the preset at provider row `index`.
    pub(crate) fn verify_endpoints_under(&mut self, index: usize) {
        let preset = self.providers[index].provider.id;
        for entry in self.endpoints_under(preset) {
            self.request_endpoint_verification(entry);
        }
    }

    /// Route a verifier's reply to the entry it answers for. `true` when one
    /// took it.
    pub(super) fn settle_endpoint_reply(&mut self, reply: &VerifyReply) -> bool {
        let Some(row) = self
            .endpoints
            .iter_mut()
            .find(|e| e.name == reply.provider_id)
        else {
            return false;
        };
        row.checking = false;
        row.outcome = reply.outcome.clone();
        // A pick that the listing no longer names is dropped; the first id
        // the listing does name is offered when nothing was picked yet.
        let choices = row.model_choices();
        if row
            .default_model
            .as_ref()
            .is_some_and(|m| !choices.contains(m))
        {
            row.default_model = None;
        }
        if row.default_model.is_none() {
            row.default_model = choices.first().cloned();
        }
        true
    }

    /// The names of the entries under selected preset rows, for the default
    /// provider radio.
    pub(crate) fn selected_endpoint_names(&self) -> Vec<String> {
        self.selected_providers()
            .into_iter()
            .filter(|&i| self.is_endpoint_preset(i))
            .flat_map(|i| self.endpoints_under(self.providers[i].provider.id))
            .map(|e| self.endpoints[e].name.clone())
            .collect()
    }

    /// The default model the entry named `name` chose, if it did.
    pub(crate) fn endpoint_default_model(&self, name: &str) -> Option<String> {
        self.endpoints
            .iter()
            .find(|e| e.name == name)
            .and_then(|e| e.default_model.clone())
    }

    /// Fold the entries into `config`: every endpoint entry the file held is
    /// replaced by what the wizard holds now, and a script entry is untouched.
    pub(super) fn write_endpoints(&self, config: &mut Config) {
        let existing: HashMap<String, ModelProviderConfig> = config
            .model_providers
            .iter()
            .filter(|(_, e)| e.is_endpoint())
            .map(|(name, e)| (name.clone(), e.clone()))
            .collect();
        config.model_providers.retain(|_, e| !e.is_endpoint());
        let selected = self.selected_endpoint_names();
        for row in self.endpoints.iter().filter(|e| selected.contains(&e.name)) {
            config
                .model_providers
                .insert(row.name.clone(), row.to_config(existing.get(&row.name)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_wizard;
    use super::*;

    fn preset_index(wizard: &Wizard, id: &str) -> usize {
        wizard
            .providers
            .iter()
            .position(|r| r.provider.id == id)
            .expect("the preset is in the table")
    }

    #[test]
    fn headers_and_models_parse_forgivingly_and_headers_format_back() {
        assert_eq!(
            parse_headers(" X-Org: research ;junk; Accept:application/json"),
            vec![
                ("X-Org".to_string(), "research".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ]
        );
        assert!(parse_headers(": no name").is_empty());
        assert_eq!(
            parse_models("a, ,b ,"),
            vec!["a".to_string(), "b".to_string()]
        );
        let map: std::collections::BTreeMap<String, String> =
            parse_headers("B: 2; A: 1").into_iter().collect();
        assert_eq!(format_headers(&map), "A: 1; B: 2");
    }

    #[test]
    fn every_field_is_labelled_and_the_text_fields_are_the_first_five() {
        for field in EndpointField::ALL {
            assert!(!field.label().is_empty());
            // Every value row explains itself; the buttons say what they do
            // in their label.
            assert_eq!(
                field.help().is_empty(),
                matches!(field, EndpointField::Verify | EndpointField::Remove),
                "{field:?}"
            );
        }
        let text: Vec<bool> = EndpointField::ALL.iter().map(|f| f.is_text()).collect();
        assert_eq!(text, [true, true, true, true, true, false, false, false]);
        assert!(EndpointField::Verify.help().is_empty());
        assert!(!EndpointField::Headers.help().is_empty());
    }

    #[test]
    fn picking_a_preset_adds_a_named_entry_and_removing_the_last_deselects_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = test_wizard(dir.path());
        let llama = preset_index(&w, "llama-cpp");
        assert!(w.endpoints.is_empty());

        let first = w.add_endpoint(llama).expect("added");
        let second = w.add_endpoint(llama).expect("added");
        assert_eq!(w.endpoints[first].name, "llama-cpp");
        assert_eq!(w.endpoints[second].name, "llama-cpp-2");
        assert_eq!(w.endpoints[first].base_url, "http://localhost:8080/v1");
        assert!(w.providers[llama].selected);
        assert!(w.dirty);
        assert_eq!(w.endpoints_under("llama-cpp"), vec![0, 1]);
        assert_eq!(w.endpoint_row_count(llama), 2 * 8 + 1);

        // A row that is not a preset adds nothing.
        assert_eq!(w.add_endpoint(0), None);
        assert_eq!(w.add_endpoint(99), None);

        w.remove_endpoint(0);
        assert!(w.providers[llama].selected, "one entry is left");
        w.remove_endpoint(0);
        assert!(!w.providers[llama].selected, "the last one went");
        w.remove_endpoint(5);
        assert!(w.endpoints.is_empty());

        // The custom preset starts with no address.
        let custom = preset_index(&w, "openai-compatible");
        let entry = w.add_endpoint(custom).expect("added");
        assert!(w.endpoints[entry].base_url.is_empty());
        w.remove_endpoints_under("openai-compatible");
        assert!(w.endpoints.is_empty());
    }

    #[test]
    fn the_cursor_maps_onto_fields_and_the_add_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = test_wizard(dir.path());
        let llama = preset_index(&w, "llama-cpp");
        w.add_endpoint(llama);
        w.add_endpoint(llama);

        w.cursor = 0;
        assert_eq!(
            w.endpoint_cursor(llama),
            Some(EndpointCursor::Field(0, EndpointField::Name))
        );
        w.cursor = 8 + 5;
        assert_eq!(
            w.endpoint_cursor(llama),
            Some(EndpointCursor::Field(1, EndpointField::DefaultModel))
        );
        w.cursor = 16;
        assert_eq!(w.endpoint_cursor(llama), Some(EndpointCursor::Add));
        w.cursor = 17;
        assert_eq!(w.endpoint_cursor(llama), None, "the continue button");
        assert_eq!(w.endpoint_cursor(99), None);
        assert!(w.is_endpoint_preset(llama));
        assert!(!w.is_endpoint_preset(0));
    }

    #[test]
    fn edits_land_on_the_right_field_and_a_bad_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = test_wizard(dir.path());
        let custom = preset_index(&w, "openai-compatible");
        w.add_endpoint(custom);
        w.add_endpoint(custom);

        w.commit_endpoint_edit(0, EndpointField::Name, " vllm ");
        assert_eq!(w.endpoints[0].name, "vllm");
        w.commit_endpoint_edit(0, EndpointField::Name, "");
        assert_eq!(w.endpoints[0].name, "vllm", "empty is refused");
        assert!(w.message.take().unwrap().contains("letters"));
        w.commit_endpoint_edit(0, EndpointField::Name, "has space");
        assert_eq!(w.endpoints[0].name, "vllm");
        w.commit_endpoint_edit(0, EndpointField::Name, "ollama");
        assert_eq!(w.endpoints[0].name, "vllm", "a built-in name is refused");
        assert!(w.message.take().unwrap().contains("taken"));
        w.commit_endpoint_edit(0, EndpointField::Name, "openai-compatible-2");
        assert_eq!(
            w.endpoints[0].name, "vllm",
            "another entry's name is refused"
        );
        w.commit_endpoint_edit(0, EndpointField::Name, "lm-studio");
        assert_eq!(w.endpoints[0].name, "lm-studio", "a preset's id is allowed");
        w.commit_endpoint_edit(0, EndpointField::Name, "vllm");

        w.endpoints[0].outcome = Outcome::Reachable {
            models: vec!["m".to_string()],
        };
        w.commit_endpoint_edit(0, EndpointField::BaseUrl, "http://h:8000/v1 ");
        assert_eq!(w.endpoints[0].base_url, "http://h:8000/v1");
        assert_eq!(
            w.endpoints[0].outcome,
            Outcome::Skipped,
            "the check is stale"
        );
        w.commit_endpoint_edit(0, EndpointField::ApiKey, "k");
        assert_eq!(w.endpoints[0].api_key, "k");
        w.commit_endpoint_edit(0, EndpointField::Headers, "X: 1");
        assert_eq!(w.endpoints[0].headers, "X: 1");

        w.commit_endpoint_edit(0, EndpointField::Models, "a, b");
        w.endpoints[0].default_model = Some("b".to_string());
        w.commit_endpoint_edit(0, EndpointField::Models, "a");
        assert_eq!(w.endpoints[0].default_model, None, "the pick is gone");
        w.commit_endpoint_edit(0, EndpointField::Verify, "ignored");
        w.commit_endpoint_edit(9, EndpointField::Name, "ignored");
    }

    #[test]
    fn the_editor_opens_on_text_fields_only_and_masks_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = test_wizard(dir.path());
        let llama = preset_index(&w, "llama-cpp");
        w.add_endpoint(llama);

        w.open_endpoint_editor(0, EndpointField::ApiKey);
        let edit = w.edit.take().expect("opened");
        assert_eq!(
            edit.target,
            EditTarget::Endpoint {
                entry: 0,
                field: EndpointField::ApiKey
            }
        );
        for field in [
            EndpointField::Name,
            EndpointField::BaseUrl,
            EndpointField::Headers,
            EndpointField::Models,
        ] {
            w.open_endpoint_editor(0, field);
            assert!(w.edit.take().is_some(), "{field:?}");
        }
        w.open_endpoint_editor(0, EndpointField::DefaultModel);
        assert!(w.edit.is_none());
        w.open_endpoint_editor(3, EndpointField::Name);
        assert!(w.edit.is_none());
    }

    #[test]
    fn the_default_model_cycles_through_what_was_found_or_typed() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = test_wizard(dir.path());
        let llama = preset_index(&w, "llama-cpp");
        w.add_endpoint(llama);
        w.dirty = false;

        w.cycle_endpoint_model(0, 1);
        assert_eq!(w.endpoints[0].default_model, None);
        assert!(w.message.take().unwrap().contains("Check"));
        assert!(!w.dirty);

        w.endpoints[0].models = "x, y".to_string();
        w.cycle_endpoint_model(0, -1);
        assert_eq!(w.endpoints[0].default_model.as_deref(), Some("y"));
        w.cycle_endpoint_model(0, 1);
        assert_eq!(w.endpoints[0].default_model.as_deref(), Some("x"));
        w.cycle_endpoint_model(0, 1);
        assert_eq!(w.endpoints[0].default_model.as_deref(), Some("y"));
        assert!(w.dirty);

        // A listing outranks the typed ids.
        w.endpoints[0].outcome = Outcome::Reachable {
            models: vec!["listed".to_string()],
        };
        w.endpoints[0].default_model = None;
        w.cycle_endpoint_model(0, 1);
        assert_eq!(w.endpoints[0].default_model.as_deref(), Some("listed"));
        w.cycle_endpoint_model(7, 1);
    }

    #[tokio::test]
    async fn verification_sends_the_entry_as_an_endpoint_cred_and_the_reply_lands() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = test_wizard(dir.path());
        let (mut requests, replies) = w.take_verify_ends().expect("first take");
        let custom = preset_index(&w, "openai-compatible");
        w.add_endpoint(custom);

        // No address: refused on the spot, nothing sent.
        w.request_endpoint_verification(0);
        assert!(w.endpoints[0].outcome.is_failure());
        assert!(requests.try_recv().is_err());

        w.endpoints[0].base_url = "http://127.0.0.1:1/v1".to_string();
        w.endpoints[0].api_key = "k".to_string();
        w.endpoints[0].headers = "X-Org: r".to_string();
        w.verify_endpoints_under(custom);
        assert!(w.endpoints[0].checking);
        let request = requests.recv().await.expect("sent");
        assert_eq!(request.provider_id, "openai-compatible");
        let spec = leviath_runtime::provider_creds::EndpointSpec::from_creds(&request.creds)
            .expect("an endpoint cred");
        assert_eq!(spec.base_url, "http://127.0.0.1:1/v1");
        assert_eq!(spec.headers, vec![("X-Org".to_string(), "r".to_string())]);
        assert_eq!(request.creds.api_key.as_deref(), Some("k"));
        assert_eq!(request.creds.request_timeout_secs, Some(20));

        replies
            .send(VerifyReply {
                provider_id: "openai-compatible".to_string(),
                outcome: Outcome::Reachable {
                    models: vec!["gpt-mock".to_string()],
                },
            })
            .unwrap();
        w.drain_verifications();
        assert!(!w.endpoints[0].checking);
        assert_eq!(
            w.endpoints[0].default_model.as_deref(),
            Some("gpt-mock"),
            "the first listed id is offered"
        );
        assert_eq!(w.discovered_models(), vec!["gpt-mock".to_string()]);

        // A pick the next listing does not name is dropped for the first.
        w.endpoints[0].default_model = Some("gone".to_string());
        replies
            .send(VerifyReply {
                provider_id: "openai-compatible".to_string(),
                outcome: Outcome::Reachable {
                    models: vec!["other".to_string()],
                },
            })
            .unwrap();
        w.drain_verifications();
        assert_eq!(w.endpoints[0].default_model.as_deref(), Some("other"));

        // A pick the listing still names is kept.
        replies
            .send(VerifyReply {
                provider_id: "openai-compatible".to_string(),
                outcome: Outcome::Reachable {
                    models: vec!["first".to_string(), "other".to_string()],
                },
            })
            .unwrap();
        w.drain_verifications();
        assert_eq!(w.endpoints[0].default_model.as_deref(), Some("other"));

        // A reply for nobody is dropped.
        replies
            .send(VerifyReply {
                provider_id: "nobody".to_string(),
                outcome: Outcome::Skipped,
            })
            .unwrap();
        w.drain_verifications();
        w.request_endpoint_verification(4);
    }

    /// The preset's credential screen has no action rows of its own (each
    /// entry carries its buttons), and the chooser names an entry by its
    /// preset and address.
    #[test]
    fn a_preset_has_no_detail_actions_and_the_chooser_names_its_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = test_wizard(dir.path());
        let llama = preset_index(&w, "llama-cpp");
        w.add_endpoint(llama);
        w.enter(Step::ProviderDetail);
        assert_eq!(w.detail_row(), Some(llama));
        assert!(w.detail_actions().is_empty());

        assert_eq!(
            w.provider_detail("llama-cpp"),
            "llama.cpp at http://localhost:8080/v1"
        );
        assert_eq!(w.provider_detail("anthropic"), "Anthropic");
        assert_eq!(w.provider_detail("nobody"), "from your config");
    }

    #[test]
    fn entries_are_read_from_the_config_and_written_back_over_the_old_ones() {
        let mut base = Config::default();
        base.model_providers.insert(
            "lm-studio".to_string(),
            ModelProviderConfig {
                kind: Some(ModelProviderKind::OpenaiCompatible),
                base_url: Some("http://localhost:1234/v1".to_string()),
                api_key: Some("k".to_string()),
                headers: Some(
                    [("X-Org".to_string(), "r".to_string())]
                        .into_iter()
                        .collect(),
                ),
                models: Some(vec!["a".to_string(), "b".to_string()]),
                serves: Some(vec!["kept".to_string()]),
                ..Default::default()
            },
        );
        base.model_providers.insert(
            "stale".to_string(),
            ModelProviderConfig {
                kind: Some(ModelProviderKind::OpenaiCompatible),
                base_url: Some("http://old".to_string()),
                ..Default::default()
            },
        );
        base.model_providers.insert(
            "groq".to_string(),
            ModelProviderConfig {
                script: Some("groq.rhai".to_string()),
                ..Default::default()
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let mut w = Wizard::new(
            base,
            &|_| None,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        let names: Vec<&str> = w.endpoints.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["lm-studio", "stale"]);
        assert_eq!(w.endpoints[0].preset, "lm-studio");
        assert_eq!(w.endpoints[0].headers, "X-Org: r");
        assert_eq!(w.endpoints[0].models, "a, b");
        assert_eq!(w.endpoints[1].preset, "openai-compatible");
        let lm = preset_index(&w, "lm-studio");
        let custom = preset_index(&w, "openai-compatible");
        assert!(w.providers[lm].selected);
        assert!(w.providers[custom].selected);
        assert!(w.is_endpoint_preset(lm));

        // Drop the stale one, edit the other, and write.
        w.remove_endpoint(1);
        assert!(!w.providers[custom].selected);
        w.endpoints[0].api_key = String::new();
        w.endpoints[0].headers = String::new();
        w.endpoints[0].models = String::new();
        let config = w.build_config();
        assert!(!config.model_providers.contains_key("stale"));
        assert!(
            config.model_providers.contains_key("groq"),
            "scripts are untouched"
        );
        let lm = &config.model_providers["lm-studio"];
        assert!(lm.is_endpoint());
        assert_eq!(lm.api_key, None);
        assert_eq!(lm.headers, None);
        assert_eq!(lm.models, None);
        assert_eq!(
            lm.serves.as_deref(),
            Some(&["kept".to_string()][..]),
            "a hand-set field survives"
        );
        assert_eq!(w.selected_endpoint_names(), vec!["lm-studio".to_string()]);
    }

    #[test]
    fn the_default_provider_radio_offers_the_entries_and_their_model() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = test_wizard(dir.path());
        let llama = preset_index(&w, "llama-cpp");
        w.add_endpoint(llama);
        w.endpoints[0].outcome = Outcome::Reachable {
            models: vec!["qwen".to_string()],
        };
        w.endpoints[0].default_model = Some("qwen".to_string());
        w.enter(Step::Defaults);

        let providers = w.defaults[Wizard::PROVIDER_FIELD].value.options().to_vec();
        assert_eq!(providers, vec!["llama-cpp".to_string()]);
        assert_eq!(w.build_config().default_provider, "llama-cpp");
        assert_eq!(
            w.build_config().default_model.as_deref(),
            Some("qwen"),
            "the entry's pick becomes the default model"
        );
        assert_eq!(
            w.endpoint_default_model("llama-cpp").as_deref(),
            Some("qwen")
        );
        assert_eq!(w.endpoint_default_model("nobody"), None);
    }
}
