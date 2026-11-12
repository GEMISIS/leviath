//! How a run's cost reads wherever `lev` prints one.
//!
//! One wording for every surface, so a table, a status line and a summary
//! cannot describe the same run three ways. The rule they share is the cost
//! accounting's own: a figure is shown only when the cost of every call that
//! has landed is known, an unknown cost says how many calls are unknown and
//! why, and a figure with calls still in flight says so beside it.

use leviath_core::cost::UnknownCalls;

/// What a run, a stage or a visit spent, as the figures a cost line is
/// written from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Spent {
    /// The total, when the cost of every call is known.
    pub(crate) cost_usd: Option<f64>,
    /// The subtotal of the calls with a figure.
    pub(crate) priced_usd: f64,
    /// Calls whose cost the provider reported.
    pub(crate) reported_calls: usize,
    /// Calls whose cost was computed from the rate table.
    pub(crate) computed_calls: usize,
    /// Calls whose cost is unknown, by reason.
    pub(crate) unknown: UnknownCalls,
    /// Calls that are out and have not landed yet.
    pub(crate) calls_in_flight: usize,
}

impl Spent {
    /// A run's own figures, as its record lists them.
    pub(crate) fn of_run(meta: &leviath_core::run_meta::RunMeta) -> Self {
        Self {
            cost_usd: meta.cost_usd,
            priced_usd: meta.cost_priced_usd,
            reported_calls: meta.reported_calls,
            computed_calls: meta.computed_calls,
            unknown: meta
                .unknown
                .reconciled(meta.unknown_calls, meta.cost_usd.is_some()),
            calls_in_flight: meta.calls_in_flight,
        }
    }
}

/// A dollar figure to four decimals, and to six for a charge too small to
/// show in four: a call that cost a few millionths of a dollar was not free,
/// and `$0.0000` would say it was.
pub(crate) fn dollars(usd: f64) -> String {
    match usd > 0.0 && usd < 0.000_05 {
        true => format!("${usd:.6}"),
        false => format!("${usd:.4}"),
    }
}

/// One cost cell of a table: the figure, or `?` for a cost that is unknown.
/// Never `$0.0000` for unknown: a zero in this column is a claim that the
/// calls were free.
pub(crate) fn cell(cost_usd: Option<f64>) -> String {
    match cost_usd {
        Some(usd) => dollars(usd),
        None => "?".to_string(),
    }
}

/// A cost in the space of a status line: the figure, or `$?` with how many
/// calls are unknown.
pub(crate) fn brief(cost_usd: Option<f64>, unknown_calls: usize) -> String {
    match cost_usd {
        Some(usd) => dollars(usd),
        None => format!("$? ({unknown_calls} unknown)"),
    }
}

/// What `spent` comes to in the space of a status line: the figure, with
/// how many calls are in flight beside it while any is, which is how a run
/// reads while it works. A call whose cost is unknown leaves no figure to
/// show.
pub(crate) fn brief_of(spent: &Spent) -> String {
    let figure = brief(spent.cost_usd, spent.unknown.total() as usize);
    match spent.calls_in_flight {
        0 => figure,
        out => format!("{figure} ({} in flight)", calls(out)),
    }
}

/// `n` calls, in words.
fn calls(n: usize) -> String {
    match n {
        1 => "1 call".to_string(),
        n => format!("{n} calls"),
    }
}

/// A cost as a sentence or two: the figure with what it rests on, or that it
/// is unknown, for how many calls and why, with what the rest come to.
pub(crate) fn sentence(spent: &Spent) -> String {
    let known = spent.reported_calls + spent.computed_calls;
    let said = match (spent.cost_usd, spent.calls_in_flight) {
        (Some(usd), 0) => format!(
            "{}, exact: {} reported by the provider, {} computed from the rate table.",
            dollars(usd),
            calls(spent.reported_calls),
            calls(spent.computed_calls),
        ),
        (Some(usd), _) => format!(
            "{} so far: {} reported by the provider, {} computed from the rate table.",
            dollars(usd),
            calls(spent.reported_calls),
            calls(spent.computed_calls),
        ),
        (None, _) => format!(
            "unknown. {}. The {} with a figure come to {}.",
            spent.unknown.describe(),
            calls(known),
            dollars(spent.priced_usd),
        ),
    };
    match spent.calls_in_flight {
        0 => said,
        out => format!("{said} {} in flight, not in that figure.", calls(out)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::cost::CostUnknown;

    #[test]
    fn a_cell_is_a_figure_or_a_question_mark() {
        assert_eq!(cell(Some(0.25)), "$0.2500");
        assert_eq!(cell(Some(0.0)), "$0.0000");
        assert_eq!(cell(None), "?");
    }

    /// A charge too small for four decimals is still shown as a charge.
    #[test]
    fn a_tiny_charge_does_not_read_as_free() {
        assert_eq!(dollars(0.000_016_1), "$0.000016");
        assert_eq!(dollars(0.000_05), "$0.0001");
        assert_eq!(dollars(0.0), "$0.0000");
        assert_eq!(brief(Some(0.000_002), 0), "$0.000002");
    }

    #[test]
    fn a_brief_cost_counts_what_is_unknown() {
        assert_eq!(brief(Some(1.5), 0), "$1.5000");
        assert_eq!(brief(None, 2), "$? (2 unknown)");
    }

    /// While a run works, a call of its is in flight: the line shows what
    /// the calls that landed came to, and how many are out. An unknown call
    /// leaves no figure, in flight or not.
    #[test]
    fn a_line_shows_what_was_spent_so_far_while_a_call_is_in_flight() {
        let mut spent = Spent {
            cost_usd: Some(0.25),
            priced_usd: 0.25,
            reported_calls: 2,
            computed_calls: 0,
            unknown: UnknownCalls::default(),
            calls_in_flight: 0,
        };
        assert_eq!(brief_of(&spent), "$0.2500");
        spent.calls_in_flight = 1;
        assert_eq!(brief_of(&spent), "$0.2500 (1 call in flight)");
        assert_eq!(
            sentence(&spent),
            "$0.2500 so far: 2 calls reported by the provider, 0 calls computed from the rate \
             table. 1 call in flight, not in that figure."
        );
        spent.cost_usd = None;
        spent.unknown.add(CostUnknown::NoLookup);
        spent.calls_in_flight = 2;
        assert_eq!(brief_of(&spent), "$? (1 unknown) (2 calls in flight)");
        assert!(
            sentence(&spent).ends_with("come to $0.2500. 2 calls in flight, not in that figure.")
        );
        // A call still out on a run that has ended is not settled: unknown.
        spent.calls_in_flight = 0;
        spent.unknown.add(CostUnknown::NotSettled);
        assert_eq!(brief_of(&spent), "$? (2 unknown)");
    }

    #[test]
    fn a_known_cost_says_what_it_rests_on() {
        let spent = Spent {
            cost_usd: Some(0.4211),
            priced_usd: 0.4211,
            reported_calls: 12,
            computed_calls: 1,
            unknown: UnknownCalls::default(),
            calls_in_flight: 0,
        };
        assert_eq!(
            sentence(&spent),
            "$0.4211, exact: 12 calls reported by the provider, 1 call computed from the rate \
             table."
        );
    }

    #[test]
    fn an_unknown_cost_says_how_many_calls_and_why() {
        let mut unknown = UnknownCalls::default();
        unknown.add(CostUnknown::NoLookup);
        let spent = Spent {
            cost_usd: None,
            priced_usd: 0.5,
            reported_calls: 3,
            computed_calls: 0,
            unknown,
            calls_in_flight: 0,
        };
        assert_eq!(
            sentence(&spent),
            format!(
                "unknown. 1 call: {}. The 3 calls with a figure come to $0.5000.",
                CostUnknown::NoLookup.reason()
            )
        );
    }

    #[test]
    fn a_runs_figures_are_read_off_its_record() {
        let mut meta = crate::test_fixtures::fixtures::run_meta("run-c");
        meta.cost_usd = None;
        meta.cost_priced_usd = 2.0;
        meta.reported_calls = 4;
        meta.computed_calls = 1;
        // A record that counts its unknown calls without reasons.
        meta.unknown_calls = 2;
        meta.calls_in_flight = 3;
        let spent = Spent::of_run(&meta);
        assert_eq!(spent.calls_in_flight, 3);
        assert_eq!(spent.unknown.no_price, 2);
        assert_eq!((spent.reported_calls, spent.computed_calls), (4, 1));
        assert_eq!(spent.priced_usd, 2.0);
    }
}
