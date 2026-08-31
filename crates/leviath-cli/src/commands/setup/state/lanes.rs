//! The two background lanes, and how their answers land on a provider row.
//!
//! Both exist for the same reason: the wizard draws on a tick, and anything
//! that waits on somebody else must not wait on that thread. A credential
//! check is a round trip; a browser sign-in is a person. Neither belongs in
//! the draw loop, so each is asked over a channel and drained per tick.
//!
//! They are separate lanes rather than one, because a sign-in can hold its
//! channel open for minutes and would stall every check queued behind it.

use std::collections::HashMap;

use super::{Credential, Outcome, SigninAction, SigninEvent, SigninRequest, Step, VerifyRequest};
use super::{Wizard, catalog};

impl Wizard {
    /// Ask the background verifier about the provider at `index`.
    ///
    /// A provider with nothing to check is left alone rather than queued: a
    /// blank API key would fail with a message about the key rather than saying
    /// the obvious, that none was given.
    pub(crate) fn request_verification(&mut self, index: usize) {
        if self.is_endpoint_preset(index) {
            self.verify_endpoints_under(index);
            return;
        }
        let Some(row) = self.providers.get_mut(index) else {
            return;
        };
        if !row.has_credential() {
            row.outcome = Outcome::Skipped;
            return;
        }
        let id = row.provider.id.to_string();
        let key = if row.value.is_empty() {
            self.env_only
                .get(row.provider.env_var.unwrap_or_default())
                .cloned()
        } else {
            Some(row.value.clone())
        };
        let base_url = (row.provider.credential == Credential::BaseUrl).then(|| {
            if row.value.is_empty() {
                catalog::DEFAULT_OLLAMA_URL.to_string()
            } else {
                row.value.clone()
            }
        });
        let signin = row.provider.credential == Credential::Signin;
        row.checking = true;

        // A sign-in provider is built from a grant on disk rather than from
        // anything on this screen, and the registry is deliberately unwilling
        // to guess where that grant lives. Built the same way a run builds it,
        // so the wizard cannot report a sign-in working that a run would not
        // find.
        let options = if signin {
            crate::commands::run::session::codex_options(&self.base)
        } else {
            HashMap::new()
        };
        let creds = leviath_runtime::provider_creds::ProviderCreds {
            name: id.clone(),
            api_key: base_url.is_none().then_some(key).flatten(),
            base_url,
            model_capabilities: HashMap::new(),
            request_timeout_secs: Some(20),
            rate_limit: None,
            options,
        };
        // A closed receiver means the background task is gone; the row simply
        // stays "checking" and nothing else breaks.
        let _ = self.verify_tx.send(VerifyRequest {
            provider_id: id,
            creds,
        });
    }

    /// Ask about every selected provider at once.
    pub(crate) fn verify_all(&mut self) {
        for index in self.selected_providers() {
            self.request_verification(index);
        }
    }

    /// Take whatever the background verifier has answered.
    pub(crate) fn drain_verifications(&mut self) {
        let mut landed = false;
        while let Ok(reply) = self.reply_rx.try_recv() {
            // Entries first: an entry is named after its preset by default
            // (`llama-cpp`), and the preset's own row never asks for a check
            // under its id, so a reply carrying that name is the entry's.
            if self.settle_endpoint_reply(&reply) {
                landed = true;
            } else if let Some(row) = self
                .providers
                .iter_mut()
                .find(|r| r.provider.id == reply.provider_id)
            {
                row.checking = false;
                row.outcome = reply.outcome;
                landed = true;
            }
        }
        // A reply carries the model list, and the picker was built from
        // whatever had arrived when the screen opened. Without this, moving on
        // from a credential and straight into Defaults shows an empty picker
        // for the provider that was verified half a second ago. Rebuilding
        // keeps the current selection, so nothing the user chose is lost.
        if landed && self.step == Step::Defaults {
            self.rebuild_defaults();
        }
    }

    // ── Signing in ──────────────────────────────────────────────────────────

    /// Send the provider at `index` to the sign-in lane.
    ///
    /// The row goes into its waiting state here rather than when the lane
    /// picks the request up, so the screen changes on the key press instead of
    /// a tick or two later. Selecting the provider is implied: someone who
    /// signs in has said what they want more clearly than a checkbox does.
    pub(crate) fn request_signin(&mut self, index: usize, action: SigninAction) {
        let Some(row) = self.providers.get_mut(index) else {
            return;
        };
        let id = row.provider.id.to_string();
        if action == SigninAction::In {
            row.signing_in = true;
            row.authorize_url = None;
            row.selected = true;
            self.dirty = true;
        }
        // The old check described the old account. Leaving it up through a
        // sign-out would show a green tick beside "Not signed in".
        row.outcome = Outcome::Skipped;
        self.message = Some(
            match action {
                SigninAction::In => "Opening your browser…",
                SigninAction::Out => "Signing out…",
            }
            .to_string(),
        );
        // A closed receiver means the lane is gone; the row stays as it is and
        // nothing else breaks.
        let _ = self.signin_tx.send(SigninRequest {
            provider_id: id,
            action,
        });
    }

    /// Take whatever the sign-in lane has reported.
    pub(crate) fn drain_signins(&mut self) {
        while let Ok(event) = self.signin_reply_rx.try_recv() {
            let Some(row) = self
                .providers
                .iter_mut()
                .find(|r| r.provider.id == event.provider_id())
            else {
                continue;
            };
            match event {
                SigninEvent::Opened { url, .. } => {
                    row.authorize_url = Some(url);
                }
                SigninEvent::SignedIn { who, .. } => {
                    row.signing_in = false;
                    row.authorize_url = None;
                    row.signed_in = Some(who);
                    self.message = Some("Signed in. Check it to confirm it works.".to_string());
                }
                SigninEvent::SignedOut { .. } => {
                    row.signing_in = false;
                    row.authorize_url = None;
                    row.signed_in = None;
                    self.message = Some("Signed out. The provider is still enabled.".to_string());
                }
                SigninEvent::Failed { message, .. } => {
                    row.signing_in = false;
                    row.authorize_url = None;
                    row.outcome = Outcome::Failed { message };
                    self.message = None;
                }
            }
        }
    }
}
