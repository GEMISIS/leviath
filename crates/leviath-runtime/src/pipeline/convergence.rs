//! Telling a looping stage what its last visit actually produced.
//!
//! A stage that loops - `analyze` sending itself back to `gather`, `compare`
//! back to `survey` - has no idea how much its previous pass added, because
//! every visit starts against the same accumulated context and looks equally
//! productive from the inside. Measured on one run, `analyze` re-read 4.76M
//! input tokens across its visits to produce 47K of output: a hundred tokens
//! in for every one out, which is what a stage re-reading everything to add
//! almost nothing looks like.
//!
//! The counters already available do not catch it. `max_revisits` and the
//! `stuck` thresholds are both budgets, and a budget ends a stage whether or
//! not it was still finding things - it cuts off the productive pass and the
//! wasted one alike. What decides whether another pass is worth running is not
//! how many have run, it is whether the last one added anything.
//!
//! So this measures the yield of each visit and hands the number back on
//! re-entry. The model is not asked to introspect about whether it is making
//! progress, which is exactly the kind of question a confident model answers
//! wrongly: it is told what the last pass added, and the stage's transition
//! prompt names the edge to take when that is little. A stage still turning up
//! material keeps looping, and nothing is capped.
//!
//! ## What counts as progress
//!
//! The regions a blueprint marks `volatility = "grows"` - documented as "a
//! findings list, a bibliography, a transcript of what has been read". That is
//! already exactly the accumulating evidence, so no blueprint needs new
//! configuration to opt in, and a blueprint that marks nothing gets no note
//! rather than a wrong one.

use super::*;
use std::collections::HashMap;

/// Where the note goes when a blueprint declares it, else `conversation`.
pub(crate) const PROGRESS_REPORT_REGION: &str = "progress_report";

/// Sizes of the accumulating regions when the current stage was entered, so its
/// exit can be measured against them.
///
/// Holds the stage name as well: the delta is credited to the stage that was
/// running when it accrued, and by the time it is closed out the agent has
/// already moved on.
#[derive(Component, Debug, Clone)]
pub struct StageEntrySizes {
    /// The stage these sizes were taken on entry to.
    pub stage: String,
    /// Region name to token count at entry.
    pub sizes: HashMap<String, usize>,
}

/// What each visit to a stage added, oldest first, keyed by stage name.
///
/// Per stage rather than one list, because the comparison that matters is
/// between a loop's own visits: `gather` adding little says nothing about
/// whether `analyze` has converged.
#[derive(Component, Debug, Clone, Default)]
pub struct VisitYield(pub HashMap<String, Vec<Yield>>);

/// One visit's growth across the accumulating regions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Yield {
    /// Per-region growth in tokens, only regions that actually grew.
    pub grew: Vec<(String, usize)>,
}

impl Yield {
    /// Total tokens added across every accumulating region.
    pub fn total(&self) -> usize {
        self.grew.iter().map(|(_, n)| n).sum()
    }
}

/// Current sizes of the regions marked `Grows`.
///
/// Shrinkage is not tracked: a compacting pass can make a region smaller
/// without the run having lost anything, and reporting that as negative
/// progress would be worse than reporting nothing.
pub(crate) fn accumulating_sizes(window: &ContextWindow) -> HashMap<String, usize> {
    window
        .regions
        .iter()
        .filter(|r| r.volatility == leviath_core::region::Volatility::Grows)
        .map(|r| (r.name.clone(), r.current_tokens))
        .collect()
}

/// Growth from `before` to `after`, dropping regions that did not grow.
pub(crate) fn measure(before: &HashMap<String, usize>, after: &HashMap<String, usize>) -> Yield {
    let mut grew: Vec<(String, usize)> = after
        .iter()
        .filter_map(|(name, now)| {
            let was = before.get(name).copied().unwrap_or(0);
            now.checked_sub(was)
                .filter(|d| *d > 0)
                .map(|d| (name.clone(), d))
        })
        .collect();
    // Biggest first, so a note trimmed for space keeps the part that matters.
    grew.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Yield { grew }
}

/// The note handed to a stage being re-entered, or `None` when there is nothing
/// useful to say yet.
///
/// Returns `None` on a first visit (nothing to compare) and when no region
/// grew on any visit (a blueprint that marks nothing as `Grows`, where a note
/// would be measuring silence).
pub(crate) fn note(stage: &str, history: &[Yield]) -> Option<String> {
    let last = history.last()?;
    if history.iter().all(|y| y.total() == 0) {
        return None;
    }
    let describe = |y: &Yield| match y.grew.is_empty() {
        true => "nothing".to_string(),
        false => y
            .grew
            .iter()
            .map(|(name, n)| format!("{name} +{n}"))
            .collect::<Vec<_>>()
            .join(", "),
    };
    let mut out = format!(
        "[Progress in stage '{stage}'] Your last visit here added: {}.",
        describe(last)
    );
    if let Some(prev) = history.len().checked_sub(2).and_then(|i| history.get(i)) {
        out.push_str(&format!(" The visit before added: {}.", describe(prev)));
    }
    out.push_str(
        " This is what another pass has to beat. If this one is turning up little \
         that is new, the work here has converged and the right move is to go on \
         rather than round again - say so and take the forward edge. If you are \
         still finding material, keep going: nothing is capping you.",
    );
    Some(out)
}

/// What [`track_stage_progress`] reads: the entering stage, the window to
/// measure and write the note into, and the two components that carry the
/// history, both optional because the first stage of a run has neither.
type ProgressQuery = (
    Entity,
    &'static StageJustEntered,
    &'static mut ContextWindow,
    Option<&'static StageEntrySizes>,
    Option<&'static mut VisitYield>,
);

/// Close out the stage just left and open one for the stage just entered,
/// writing the previous yield into context when this stage has been here before.
///
/// Ordered with the other `StageJustEntered` consumers, before `sync_tool_stages`
/// clears the marker and therefore before the stage's first inference is built.
pub fn track_stage_progress(mut commands: Commands, mut agents: Query<ProgressQuery>) {
    crate::tick_scope::clear();
    for (entity, entered, mut window, previous, yields) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        let now = accumulating_sizes(&window);

        // Close out the stage that was running, crediting its growth to it.
        let mut history: HashMap<String, Vec<Yield>> =
            yields.as_ref().map(|y| y.0.clone()).unwrap_or_default();
        if let Some(prev) = previous {
            let y = measure(&prev.sizes, &now);
            history.entry(prev.stage.clone()).or_default().push(y);
        }

        // Then tell the stage being entered what its own last pass produced.
        // Only its own: `gather` adding little says nothing about `analyze`.
        if let Some(mine) = history.get(entered.name.as_str())
            && let Some(text) = note(&entered.name, mine)
        {
            let region = match window.get_region(PROGRESS_REPORT_REGION).is_some() {
                true => PROGRESS_REPORT_REGION,
                false => "conversation",
            };
            let tokens = leviath_core::estimate_tokens(&text);
            // Best-effort, like the stuck and error notes: an overflowing
            // region drops it rather than failing the stage.
            let _ = window.add_to_region(region, text, tokens);
        }

        commands
            .entity(entity)
            .insert(VisitYield(history))
            .insert(StageEntrySizes {
                stage: entered.name.clone(),
                sizes: now,
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::{Region, RegionKind, region::Volatility};

    fn grows(name: &str, tokens: usize) -> Region {
        let mut r = Region::new(name.to_string(), RegionKind::Pinned, 10_000);
        r.volatility = Volatility::Grows;
        r.current_tokens = tokens;
        r
    }

    fn window(regions: Vec<Region>) -> ContextWindow {
        let mut w = ContextWindow::new(100_000);
        for r in regions {
            w.add_region(r);
        }
        w
    }

    /// Only `Grows` regions are progress. A stable region is setup and a
    /// rewritten one is a scratchpad; counting either would call rewriting the
    /// same scratchpad ten times "progress".
    #[test]
    fn only_accumulating_regions_count_as_progress() {
        let mut stable = Region::new("query".to_string(), RegionKind::Pinned, 10_000);
        stable.volatility = Volatility::Stable;
        stable.current_tokens = 500;
        let mut scratch = Region::new("notes".to_string(), RegionKind::Pinned, 10_000);
        scratch.volatility = Volatility::Rewritten;
        scratch.current_tokens = 900;
        let w = window(vec![grows("sources_index", 120), stable, scratch]);

        let sizes = accumulating_sizes(&w);
        assert_eq!(sizes.len(), 1);
        assert_eq!(sizes.get("sources_index"), Some(&120));
    }

    /// Growth is per region, biggest first, and regions that did not grow are
    /// dropped rather than reported as zero.
    #[test]
    fn measure_reports_only_what_grew_biggest_first() {
        let before = HashMap::from([
            ("sources_index".to_string(), 100),
            ("challenges".to_string(), 50),
            ("sub_findings".to_string(), 10),
        ]);
        let after = HashMap::from([
            ("sources_index".to_string(), 130),
            ("challenges".to_string(), 250),
            ("sub_findings".to_string(), 10),
        ]);
        let y = measure(&before, &after);
        assert_eq!(
            y.grew,
            vec![
                ("challenges".to_string(), 200),
                ("sources_index".to_string(), 30)
            ],
            "sub_findings did not grow and must not appear"
        );
        assert_eq!(y.total(), 230);
    }

    /// Two regions that grew by the same amount order by name, so the note a
    /// run prints is the same on every run rather than following whatever order
    /// the map happened to iterate in.
    #[test]
    fn equal_growth_breaks_the_tie_on_name() {
        let before = HashMap::from([
            ("sub_findings".to_string(), 0),
            ("challenges".to_string(), 0),
        ]);
        let after = HashMap::from([
            ("sub_findings".to_string(), 50),
            ("challenges".to_string(), 50),
        ]);
        assert_eq!(
            measure(&before, &after).grew,
            vec![
                ("challenges".to_string(), 50),
                ("sub_findings".to_string(), 50)
            ]
        );
    }

    /// A region that shrank is not negative progress. Compaction makes a region
    /// smaller without the run having lost anything.
    #[test]
    fn a_region_that_shrank_is_not_negative_progress() {
        let before = HashMap::from([("sources".to_string(), 900)]);
        let after = HashMap::from([("sources".to_string(), 300)]);
        assert_eq!(measure(&before, &after), Yield::default());
    }

    /// A region that appears mid-run counts from zero rather than being skipped.
    #[test]
    fn a_region_that_did_not_exist_before_counts_from_zero() {
        let after = HashMap::from([("claims".to_string(), 40)]);
        let y = measure(&HashMap::new(), &after);
        assert_eq!(y.grew, vec![("claims".to_string(), 40)]);
    }

    /// The note names both the last visit and the one before, so "little" is
    /// judged against this stage's own history rather than an absolute.
    #[test]
    fn the_note_compares_the_last_two_visits() {
        let history = vec![
            Yield {
                grew: vec![("sources_index".to_string(), 140)],
            },
            Yield {
                grew: vec![("sources_index".to_string(), 6)],
            },
        ];
        let n = note("analyze", &history).expect("a note");
        assert!(n.contains("last visit here added: sources_index +6"), "{n}");
        assert!(n.contains("visit before added: sources_index +140"), "{n}");
        assert!(n.contains("nothing is capping you"), "{n}");
    }

    /// A visit that added nothing says so in words rather than reporting an
    /// empty list, and still gets the note because an earlier visit produced.
    #[test]
    fn a_visit_that_added_nothing_is_described_as_nothing() {
        let history = vec![
            Yield {
                grew: vec![("claims".to_string(), 90)],
            },
            Yield::default(),
        ];
        let n = note("compare", &history).expect("a note");
        assert!(n.contains("last visit here added: nothing"), "{n}");
    }

    /// No note on a first visit: there is nothing to compare against, and a
    /// stage told "you added nothing" before it has run is being misinformed.
    #[test]
    fn a_first_visit_gets_no_note() {
        assert!(note("analyze", &[]).is_none());
    }

    /// A blueprint that marks no region `Grows` gets no note at all, rather
    /// than one reporting that nothing ever happens.
    #[test]
    fn a_blueprint_with_nothing_accumulating_gets_no_note() {
        let history = vec![Yield::default(), Yield::default()];
        assert!(note("analyze", &history).is_none());
    }
}

#[cfg(test)]
mod system_tests {
    use super::*;
    use bevy_ecs::schedule::Schedule;
    use leviath_core::{Region, RegionKind, region::Volatility};

    fn grows(name: &str, tokens: usize) -> Region {
        let mut r = Region::new(name.to_string(), RegionKind::Pinned, 100_000);
        r.volatility = Volatility::Grows;
        r.current_tokens = tokens;
        r
    }

    /// Spawn an agent sitting in `stage`, with a `conversation` region (which
    /// every blueprint must declare, and where the note lands by default) and
    /// one accumulating region at `sources`.
    fn agent_in(world: &mut World, stage: &str, sources: usize) -> Entity {
        let mut w = ContextWindow::new(1_000_000);
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Pinned,
            100_000,
        ));
        w.add_region(grows("sources_index", sources));
        world
            .spawn((
                w,
                StageJustEntered {
                    index: 0,
                    name: stage.to_string(),
                },
            ))
            .id()
    }

    fn enter(world: &mut World, e: Entity, stage: &str, sources: usize) {
        {
            let mut w = world.get_mut::<ContextWindow>(e).expect("window");
            let r = w
                .regions
                .iter_mut()
                .find(|r| r.name == "sources_index")
                .expect("sources_index");
            r.current_tokens = sources;
        }
        world.entity_mut(e).insert(StageJustEntered {
            index: 0,
            name: stage.to_string(),
        });
        let mut s = Schedule::default();
        s.add_systems(track_stage_progress);
        s.run(world);
    }

    fn conversation(world: &World, e: Entity) -> String {
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .content
            .iter()
            .map(|x| x.content.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole point, end to end: a stage revisited after a pass that added
    /// little is told so, in the numbers, with its own earlier pass to compare
    /// against.
    #[test]
    fn a_revisited_stage_is_told_what_its_last_pass_added() {
        let mut world = World::new();
        let e = agent_in(&mut world, "analyze", 0);
        let mut s = Schedule::default();
        s.add_systems(track_stage_progress);
        s.run(&mut world);

        // analyze -> gather: analyze's first pass grew sources by 500.
        enter(&mut world, e, "gather", 500);
        // gather -> analyze: nothing said yet about analyze's SECOND pass, but
        // its first is now on record.
        enter(&mut world, e, "analyze", 500);
        let first = conversation(&world, e);
        assert!(
            first.contains("last visit here added: sources_index +500"),
            "{first}"
        );

        // analyze's second pass adds almost nothing, and the third entry says so
        // with the productive pass alongside it for contrast.
        enter(&mut world, e, "gather", 504);
        enter(&mut world, e, "analyze", 504);
        let second = conversation(&world, e);
        assert!(
            second.contains("last visit here added: sources_index +4"),
            "the barren pass is reported: {second}"
        );
        assert!(
            second.contains("visit before added: sources_index +500"),
            "with the productive one to judge it against: {second}"
        );
    }

    /// Growth is credited to the stage that was running when it happened, not
    /// to whichever stage happens to be entered next.
    #[test]
    fn growth_is_credited_to_the_stage_that_produced_it() {
        let mut world = World::new();
        let e = agent_in(&mut world, "analyze", 0);
        let mut s = Schedule::default();
        s.add_systems(track_stage_progress);
        s.run(&mut world);

        // `gather` is what actually grows sources here.
        enter(&mut world, e, "gather", 0);
        enter(&mut world, e, "analyze", 900);

        let y = world.get::<VisitYield>(e).expect("tracked");
        assert_eq!(
            y.0.get("gather").map(|v| v.last().unwrap().total()),
            Some(900),
            "the 900 belongs to gather"
        );
        assert_eq!(
            y.0.get("analyze").map(|v| v.last().unwrap().total()),
            Some(0),
            "analyze's own pass produced nothing and must not be credited"
        );
    }

    /// A first visit says nothing. A stage told "you added nothing" before it
    /// has run once is being told something false.
    #[test]
    fn the_first_visit_to_a_stage_gets_no_note() {
        let mut world = World::new();
        let e = agent_in(&mut world, "analyze", 0);
        let mut s = Schedule::default();
        s.add_systems(track_stage_progress);
        s.run(&mut world);
        assert!(
            !conversation(&world, e).contains("[Progress in stage"),
            "no note on the way in"
        );
    }

    /// The note prefers a declared `progress_report` region, leaving the
    /// conversation clean for blueprints that want it separated.
    #[test]
    fn a_declared_progress_report_region_takes_the_note() {
        let mut world = World::new();
        let e = agent_in(&mut world, "analyze", 0);
        world
            .get_mut::<ContextWindow>(e)
            .unwrap()
            .add_region(Region::new(
                PROGRESS_REPORT_REGION.to_string(),
                RegionKind::Pinned,
                10_000,
            ));
        let mut s = Schedule::default();
        s.add_systems(track_stage_progress);
        s.run(&mut world);
        enter(&mut world, e, "gather", 300);
        enter(&mut world, e, "analyze", 300);

        let w = world.get::<ContextWindow>(e).unwrap();
        let report = w
            .get_region(PROGRESS_REPORT_REGION)
            .unwrap()
            .content
            .iter()
            .map(|x| x.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(report.contains("[Progress in stage 'analyze']"), "{report}");
        assert!(
            !conversation(&world, e).contains("[Progress in stage"),
            "and not duplicated into conversation"
        );
    }
}
