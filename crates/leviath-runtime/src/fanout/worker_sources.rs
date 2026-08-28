//! What a worker read reaches its parent's bibliography.
//!
//! A fan-out worker builds its own `sources_index` and then goes away. Without
//! this the parent's bibliography describes only the part of the run the parent
//! did itself, and the report cannot cite anything a worker found (issue #574).
//!
//! Kept apart from [`super`] because it is a different subject: that module is
//! how a fan-out is split, run and merged, this one is bibliographies and
//! deduplication by URL. They meet at one call, when a worker is reaped.

use super::{ContextWindow, Entity, World};

/// The region a research blueprint accumulates its bibliography in.
const SOURCES_REGION: &str = "sources_index";

/// Copy a finished worker's bibliography into its parent's, deduplicated by URL.
///
/// Called as the worker is reaped, while both entities still hold their context
/// windows - `slim_merged_workers` drops the worker's shortly afterwards, and
/// after that the sources exist only in the worker's run directory.
///
/// Merged lines are prefixed with the worker's item id and carry NO `[n]`
/// marker. Numbering is per agent: worker A's `[3]` and worker B's `[3]` are
/// different sources, and the findings already merged into `sub_findings` refer
/// to them by those numbers. Renumbering here would silently repoint those
/// references, so the merged entries are identified by URL instead, which is
/// what makes a source checkable in the first place.
///
/// Best-effort throughout, like the stuck and error notes: a parent with no
/// such region, a worker that recorded nothing, or a region too full to take
/// the lines all leave the parent exactly as it was.
/// The sources region of `entity`'s context window as one text, one entry
/// per line, if the entity has such a region.
fn sources_text(world: &World, entity: Entity) -> Option<String> {
    world
        .get::<ContextWindow>(entity)
        .and_then(|w| w.get_region(SOURCES_REGION))
        .map(|r| {
            r.content
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
}

pub(super) fn merge_worker_sources(
    world: &mut World,
    parent: Entity,
    worker: Entity,
    item_id: &str,
) {
    let Some(worker_text) = sources_text(world, worker) else {
        return;
    };

    let Some(existing) = sources_text(world, parent) else {
        // The parent does not keep a bibliography, so there is nothing this
        // could usefully merge into.
        return;
    };

    let fresh: Vec<String> = worker_text
        .lines()
        .filter_map(|line| {
            let url = source_url(line)?;
            // Deduplicated on URL rather than on the whole line: the same page
            // arrives from different workers with different titles and
            // different credibility notes, and all of those are one source.
            (!existing.contains(url.as_str()))
                .then(|| format!("[from {item_id}] {}", strip_marker(line)))
        })
        .collect();
    if fresh.is_empty() {
        return;
    }

    // The parent's window was read above to build `existing`, and reading its
    // region is what proved the window exists - so this cannot miss, and an
    // `if let` here would leave a branch no test can take.
    let mut window = world
        .get_mut::<ContextWindow>(parent)
        .expect("the parent's window was read to build `existing`");
    let headroom = window
        .get_region(SOURCES_REGION)
        .map(|r| r.max_tokens.saturating_sub(r.current_tokens))
        .unwrap_or(0);

    // As many of them as there is room for, rather than all or nothing.
    // `add_to_region` refuses an entry that does not fit, whole, and the refusal
    // used to be discarded - so a worker arriving at a nearly full region lost
    // every source it found. Measured on a 7-worker run: four bibliographies
    // landed, three vanished, and the run reported success.
    let mut taking = Vec::with_capacity(fresh.len());
    let mut used = 0usize;
    for line in &fresh {
        let cost = leviath_core::estimate_tokens(line) + 1; // the newline joining it
        if used + cost > headroom {
            break;
        }
        used += cost;
        taking.push(line.clone());
    }
    let dropped = fresh.len() - taking.len();
    if dropped > 0 {
        tracing::warn!(
            worker = %item_id,
            dropped,
            kept = taking.len(),
            headroom,
            "the parent's bibliography region is full, so some of this worker's \
             sources were not merged; the report cannot cite what is not here. \
             Raise this region's budget to keep a fan-out this wide"
        );
    }
    if taking.is_empty() {
        return;
    }
    // `used` rather than a fresh estimate of the joined block: it is the number
    // the loop above checked against the headroom, and the region decides by
    // comparing `current_tokens + tokens` against `max_tokens` with whatever it
    // is handed. Passing the checked number is what makes the write fit by
    // construction, so there is no failure here to handle - re-estimating would
    // reintroduce two numbers for one quantity and a branch to reconcile them.
    let _ = window.add_to_region(SOURCES_REGION, taking.join("\n"), used);
}

/// A bibliography line without its leading `[n]` citation marker.
///
/// The marker has to go, not just be prefixed: it is per agent, so carrying a
/// worker's `[1]` into a parent that has its own `[1]` puts two different
/// sources under one number. The findings merged into `sub_findings` still
/// refer to the worker's numbering, and repointing those references silently is
/// worse than the missing entries this whole merge exists to fix.
fn strip_marker(line: &str) -> String {
    let t = line.trim();
    match t.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((_, rest)) => rest.trim_start().to_string(),
        // A line that opens with no marker keeps whatever it does open with.
        None => t.to_string(),
    }
}

/// The first URL in a bibliography line, or `None` for a line that names no
/// source - a heading, a blank, or an entry recording a local path.
///
/// Trailing punctuation is trimmed because a line commonly ends "- <url> -
/// fetched ...", and a URL that keeps its trailing dash will not match the same
/// URL written without one.
fn source_url(line: &str) -> Option<String> {
    // Taken character by character rather than by byte index: a bibliography
    // line carries titles in whatever language the source is written in, and
    // slicing a byte offset out of one is how a panic gets shipped.
    let (_, after) = line.split_once("http")?;
    let tail: String = after
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ')' && *c != ']')
        .collect();
    let url = format!("http{tail}");
    let url = url.trim_end_matches(['-', ',', '.', ';']);
    (url.len() > "https://".len()).then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::ContextWindow;
    use bevy_ecs::world::World;

    fn sources_text(world: &World, e: Entity) -> String {
        world
            .get::<ContextWindow>(e)
            .and_then(|w| w.get_region("sources_index"))
            .map(|r| {
                r.content
                    .iter()
                    .map(|x| x.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
    fn window_with_sources(lines: &[&str]) -> ContextWindow {
        let mut w = ContextWindow::new(100_000);
        let mut r = leviath_core::Region::new(
            "sources_index".to_string(),
            leviath_core::RegionKind::Pinned,
            50_000,
        );
        r.volatility = leviath_core::region::Volatility::Grows;
        w.add_region(r);
        if !lines.is_empty() {
            let text = lines.join("\n");
            let tokens = leviath_core::estimate_tokens(&text);
            let _ = w.add_to_region("sources_index", text, tokens);
        }
        w
    }
    /// A parent's bibliography region left with exactly `headroom` tokens free,
    /// standing in for one that filled up earlier in a wide fan-out.
    fn window_with_headroom(headroom: usize) -> ContextWindow {
        const CAP: usize = 1_000;
        let mut w = ContextWindow::new(100_000);
        let mut r = leviath_core::Region::new(
            "sources_index".to_string(),
            leviath_core::RegionKind::Pinned,
            CAP,
        );
        r.volatility = leviath_core::region::Volatility::Grows;
        w.add_region(r);
        // The parent's own bibliography, declared at whatever size leaves the
        // headroom this test is about.
        let _ = w.add_to_region(
            "sources_index",
            "[0] Parent - https://parent.example/own".to_string(),
            CAP - headroom,
        );
        w
    }
    /// What a worker read reaches its parent's bibliography, so the region
    /// describes the whole run rather than only the part the parent did itself.
    #[test]
    fn a_workers_sources_reach_its_parent() {
        let mut world = World::new();
        let parent = world
            .spawn(window_with_sources(&[
                "[1] Parent page - https://parent.example/a",
            ]))
            .id();
        let worker = world
            .spawn(window_with_sources(&[
                "[1] Worker page - https://worker.example/x - credibility: high",
                "[2] Another - https://worker.example/y - credibility: med",
            ]))
            .id();

        merge_worker_sources(&mut world, parent, worker, "evaluation");

        let got = sources_text(&world, parent);
        assert!(got.contains("https://worker.example/x"), "{got}");
        assert!(got.contains("https://worker.example/y"), "{got}");
        assert!(
            got.contains("https://parent.example/a"),
            "the parent keeps its own"
        );
    }
    /// A worker arriving at a nearly full bibliography contributes what fits
    /// rather than nothing.
    ///
    /// This is what a finished 7-worker run did: `sources_index` ended at 19,461
    /// tokens of 20,000 holding four worker merges, and the other three workers'
    /// sources were nowhere. `add_to_region` refuses an over-budget entry whole
    /// and the refusal was discarded, so the run reported success having lost
    /// them.
    #[test]
    fn a_worker_arriving_at_a_full_region_still_contributes_what_fits() {
        let mut world = World::new();
        // Room for one or two short lines, not for all four.
        let parent = world.spawn(window_with_headroom(25)).id();
        let worker = world
            .spawn(window_with_sources(&[
                "[1] First - https://worker.example/1",
                "[2] Second - https://worker.example/2",
                "[3] Third - https://worker.example/3",
                "[4] Fourth - https://worker.example/4",
            ]))
            .id();

        merge_worker_sources(&mut world, parent, worker, "late-arrival");

        let got = sources_text(&world, parent);
        let landed = ["1", "2", "3", "4"]
            .iter()
            .filter(|n| got.contains(&format!("https://worker.example/{n}")))
            .count();
        assert!(landed > 0, "a full region used to take none of them: {got}");
        assert!(
            landed < 4,
            "this fixture is only meaningful while the region is too small for \
             all four; it took {landed}"
        );
    }
    /// A worker arriving at a region with no room at all contributes nothing,
    /// and says so rather than appearing to have had nothing to contribute.
    #[test]
    fn a_worker_arriving_at_a_region_with_no_room_contributes_nothing() {
        let mut world = World::new();
        let parent = world.spawn(window_with_headroom(0)).id();
        let worker = world
            .spawn(window_with_sources(&[
                "[1] First - https://worker.example/1",
            ]))
            .id();

        merge_worker_sources(&mut world, parent, worker, "too-late");

        let got = sources_text(&world, parent);
        assert!(
            !got.contains("worker.example"),
            "nothing fits, so nothing is written: {got}"
        );
        assert!(
            got.contains("parent.example"),
            "and the parent's own bibliography is untouched: {got}"
        );
    }

    /// The whole bibliography still lands when there is room for it, so the
    /// trimming above never costs a worker sources it could have contributed.
    #[test]
    fn a_worker_arriving_at_an_empty_region_contributes_everything() {
        let mut world = World::new();
        let parent = world.spawn(window_with_sources(&[])).id();
        let worker = world
            .spawn(window_with_sources(&[
                "[1] First - https://worker.example/1",
                "[2] Second - https://worker.example/2",
                "[3] Third - https://worker.example/3",
            ]))
            .id();

        merge_worker_sources(&mut world, parent, worker, "roomy");

        let got = sources_text(&world, parent);
        for n in ["1", "2", "3"] {
            assert!(
                got.contains(&format!("https://worker.example/{n}")),
                "source {n} missing with room to spare: {got}"
            );
        }
    }

    // ─── worker bibliographies reaching the parent (#574) ──────────────────

    /// Merged lines carry the worker they came from and NO citation number.
    ///
    /// `[n]` is per agent: this worker's `[1]` and the parent's `[1]` are
    /// different sources, and the findings already merged into `sub_findings`
    /// refer to them by those numbers. A merged line that kept or was given a
    /// number would silently repoint those references, and a wrong citation
    /// reads exactly like a correct one.
    #[test]
    fn merged_lines_are_attributed_and_unnumbered() {
        let mut world = World::new();
        let parent = world
            .spawn(window_with_sources(&[
                "[1] Parent - https://parent.example/a",
            ]))
            .id();
        let worker = world
            .spawn(window_with_sources(&[
                "[1] Worker - https://worker.example/x",
            ]))
            .id();

        merge_worker_sources(&mut world, parent, worker, "sandboxing");

        let merged: Vec<String> = sources_text(&world, parent)
            .lines()
            .filter(|l| l.contains("worker.example"))
            .map(str::to_string)
            .collect();
        assert_eq!(merged.len(), 1, "one merged line");
        assert!(merged[0].starts_with("[from sandboxing]"));
        // The parent now has exactly one `[1]`, its own.
        assert_eq!(sources_text(&world, parent).matches("[1]").count(), 1);
    }

    /// The same page found by two workers is one source. Deduplication is on
    /// URL, not on the whole line: the title and credibility note differ
    /// between workers and describe the same thing.
    #[test]
    fn the_same_url_from_two_workers_lands_once() {
        let mut world = World::new();
        let parent = world.spawn(window_with_sources(&[])).id();
        let a = world
            .spawn(window_with_sources(&[
                "[1] Spec - https://shared.example/spec - credibility: high",
            ]))
            .id();
        let b = world
            .spawn(window_with_sources(&[
                "[4] The Spec (2026) - https://shared.example/spec - credibility: med",
            ]))
            .id();

        merge_worker_sources(&mut world, parent, a, "one");
        merge_worker_sources(&mut world, parent, b, "two");

        assert_eq!(
            sources_text(&world, parent)
                .matches("https://shared.example/spec")
                .count(),
            1,
            "one source, however many workers found it"
        );
    }

    /// A parent that keeps no bibliography is left alone rather than having a
    /// region invented for it.
    #[test]
    fn a_parent_without_the_region_is_untouched() {
        let mut world = World::new();
        let parent = world.spawn(ContextWindow::new(10_000)).id();
        let worker = world
            .spawn(window_with_sources(&["[1] W - https://worker.example/x"]))
            .id();
        merge_worker_sources(&mut world, parent, worker, "x");
        assert_eq!(sources_text(&world, parent), "");
    }

    /// Lines naming no URL - headings, blanks, a local path - are not sources
    /// and are not carried up.
    #[test]
    fn lines_without_a_url_are_not_merged() {
        let mut world = World::new();
        let parent = world.spawn(window_with_sources(&[])).id();
        let worker = world
            .spawn(window_with_sources(&[
                "## Bibliography",
                "",
                "[1] Local notes - ./notes.md - credibility: low",
            ]))
            .id();
        merge_worker_sources(&mut world, parent, worker, "x");
        assert_eq!(sources_text(&world, parent), "", "nothing to carry");
    }

    /// A URL written with trailing punctuation is the same URL. Bibliography
    /// lines commonly read "- <url> - fetched ...", and a trailing dash kept on
    /// one copy would defeat the deduplication.
    #[test]
    fn a_trailing_dash_does_not_defeat_deduplication() {
        assert_eq!(
            source_url("[1] X - https://a.example/p - fetched 2026-08-22"),
            Some("https://a.example/p".to_string())
        );
        assert_eq!(source_url("[2] no url here - ./local.md"), None);
        // A line that opens with no marker keeps its text, and a non-ASCII
        // title must not be sliced through a character.
        assert_eq!(
            source_url("Übersicht über Agenten - https://de.example/seite - hoch"),
            Some("https://de.example/seite".to_string())
        );
        assert_eq!(
            strip_marker("Übersicht - https://de.example/s"),
            "Übersicht - https://de.example/s"
        );
    }
}
