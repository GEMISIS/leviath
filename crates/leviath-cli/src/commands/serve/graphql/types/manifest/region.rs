//! What fills a region at spawn, and how it behaves once full.

use async_graphql::{Enum, SimpleObject, Union};
use leviath_graphql_derive::mirror;

use super::count;
use crate::commands::serve::graphql::scalars::Json;

/// How much a region's contents move between requests.
///
/// A prompt cache keys on an unchanged prefix, so this is what decides where a
/// region sits in the assembled prompt.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RegionVolatility {
    /// Written once and left alone.
    Stable,
    /// Appended to, keeping what is already there.
    Grows,
    /// Replaced wholesale.
    Rewritten,
}

impl From<leviath_core::region::Volatility> for RegionVolatility {
    fn from(volatility: leviath_core::region::Volatility) -> Self {
        use leviath_core::region::Volatility as Core;
        match volatility {
            Core::Stable => Self::Stable,
            Core::Grows => Self::Grows,
            Core::Rewritten => Self::Rewritten,
        }
    }
}

/// What happens to a write that does not fit.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RegionAdmission {
    /// The oldest entries roll off to make room.
    Evict,
    /// The write is refused, and what is there stays.
    Reject,
}

impl From<leviath_core::region::Admission> for RegionAdmission {
    fn from(admission: leviath_core::region::Admission) -> Self {
        use leviath_core::region::Admission as Core;
        match admission {
            Core::Evict => Self::Evict,
            Core::Reject => Self::Reject,
        }
    }
}

/// How a sliding region makes room.
///
/// It decides more than eviction: `PER_ITEM` shifts the prompt's prefix every
/// turn, which costs the prompt cache, while the other two keep it still
/// between eviction events.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RegionStrategy {
    /// One entry at a time, as soon as the region is over.
    PerItem,
    /// Nothing until the region is `overflow` entries over, then back down to
    /// its ceiling in one go.
    Bulk,
    /// The oldest entries are summarized into one, `compactCount` at a time.
    Compact,
}

/// A region's eviction strategy, with the number that arms it.
#[derive(Debug)]
pub(crate) struct RegionEviction {
    /// Which strategy.
    pub(crate) strategy: RegionStrategy,
    /// How many entries over the ceiling a bulk eviction waits for.
    pub(crate) overflow: Option<i32>,
    /// How many entries one compaction pass takes.
    pub(crate) compact_count: Option<i32>,
}

impl From<leviath_core::region::EvictionStrategy> for RegionEviction {
    fn from(strategy: leviath_core::region::EvictionStrategy) -> Self {
        use leviath_core::region::EvictionStrategy as Core;
        match strategy {
            Core::PerItem => Self {
                strategy: RegionStrategy::PerItem,
                overflow: None,
                compact_count: None,
            },
            Core::Bulk { overflow } => Self {
                strategy: RegionStrategy::Bulk,
                overflow: Some(count(overflow)),
                compact_count: None,
            },
            Core::Compact { compact_count } => Self {
                strategy: RegionStrategy::Compact,
                overflow: None,
                compact_count: Some(count(compact_count)),
            },
        }
    }
}

/// When a seed that runs tools runs again.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SeedRefresh {
    /// Once, at spawn. What every other kind of seed does.
    Once,
    /// Again whenever a stage is entered, replacing the region. For a seed whose
    /// answer moves, such as the time.
    EachStage,
}

impl From<leviath_core::layout::SeedRefresh> for SeedRefresh {
    fn from(refresh: leviath_core::layout::SeedRefresh) -> Self {
        use leviath_core::layout::SeedRefresh as Core;
        match refresh {
            Core::Once => Self::Once,
            Core::EachStage => Self::EachStage,
        }
    }
}

/// One tool call a seed makes.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct SeedToolCall {
    /// The tool to call, by the name the manifest used, so an MCP tool keeps
    /// its `<server>__<tool>` qualification.
    pub(crate) tool: String,
    /// Its arguments, as the manifest wrote them.
    ///
    /// Raw JSON, and not because nothing has typed it yet: a seed may name any
    /// tool this machine can offer, including an MCP tool and a script tool, so
    /// the shape is whatever that one tool's schema is. `tools` carries the
    /// schema a name resolves to. Empty for the many tools that take none.
    pub(crate) args: Json,
}

/// Filled at run time by whoever starts the run.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct SeedFromCaller {
    /// The caller-input key this region is filled from. The key `task` is the
    /// run's own prompt.
    pub(crate) key: String,
}

/// Filled from the working-directory files matching a pattern.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct SeedFromGlob {
    /// The pattern, resolved against the run's working directory.
    pub(crate) pattern: String,
}

/// Filled from a list of files.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct SeedFromFiles {
    /// The paths, resolved against the run's working directory.
    pub(crate) paths: Vec<String>,
}

/// Filled with text written into the blueprint.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct SeedFromLiteral {
    /// The text, verbatim.
    pub(crate) text: String,
}

/// Filled with what a Rhai script returns.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct SeedFromScript {
    /// The script, relative to the blueprint.
    pub(crate) script: String,
}

/// Filled with the output of a shell command run at spawn.
///
/// The one seed that executes something through a shell, and it runs before the
/// first inference, so before any approval prompt could exist. It runs inside
/// the entry stage's sandbox when one is configured, and the `allow_seed_commands`
/// switch turns the whole feature off.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct SeedFromCommand {
    /// The command line, run with the platform shell in the working directory.
    pub(crate) command: String,
}

/// Filled with the output of tool calls run at spawn.
///
/// Through the run's own tool layer rather than a shell, so each call answers to
/// the same permissions and taint rules it would answer to mid-run. That is what
/// makes an unrestricted list safe: a seed reaches nothing the run was not
/// already granted.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct SeedFromTools {
    /// The calls, in order. Each writes its own headed block into the region.
    pub(crate) calls: Vec<SeedToolCall>,
    /// Whether the calls run once or on every stage entry.
    pub(crate) refresh: SeedRefresh,
}

/// What fills a region before the first inference.
///
/// A region with no seed starts empty and is filled by the run. A union rather
/// than one object with a field per source: a seed has exactly one source, and a
/// bag of nullable fields would admit combinations no manifest can express.
#[mirror]
#[derive(Debug, Union)]
pub(crate) enum RegionSeed {
    /// From the caller.
    Caller(SeedFromCaller),
    /// From files matching a pattern.
    Glob(SeedFromGlob),
    /// From named files.
    Files(SeedFromFiles),
    /// From text in the blueprint.
    Literal(SeedFromLiteral),
    /// From a script.
    Script(SeedFromScript),
    /// From a shell command.
    Command(SeedFromCommand),
    /// From tool calls.
    Tools(SeedFromTools),
}

impl From<&leviath_core::layout::RegionSeed> for RegionSeed {
    fn from(seed: &leviath_core::layout::RegionSeed) -> Self {
        use leviath_core::layout::RegionSeed as Core;
        match seed {
            Core::CallerInput { name } => Self::Caller(SeedFromCaller { key: name.clone() }),
            Core::Glob { pattern } => Self::Glob(SeedFromGlob {
                pattern: pattern.clone(),
            }),
            Core::Files { paths } => Self::Files(SeedFromFiles {
                paths: paths.clone(),
            }),
            Core::Literal { text } => Self::Literal(SeedFromLiteral { text: text.clone() }),
            Core::Rhai { script } => Self::Script(SeedFromScript {
                script: script.clone(),
            }),
            Core::Command { command } => Self::Command(SeedFromCommand {
                command: command.clone(),
            }),
            Core::Tools { calls, refresh } => Self::Tools(SeedFromTools {
                calls: calls
                    .iter()
                    .map(|call| SeedToolCall {
                        tool: call.name.clone(),
                        args: Json(call.args.clone()),
                    })
                    .collect(),
                refresh: SeedRefresh::from(*refresh),
            }),
        }
    }
}

#[cfg(test)]
#[path = "region_tests.rs"]
mod tests;
