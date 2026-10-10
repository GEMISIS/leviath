//! The eviction cascade a full context window runs before an inference.

use super::*;

impl ContextWindow {
    /// Execute eviction cascade to free up space.
    ///
    /// Returns an `EvictionResult` with tokens freed and any regions that need
    /// LLM-based compaction. The caller is responsible for performing compaction
    /// on the listed regions (since it requires async LLM access).
    ///
    /// The phases go by region kind. Within each, the regions
    /// [`eviction_order`](Self::eviction_order) names go first, in its order,
    /// and the rest follow in the window's own order. A named region the phase
    /// cannot take from is passed over: naming a region never makes it
    /// evictable.
    pub fn try_evict(&mut self, target_free_tokens: usize) -> leviath_core::Result<EvictionResult> {
        use leviath_core::RegionKind;

        let initial_tokens = self.current_tokens;
        let (named, rest) = self.eviction_turns();

        // A region under `admission = "reject"` is exempt from every phase
        // below. Refusing writes to protect what a region holds would mean
        // nothing if the window-level cascade could take the same entries a
        // moment later - `reject` would only change which code did the silent
        // dropping. The agent releases from these, or nothing does.
        let has_evictable = self.regions.iter().any(Region::evictable);

        if !has_evictable {
            tracing::warn!(
                "Context window has no Clearable or Temporary regions. \
                 This may be intentional, but usually indicates a configuration error."
            );
        }

        // Phase 1: Clear Clearable regions (all-or-nothing)
        for &i in named.iter().chain(&rest) {
            let region = &mut self.regions[i];
            if matches!(region.kind, RegionKind::Clearable)
                && region.admission != leviath_core::region::Admission::Reject
                && !region.content.is_empty()
            {
                let freed = region.current_tokens;
                region.clear();
                self.current_tokens -= freed;
                tracing::debug!(
                    region = %region.name,
                    tokens_freed = freed,
                    "Cleared Clearable region (all-or-nothing)"
                );

                if self.max_tokens.saturating_sub(self.current_tokens) >= target_free_tokens {
                    return Ok(EvictionResult {
                        tokens_freed: initial_tokens - self.current_tokens,
                        needs_compaction: Vec::new(),
                    });
                }
            }
        }

        // Phase 1.5: Give each non-persistent custom region's on_overflow
        // hook first say over what IT loses, before the indiscriminate
        // oldest-first cascade below. A script that keeps errors and drops
        // successes only works if it runs before oldest-first does. Hook
        // absent/failing/insufficient → phase 2 makes the guaranteed
        // progress.
        let mut custom_freed = 0usize;
        for &i in named.iter().chain(&rest) {
            let needed = target_free_tokens
                .saturating_sub(self.max_tokens.saturating_sub(self.current_tokens));
            if needed == 0 {
                break;
            }
            let region = &self.regions[i];
            if !matches!(region.kind, RegionKind::Custom { pinned: false, .. })
                || region.admission == leviath_core::region::Admission::Reject
                || region.content.is_empty()
            {
                continue;
            }
            let Some(script) = self.custom_script_for(&region.name.clone()) else {
                continue;
            };
            if !script.has_on_overflow() {
                continue;
            }
            let freed = crate::custom_region::apply_overflow(&script, &mut self.regions[i], needed);
            self.current_tokens = self.current_tokens.saturating_sub(freed);
            custom_freed += freed;
            if freed > 0 {
                tracing::debug!(
                    region = %self.regions[i].name,
                    tokens_freed = freed,
                    "custom region's on_overflow chose its own evictions"
                );
            }
        }
        // Return early ONLY when a script's own drops satisfied the target -
        // otherwise phase 2 would immediately evict one more entry (it checks
        // the target *after* each eviction), overriding the script's
        // retention choice. Windows with no custom drops (custom_freed == 0)
        // go on to phase 2 unchanged.
        if custom_freed > 0
            && self.max_tokens.saturating_sub(self.current_tokens) >= target_free_tokens
        {
            return Ok(EvictionResult {
                tokens_freed: initial_tokens - self.current_tokens,
                needs_compaction: Vec::new(),
            });
        }

        // Phase 2: Evict from Temporary regions (oldest first, one at a time).
        // Non-persistent Custom regions join this phase: their script's
        // on_overflow hook (when present) has already had its say in phase
        // 1.5; oldest-first is the guaranteed-progress fallback.
        let gives_oldest = |r: &Region| {
            matches!(
                r.kind,
                RegionKind::Temporary | RegionKind::Custom { pinned: false, .. }
            ) && r.admission != leviath_core::region::Admission::Reject
        };
        // A region the eviction order names is emptied before the next one is
        // touched: that is what naming it asks for.
        for &i in &named {
            if !gives_oldest(&self.regions[i]) {
                continue;
            }
            while let Some(entry) = self.regions[i].remove_oldest() {
                let freed = entry.tokens;
                self.current_tokens -= freed;
                let region = &self.regions[i].name;
                tracing::debug!(
                    %region,
                    tokens_freed = freed,
                    "Evicted named region entry (oldest first)"
                );
                if self.max_tokens.saturating_sub(self.current_tokens) >= target_free_tokens {
                    return Ok(EvictionResult {
                        tokens_freed: initial_tokens - self.current_tokens,
                        needs_compaction: Vec::new(),
                    });
                }
            }
        }
        // The rest take turns, one entry each per round.
        loop {
            let mut evicted_any = false;

            for &i in &rest {
                let region = &mut self.regions[i];
                if gives_oldest(region)
                    && let Some(entry) = region.remove_oldest()
                {
                    let freed = entry.tokens;
                    self.current_tokens -= freed;
                    evicted_any = true;

                    tracing::debug!(
                        region = %region.name,
                        tokens_freed = freed,
                        "Evicted temporary region entry (oldest first)"
                    );

                    if self.max_tokens.saturating_sub(self.current_tokens) >= target_free_tokens {
                        return Ok(EvictionResult {
                            tokens_freed: initial_tokens - self.current_tokens,
                            needs_compaction: Vec::new(),
                        });
                    }
                }
            }

            if !evicted_any {
                break;
            }
        }

        // Phase 3: If still need space, identify Compacting regions that need compaction
        let mut needs_compaction = Vec::new();
        if self.max_tokens.saturating_sub(self.current_tokens) < target_free_tokens {
            for region in &self.regions {
                if region.needs_compaction() {
                    needs_compaction.push(region.name.clone());
                }
            }
        }

        // Phase 4: SlidingWindow regions are NEVER reduced
        // Phase 5: Pinned and CompactHistory regions are NEVER touched

        // Check for pinned regions over budget
        let pinned_tokens: usize = self
            .regions
            .iter()
            .filter(|r| {
                matches!(
                    r.kind,
                    RegionKind::Pinned
                        | RegionKind::CompactHistory { .. }
                        | RegionKind::Custom { pinned: true, .. }
                )
            })
            .map(|r| r.current_tokens)
            .sum();

        if pinned_tokens > self.max_tokens {
            return Err(leviath_core::Error::PinnedRegionsOverBudget {
                pinned_tokens,
                total_budget: self.max_tokens,
            });
        }

        Ok(EvictionResult {
            tokens_freed: initial_tokens - self.current_tokens,
            needs_compaction,
        })
    }

    /// The window's regions by position, in the two groups every eviction
    /// phase takes them in: the ones [`eviction_order`](Self::eviction_order)
    /// names, in its order, and then the rest, in the window's. A name with no
    /// region behind it, or one named twice, adds nothing.
    fn eviction_turns(&self) -> (Vec<usize>, Vec<usize>) {
        let mut named: Vec<usize> = Vec::new();
        for name in &self.eviction_order {
            if let Some(i) = self.regions.iter().position(|r| &r.name == name)
                && !named.contains(&i)
            {
                named.push(i);
            }
        }
        let rest = (0..self.regions.len())
            .filter(|i| !named.contains(i))
            .collect();
        (named, rest)
    }
}
