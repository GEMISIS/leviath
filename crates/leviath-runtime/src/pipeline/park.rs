//! Whether a failed provider call parks a run for a resume, and in what words.
//!
//! Asked by the stage lane, the routing lane and the stall watchdog, which
//! have to agree: the same failure a second apart must not park a run on one
//! lane and end it on another.

/// What a person has to do about a provider that could not be reached.
///
/// A separate constant rather than a line-continued literal inside the
/// `format!`: rustfmt reflows those, and it silently baked the source's own
/// indentation into the middle of the sentence a user reads.
const UNREACHABLE_REMEDY: &str =
    "check the network connection and the provider's base URL, then `lev resume` this run";

/// What to do about a provider that was reached and then did not answer in
/// time, or failed while answering.
const REACHED_REMEDY: &str =
    "the provider is up but did not finish this call; `lev resume` this run to try again";

/// Which blocker a call that ended without an answer parks a run under, from
/// what the transport knew about it: a provider never reached, one that took
/// too long, or one that answered with a failure. Shared with the stall
/// watchdog, which reads the same kind off the circuit a failure opened.
pub(crate) fn blocker_for_kind(
    kind: Option<leviath_providers::FailureKind>,
) -> leviath_core::run_meta::SetupBlocker {
    use leviath_core::run_meta::SetupBlocker;
    match kind {
        Some(leviath_providers::FailureKind::Timeout) => SetupBlocker::ProviderTimedOut,
        Some(k) if k.provider_was_reached() => SetupBlocker::ProviderFailed,
        _ => SetupBlocker::ProviderUnreachable,
    }
}

/// Whether a failed provider call is the machine's problem rather than the
/// run's, and if so what to tell the person who has to fix it.
///
/// `None` means the run itself is what went wrong and the caller should fail it.
///
/// Two lanes ask - the stage call in `collect_inference` and the routing call
/// at a stage boundary in `collect_transition_choice` - and they have to answer
/// the same way. Split the decision between them and one blip parks a run or
/// kills it depending on which call happened to be in flight when the network
/// went. The decision and the wording live here so the two cannot drift; what
/// each lane must do to keep its own continuation alive is still its own
/// business, because those genuinely differ.
pub(crate) fn setup_park(
    err: &leviath_providers::ProviderError,
    provider: &str,
) -> Option<(leviath_core::run_meta::SetupBlocker, String)> {
    use leviath_core::run_meta::SetupBlocker;
    use leviath_providers::UnavailableReason;

    match err.unavailable_reason()? {
        // Running out of credits is an account state, not a defect in the run:
        // the operator tops up and resumes. Failing here would make the run
        // permanently unresumable and throw away every iteration it has already
        // paid for, to punish somebody for a billing lapse. Unattended included
        // - a harness that cannot rescue a run cancels it instead.
        UnavailableReason::CreditsExhausted => Some((
            SetupBlocker::CreditsExhausted,
            format!("out of credits ({err}): top up the account, then `lev resume` this run"),
        )),
        // The provider could not be reached and there is no candidate left to
        // try. That is the network being down, not the run being wrong: the
        // request never got an answer, so nothing about this run is known to be
        // bad, and the condition is usually over in seconds and always somebody
        // else's to fix.
        //
        // Reachable only once the retry policy is spent - a transport failure is
        // transient, so the dispatch job has already tried and backed off
        // `inference_retry_attempts` times before the outcome gets here.
        //
        // "Unreachable" here covers every failure with no answer, so the
        // transport's own kind decides what the run says: a provider that was
        // never reached, one that was reached and ran out of time, and one
        // that failed part-way are three different things to go and check.
        UnavailableReason::Unreachable => {
            let blocker = blocker_for_kind(err.failure_kind());
            let what = match blocker {
                SetupBlocker::ProviderTimedOut => {
                    format!("'{provider}' did not answer in time ({err}): {REACHED_REMEDY}")
                }
                SetupBlocker::ProviderFailed => {
                    format!("'{provider}' failed while answering ({err}): {REACHED_REMEDY}")
                }
                _ => format!("could not reach '{provider}' ({err}): {UNREACHABLE_REMEDY}"),
            };
            Some((blocker, what))
        }
        // A rejected key or a model the account may not have is a real setup
        // problem, but one the failover list may still route around, and the
        // stall watchdog already parks a run whose every candidate is out of
        // service (see `fail_stalled_dispatch`). Left to the caller's error
        // path so this change adds no new parking reason.
        UnavailableReason::AuthFailed | UnavailableReason::Forbidden => None,
    }
}
