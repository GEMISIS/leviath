//! Tests for the write ceilings.

use super::*;

/// No limits configured is the code default, and it must impose nothing beyond
/// the disk - how much an agent should write is the user's judgement.
///
/// "Any size" means any size that fits: 50 GB onto a disk with 100 GB free is
/// allowed with no ceiling configured, which is the case the default is for.
/// A write larger than the free space is refused whatever the config says, and
/// that is the next test.
#[test]
fn unconfigured_limits_allow_anything_that_fits() {
    let free = MIN_FREE_BYTES * 100;
    assert_eq!(
        check_write(WriteLimits::default(), 0, free / 2, Some(free)),
        WriteVerdict::Allow
    );
}

/// The one ceiling nobody can configure away, because filling the disk harms
/// every other process on the machine rather than just this run.
#[test]
fn a_write_that_would_fill_the_disk_is_refused() {
    let verdict = check_write(WriteLimits::default(), 0, 500, Some(MIN_FREE_BYTES + 100));
    assert_eq!(
        verdict,
        WriteVerdict::OutOfSpace {
            available: MIN_FREE_BYTES + 100,
            required: MIN_FREE_BYTES,
        }
    );
    // The control: the same write with room to spare is fine.
    assert_eq!(
        check_write(WriteLimits::default(), 0, 500, Some(MIN_FREE_BYTES * 2)),
        WriteVerdict::Allow
    );
}

/// A filesystem the probe cannot read allows the write. A guard that cannot
/// measure has nothing to say, and refusing would block every write on any
/// filesystem `fs4` does not understand.
#[test]
fn an_unmeasurable_filesystem_does_not_refuse() {
    assert_eq!(
        check_write(WriteLimits::default(), 0, u64::MAX / 2, None),
        WriteVerdict::Allow
    );
}

/// ...but the configured ceilings still apply to it, so an unmeasurable
/// filesystem is not a way around them.
#[test]
fn an_unmeasurable_filesystem_still_obeys_the_configured_ceilings() {
    let limits = WriteLimits {
        per_call: Some(100),
        per_run: None,
    };
    assert!(matches!(
        check_write(limits, 0, 101, None),
        WriteVerdict::CallTooLarge { .. }
    ));
}

#[test]
fn the_per_call_ceiling_is_exclusive_at_the_boundary() {
    let limits = WriteLimits {
        per_call: Some(100),
        per_run: None,
    };
    let plenty = Some(MIN_FREE_BYTES * 100);
    assert_eq!(check_write(limits, 0, 100, plenty), WriteVerdict::Allow);
    assert_eq!(
        check_write(limits, 0, 101, plenty),
        WriteVerdict::CallTooLarge {
            bytes: 101,
            limit: 100
        }
    );
}

/// The per-run ceiling counts what came before, which is the case a per-call
/// ceiling alone misses: three calls of 14 GB each are individually plausible.
#[test]
fn the_per_run_ceiling_counts_earlier_calls() {
    let limits = WriteLimits {
        per_call: Some(1000),
        per_run: Some(1000),
    };
    let plenty = Some(MIN_FREE_BYTES * 100);
    // Under both, alone.
    assert_eq!(check_write(limits, 0, 600, plenty), WriteVerdict::Allow);
    // Under the per-call ceiling, over the run's.
    assert_eq!(
        check_write(limits, 600, 600, plenty),
        WriteVerdict::RunTooLarge {
            written: 1200,
            limit: 1000
        }
    );
}

/// A run already over budget stays refused even for a zero-byte write, rather
/// than letting an empty call slip past and reset anything.
#[test]
fn a_run_already_over_budget_refuses_even_an_empty_write() {
    let limits = WriteLimits {
        per_call: None,
        per_run: Some(100),
    };
    assert!(matches!(
        check_write(limits, 200, 0, Some(MIN_FREE_BYTES * 100)),
        WriteVerdict::RunTooLarge { .. }
    ));
}

/// Disk first when more than one applies. A user told "over the per-call limit"
/// goes and raises the limit, which is exactly wrong when the real problem is
/// that the machine is nearly full.
#[test]
fn a_full_disk_is_reported_before_a_configured_ceiling() {
    let limits = WriteLimits {
        per_call: Some(10),
        per_run: Some(10),
    };
    assert!(matches!(
        check_write(limits, 0, 5_000, Some(MIN_FREE_BYTES)),
        WriteVerdict::OutOfSpace { .. }
    ));
}

/// Accumulating the run total must not wrap, or a large enough history would
/// silently come back under the ceiling.
#[test]
fn a_run_total_that_would_overflow_saturates_rather_than_wrapping() {
    let limits = WriteLimits {
        per_call: None,
        per_run: Some(100),
    };
    assert!(matches!(
        check_write(limits, u64::MAX, 10, Some(MIN_FREE_BYTES * 100)),
        WriteVerdict::RunTooLarge { .. }
    ));
}

/// Every refusal names the number that was exceeded: a bare "write refused"
/// leaves a model guessing whether to retry smaller, and the retry is what
/// turns one refusal into a loop.
#[test]
fn every_refusal_names_its_number_and_allow_says_nothing() {
    assert_eq!(WriteVerdict::Allow.refusal(), None);
    for verdict in [
        WriteVerdict::OutOfSpace {
            available: 7,
            required: 9,
        },
        WriteVerdict::CallTooLarge {
            bytes: 11,
            limit: 13,
        },
        WriteVerdict::RunTooLarge {
            written: 17,
            limit: 19,
        },
    ] {
        let message = verdict.refusal().expect("a refusal explains itself");
        assert!(message.starts_with("[denied]"), "{message}");
        assert!(
            message.chars().any(|c| c.is_ascii_digit()),
            "no figure in: {message}"
        );
    }
    // The disk refusal must not point at a config key: there is none, and
    // sending someone to raise a limit that does not exist wastes their time.
    let disk = WriteVerdict::OutOfSpace {
        available: 7,
        required: 9,
    }
    .refusal()
    .expect("a refusal");
    assert!(!disk.contains("max_"), "{disk}");
}
