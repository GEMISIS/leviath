//! Checked names for the things a run refers to. They live in
//! [`leviath_core::names`], beside the run's listing record that names a
//! yolo profile too. Minting a new run id takes randomness, so it is here.

pub use leviath_core::names::*;

/// How many random bits end a minted run id, written as 12 hex digits. Ids
/// collide only within one second for one stem, so 48 bits is far more than
/// needed while staying short enough to read in `lev ps` and the dashboard.
const RUN_ID_ENTROPY_BITS: u32 = 48;

/// A new run id, `<stem>-<unix seconds>-<12 random hex digits>`, for a stem
/// the caller has already folded to what a run id may hold.
///
/// The suffix is random rather than counted. Two processes minting for the
/// same stem in the same second (two `lev run`s, two embedders sharing a state
/// directory) know nothing of each other, so nothing either one counts keeps
/// their ids apart, and two runs sharing an id share a run directory. The
/// timestamp keeps ids sorting in the order runs started, and the dashboard's
/// short id, the part after the last `-`, is the random part.
pub fn mint_run_id(stem: &str) -> String {
    use rand::RngExt as _;
    let entropy: u64 = rand::rng().random::<u64>() >> (u64::BITS - RUN_ID_ENTROPY_BITS);
    format!(
        "{stem}-{}-{entropy:012x}",
        leviath_core::duration::now_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids minted in one second share a timestamp, so the suffix alone has to
    /// tell them apart.
    #[test]
    fn ids_minted_in_one_second_differ_in_their_random_suffix() {
        let ids: Vec<String> = (0..200).map(|_| mint_run_id("same-agent")).collect();
        let mut by_second: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for id in &ids {
            let mut parts = id.rsplitn(3, '-');
            let suffix = parts.next().unwrap();
            let secs = parts.next().unwrap();
            assert_eq!(parts.next(), Some("same-agent"));
            assert_eq!(suffix.len(), 12, "{id}");
            by_second.entry(secs).or_default().push(suffix);
        }
        let mut largest = 0;
        for (secs, suffixes) in &by_second {
            let distinct: std::collections::HashSet<&&str> = suffixes.iter().collect();
            assert_eq!(
                distinct.len(),
                suffixes.len(),
                "two runs in second {secs} share a suffix: {suffixes:?}"
            );
            largest = largest.max(suffixes.len());
        }
        // 200 calls take microseconds, so they cannot all land in distinct
        // seconds; without this the assertion above would be vacuous.
        assert!(
            largest > 1,
            "expected ids sharing a second, got {by_second:?}"
        );
    }
}
