//! Putting something into a region, and saying why.
//!
//! Every path that adds to a context window comes through here: the `on_write`
//! seam a custom region can veto at, the overflow retry a script gets one shot
//! at, and the shared tail that recounts the window. Split out of
//! `context_window` because "what an agent remembers" and "how something comes
//! to be remembered" are different questions, and because the second one now
//! also answers *why* - see [`ContextCause`] and [`ContextWindow::attach_journal`].

use leviath_core::{ContextCause, Region};

use super::{ContextTxn, ContextWindow, HookDecision, Pushed, WriteOrigin};

/// Everything one typed write needs besides its content and token count: where
/// it lands, whose write it is, why the region is changing, and what taint to
/// record.
///
/// A struct rather than five loose arguments because three of them are
/// enum-shaped or `Option`-shaped and trivially transposable at a call site,
/// which the compiler would not catch. Same reasoning as
/// [`crate::inference_usage::CallUsage`].
pub(crate) struct TypedWrite<'a> {
    /// Why the region is changing. `None` for a path that cannot say, which
    /// records nothing rather than guessing.
    pub cause: Option<ContextCause>,
    /// Whose write this is, which decides what a region hook's refusal does.
    pub origin: WriteOrigin,
    /// The region the entry lands in.
    pub region: &'a str,
    /// The entry's kind, which is what decides its message role.
    pub kind: leviath_core::EntryKind,
    /// The taint to record, where the region tracks it at all.
    pub taint: Option<leviath_core::TaintLevel>,
}

impl ContextWindow {
    /// The compiled script backing `region_name`, when it is a custom region
    /// whose script path has an entry in [`Self::region_scripts`].
    pub(super) fn custom_script_for(
        &self,
        region_name: &str,
    ) -> Option<std::sync::Arc<leviath_scripting::region_hook::RegionScript>> {
        let region = self.get_region(region_name)?;
        let leviath_core::RegionKind::Custom { script, .. } = &region.kind else {
            return None;
        };
        self.region_scripts.get(script).cloned()
    }

    /// Run a custom region's `on_write` hook (when defined) for an incoming
    /// entry. Non-custom regions, missing scripts, and hook failures all
    /// store the entry unchanged; what a `Refused` decision does is the
    /// origin adapters' business ([`Self::on_write_agent`],
    /// [`Self::on_write_system`]).
    ///
    /// Deliberately NOT invoked by the layout-swap carry or restore overlay:
    /// those re-add entries the hook already accepted once.
    fn on_write_decision(
        &self,
        region_name: &str,
        content: String,
        tokens: usize,
        kind: &leviath_core::EntryKind,
        key: Option<&str>,
    ) -> HookDecision {
        let Some(script) = self.custom_script_for(region_name) else {
            return HookDecision::Store(content, tokens, None);
        };
        if !script.has_on_write() {
            return HookDecision::Store(content, tokens, None);
        }
        // The region exists - custom_script_for resolved through it.
        let region = self
            .get_region(region_name)
            .expect("custom_script_for resolved through this region");
        let incoming = crate::custom_region::IncomingEntry {
            content: &content,
            tokens,
            kind,
            key,
        };
        match crate::custom_region::apply_on_write(&script, region, incoming) {
            crate::custom_region::OnWriteOutcome::Accept {
                content,
                tokens,
                key_override,
            } => HookDecision::Store(content, tokens, key_override),
            crate::custom_region::OnWriteOutcome::Reject(reason) => HookDecision::Refused {
                content,
                tokens,
                reason,
            },
        }
    }

    /// [`Self::on_write_decision`] for an agent-origin write: a refusal
    /// becomes an error carrying the hook's reason, which the tool result
    /// reports back to the model.
    pub(super) fn on_write_agent(
        &self,
        region_name: &str,
        content: String,
        tokens: usize,
        kind: &leviath_core::EntryKind,
        key: Option<&str>,
    ) -> leviath_core::Result<(String, usize, Option<String>)> {
        match self.on_write_decision(region_name, content, tokens, kind, key) {
            HookDecision::Store(content, tokens, key_override) => {
                Ok((content, tokens, key_override))
            }
            HookDecision::Refused { reason, .. } => Err(leviath_core::Error::RegionRefusedWrite {
                region: region_name.to_string(),
                reason: reason
                    .unwrap_or_else(|| "the region's on_write hook declined it".to_string()),
            }),
        }
    }

    /// [`Self::on_write_decision`] for a system-origin write: a refusal is
    /// downgraded to store-unchanged plus a warning, because these writes are
    /// framework records (assistant turns, delivered messages, nudges) that a
    /// script must never be able to silently delete.
    pub(super) fn on_write_system(
        &self,
        region_name: &str,
        content: String,
        tokens: usize,
        kind: &leviath_core::EntryKind,
        key: Option<&str>,
    ) -> (String, usize, Option<String>) {
        match self.on_write_decision(region_name, content, tokens, kind, key) {
            HookDecision::Store(content, tokens, key_override) => (content, tokens, key_override),
            HookDecision::Refused {
                content,
                tokens,
                reason,
            } => {
                tracing::warn!(
                    region = %region_name,
                    reason = reason.as_deref().unwrap_or("none given"),
                    "on_write rejected a system-origin write; storing unchanged \
                     (a script cannot drop framework records)"
                );
                (content, tokens, None)
            }
        }
    }

    /// Retry hook for a custom-region write that hit `TokenBudgetExceeded`:
    /// let the script's `on_overflow` free room, then report whether a single
    /// retry is worthwhile. Non-custom regions and hook failures leave the
    /// original error standing (the callers' existing truncation ladders
    /// apply).
    fn try_custom_overflow(&mut self, region_name: &str, incoming_tokens: usize) -> bool {
        let Some(script) = self.custom_script_for(region_name) else {
            return false;
        };
        if !script.has_on_overflow() {
            return false;
        }
        let region = self
            .get_region_mut(region_name)
            .expect("custom_script_for resolved through this region");
        let needed = (region.current_tokens + incoming_tokens).saturating_sub(region.max_tokens);
        let freed = crate::custom_region::apply_overflow(&script, region, needed);
        self.current_tokens = self.calculate_tokens();
        freed >= needed && needed > 0
    }

    /// Add content to a specific region, recording no cause.
    ///
    /// The embedder's door, and the terse form tests write through. A write
    /// through it leaves no
    /// [`RunRecord::ContextChange`](leviath_core::run_archive::RunRecord::ContextChange)
    /// at all, which is silence rather than a guess: a history is allowed to be
    /// incomplete and is not allowed to be wrong. Every writer this crate owns
    /// states its cause through the crate-internal `add_to_region_caused`, and a
    /// new one here should too.
    pub fn add_to_region(
        &mut self,
        region_name: &str,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        self.add_to_region_keyed(
            None,
            WriteOrigin::System,
            region_name,
            None,
            content,
            tokens,
        )
    }

    /// [`add_to_region`](Self::add_to_region) for a caller that can say why the
    /// region changed, which is every caller the runtime owns.
    pub(crate) fn add_to_region_caused(
        &mut self,
        cause: ContextCause,
        region_name: &str,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        self.add_to_region_keyed(
            Some(cause),
            WriteOrigin::System,
            region_name,
            None,
            content,
            tokens,
        )
    }

    /// Add an entry that may carry a key, so the agent can name it again to
    /// release it.
    ///
    /// Routed through the same private `write_to_region` tail
    /// as the unkeyed path, so a keyed write still passes the region's
    /// `on_write` hook and still gets `on_overflow` a chance to make room.
    /// Writing keys through a shortcut instead is how they came to be honoured
    /// on one region kind and dropped on the rest.
    ///
    /// `cause` is `None` only where the caller genuinely cannot name one; see
    /// [`add_to_region`](Self::add_to_region).
    pub(crate) fn add_to_region_keyed(
        &mut self,
        cause: Option<ContextCause>,
        origin: WriteOrigin,
        region_name: &str,
        key: Option<&str>,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        let before = self.begin_change(region_name);
        let (content, tokens, key_override) = match origin {
            WriteOrigin::Agent => self.on_write_agent(
                region_name,
                content,
                tokens,
                &leviath_core::EntryKind::Text,
                key,
            )?,
            WriteOrigin::System => self.on_write_system(
                region_name,
                content,
                tokens,
                &leviath_core::EntryKind::Text,
                key,
            ),
        };
        let key = key_override.as_deref().or(key);
        self.write_to_region(
            cause,
            region_name,
            before,
            tokens,
            &mut |region, tokens| match key {
                Some(k) => region.add_keyed_entry(k, content.clone(), tokens),
                None => region.add_entry(content.clone(), tokens),
            },
        )
    }

    /// Replace a region's content with a single (possibly keyed) entry on the
    /// agent's own behalf: `context_write`'s non-hashmap arm.
    ///
    /// The `on_write` hook runs BEFORE anything is cleared, so a rejection
    /// leaves the region exactly as it was - refusing the replacement and
    /// clearing anyway would be a second way to lose content.
    ///
    /// The change is measured from before the clear, so the record says the
    /// whole region left and one entry arrived rather than describing only the
    /// arrival.
    pub(crate) fn agent_replace_region(
        &mut self,
        cause: ContextCause,
        region_name: &str,
        key: Option<&str>,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        let before = self.begin_change(region_name);
        let (content, tokens, key_override) = self.on_write_agent(
            region_name,
            content,
            tokens,
            &leviath_core::EntryKind::Text,
            key,
        )?;
        let Some(region) = self.get_region_mut(region_name) else {
            return Err(leviath_core::Error::RegionNotFound(region_name.to_string()));
        };
        region.clear();
        self.current_tokens = self.calculate_tokens();
        let key = key_override.as_deref().or(key);
        self.write_to_region(
            Some(cause),
            region_name,
            before,
            tokens,
            &mut |region, tokens| match key {
                Some(k) => region.add_keyed_entry(k, content.clone(), tokens),
                None => region.add_entry(content.clone(), tokens),
            },
        )
    }

    /// Replace a region's entire content with a single entry (clear, then add).
    /// Returns `false` (no-op) if the region does not exist. Used to keep an
    /// authoritative document region (e.g. the plan) holding only its current
    /// version, so revisions build on it instead of accumulating stale copies.
    pub(crate) fn replace_region(
        &mut self,
        cause: ContextCause,
        region_name: &str,
        content: String,
        tokens: usize,
    ) -> bool {
        // The replacement passes through on_write like any incoming entry - a
        // custom region's script sees (and may transform) it. These callers
        // are all framework lanes (stage seeds, transforms, interaction
        // answers), so a hook rejection is downgraded inside the adapter to
        // store-unchanged plus a warning: a script that could veto the
        // replacement could silently delete an interaction answer.
        let before = self.begin_change(region_name);
        let (content, tokens, key_override) = self.on_write_system(
            region_name,
            content,
            tokens,
            &leviath_core::EntryKind::Text,
            None,
        );
        if let Some(region) = self.get_region_mut(region_name) {
            region.clear();
            let stored = match key_override.as_deref() {
                Some(k) => region.add_keyed_entry(k, content, tokens),
                None => region.add_entry(content, tokens),
            };
            self.current_tokens = self.calculate_tokens();
            self.commit_change(cause, before, Pushed::Into(usize::from(stored.is_ok())));
            true
        } else {
            false
        }
    }

    /// Add a typed entry to a specific region, recording no cause.
    ///
    /// Like [`add_to_region`](Self::add_to_region) but the entry carries an
    /// `EntryKind`, so message roles are determined by type rather than by
    /// parsing a text prefix - and unattributed in the same way. Written through
    /// by tests, where the entry's kind is the thing under test and its cause is
    /// not; a writer that knows why the region changed says so through
    /// [`add_typed_entry_caused`](Self::add_typed_entry_caused).
    #[cfg(test)]
    pub(crate) fn add_typed_entry(
        &mut self,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        self.add_assistant_turn(None, region_name, kind, content, tokens, None)
    }

    /// [`add_typed_entry`](Self::add_typed_entry) for a caller that can say why
    /// the region changed.
    pub(crate) fn add_typed_entry_caused(
        &mut self,
        cause: ContextCause,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        self.add_assistant_turn(Some(cause), region_name, kind, content, tokens, None)
    }

    /// [`add_typed_entry`](Self::add_typed_entry) for a turn that carries an
    /// opaque provider token to replay.
    ///
    /// A separate method rather than a parameter on the shared one: only the
    /// two writers that record an assistant turn have such a token, and the
    /// other twenty callers would carry a `None` that means nothing to them.
    pub(crate) fn add_assistant_turn(
        &mut self,
        cause: Option<ContextCause>,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: String,
        tokens: usize,
        reasoning: Option<String>,
    ) -> leviath_core::Result<()> {
        self.add_turn(
            cause,
            region_name,
            kind,
            leviath_core::region::EntryContent::text(content),
            tokens,
            reasoning,
        )
    }

    /// [`add_assistant_turn`](Self::add_assistant_turn) for a turn that
    /// carries parts beside its text: what a model that draws or speaks
    /// handed back. Records no cause, so it is the form a test reaches for;
    /// the reply lane itself goes through [`add_turn`](Self::add_turn) with
    /// [`ContextCause::ModelReply`].
    #[cfg(test)]
    pub(crate) fn add_assistant_turn_content(
        &mut self,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: leviath_core::region::EntryContent,
        tokens: usize,
        reasoning: Option<String>,
    ) -> leviath_core::Result<()> {
        self.add_turn(None, region_name, kind, content, tokens, reasoning)
    }

    /// Record one turn: its kind, its content (text and any parts), and the
    /// opaque reasoning token it carried.
    ///
    /// The shared body of every turn-shaped write, and the one that states a
    /// cause. `None` records nothing, which is what an embedder's write and a
    /// test's write both want.
    pub(crate) fn add_turn(
        &mut self,
        cause: Option<ContextCause>,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: leviath_core::region::EntryContent,
        tokens: usize,
        reasoning: Option<String>,
    ) -> leviath_core::Result<()> {
        self.typed_write_content(
            TypedWrite {
                cause,
                origin: WriteOrigin::System,
                region: region_name,
                kind,
                taint: None,
            },
            content,
            tokens,
        )?;
        // On success the entry just written is the last one: the region may
        // have evicted to make room, but it appends what it accepted. Same
        // after-the-push attachment the core's reasoning-carrying add uses.
        if reasoning.is_some()
            && let Some(region) = self.get_region_mut(region_name)
            && let Some(entry) = region.content.last_mut()
        {
            entry.reasoning = reasoning;
        }
        Ok(())
    }

    /// Shared core of the typed write methods: run the `on_write` seam with
    /// the caller's origin, then insert the entry with its kind and (when
    /// given) taint level, honouring a key override from the hook.
    pub(crate) fn typed_write(
        &mut self,
        write: TypedWrite<'_>,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        let content = leviath_core::region::EntryContent::text(content);
        self.typed_write_content(write, content, tokens)
    }

    /// Shared tail of every region write: run the insert, give a custom
    /// region's `on_overflow` one shot at freeing room when the budget
    /// rejects it, recount the window, and record what moved. A `&mut dyn
    /// FnMut` (not generic) keeps one instantiation for the coverage gate.
    ///
    /// `before` is the caller's measurement rather than one taken here, because
    /// a replacement has already cleared the region by the time it arrives and
    /// the record has to describe the clear too.
    pub(super) fn write_to_region(
        &mut self,
        cause: Option<ContextCause>,
        region_name: &str,
        before: ContextTxn,
        tokens: usize,
        insert: &mut dyn FnMut(&mut Region, usize) -> leviath_core::Result<()>,
    ) -> leviath_core::Result<()> {
        if self.get_region(region_name).is_none() {
            return Err(leviath_core::Error::RegionNotFound(region_name.to_string()));
        }
        let first = {
            let region = self.get_region_mut(region_name).expect("checked above");
            insert(region, tokens)
        };
        let stored = match first {
            Ok(()) => {
                self.current_tokens = self.calculate_tokens();
                Ok(())
            }
            Err(leviath_core::Error::TokenBudgetExceeded { .. })
                if self.try_custom_overflow(region_name, tokens) =>
            {
                let region = self.get_region_mut(region_name).expect("checked above");
                let retried = insert(region, tokens);
                self.current_tokens = self.calculate_tokens();
                retried
            }
            Err(e) => Err(e),
        };
        if let Some(cause) = cause {
            self.commit_change(cause, before, Pushed::Into(usize::from(stored.is_ok())));
        }
        stored
    }

    /// Add tainted content to a specific region.
    pub fn add_tainted_to_region(
        &mut self,
        cause: ContextCause,
        region_name: &str,
        content: String,
        tokens: usize,
        taint_level: leviath_core::TaintLevel,
    ) -> leviath_core::Result<()> {
        self.typed_write(
            TypedWrite {
                cause: Some(cause),
                origin: WriteOrigin::System,
                region: region_name,
                kind: leviath_core::EntryKind::Text,
                taint: Some(taint_level),
            },
            content,
            tokens,
        )
    }
}
