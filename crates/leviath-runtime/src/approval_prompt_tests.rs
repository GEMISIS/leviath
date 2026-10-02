//! The words an approval's answer becomes.

use super::*;

/// A plain decline is the sentence it has always been, and a timeout is named
/// only when one is configured.
#[test]
fn the_results_say_what_happened() {
    assert_eq!(
        declined_result("shell", None),
        "[denied] User declined tool call 'shell'."
    );
    let timed = unanswered_approval_result("shell", Some(30));
    assert!(timed.contains("(30 s"), "{timed}");
    assert!(timed.contains("interaction_timeout_secs"), "{timed}");
    let closed = unanswered_approval_result("shell", None);
    assert!(closed.contains("closed without an answer"), "{closed}");
    assert!(!closed.contains("timeout"), "{closed}");
}
