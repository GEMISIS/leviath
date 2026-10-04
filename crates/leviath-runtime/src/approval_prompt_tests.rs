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

/// A prompt task that dies before it reports still settles its call: the
/// batch is refused that call, saying why, instead of waiting for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_that_panics_refuses_its_call_instead_of_hanging() {
    let silent = crate::test_support::SilentPanics::install();
    let mut world = World::new();
    install(&mut world, Handle::current(), Arc::new(Notify::new()));
    let call = leviath_providers::ToolCall {
        id: "c0".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({}),
        thought_signature: None,
    };
    let e = world
        .spawn((
            PendingBatch::new(
                vec![call],
                Vec::new(),
                std::collections::HashMap::new(),
                Vec::new(),
                Vec::new(),
            ),
            AwaitingApproval {
                call_id: "c0".to_string(),
                question: "run-approve-1".to_string(),
                keys: vec!["k".to_string()],
                charge: 3,
            },
            ToolGrants::default(),
            WriteLedger { written: 1 },
        ))
        .id();
    supervise(
        world.resource::<ApprovalStage>(),
        e,
        "c0".to_string(),
        Box::pin(async { panic!("the prompt blew up") }),
    );
    let mut schedule = Schedule::default();
    schedule.add_systems(collect_approvals);
    for _ in 0..500 {
        schedule.run(&mut world);
        if world.get::<AwaitingApproval>(e).is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    drop(silent);
    assert!(
        world.get::<AwaitingApproval>(e).is_none(),
        "the batch is no longer waiting on the prompt"
    );
    let decided = world
        .get::<PendingBatch>(e)
        .unwrap()
        .decision("c0")
        .cloned();
    let Some(Decision::Refuse(text)) = decided else {
        panic!("refused, got {decided:?}");
    };
    assert!(text.contains("the prompt blew up"), "{text}");
    assert!(text.contains("did not run"), "{text}");
    assert!(!world.get::<ToolGrants>(e).unwrap().granted("k"));
    assert_eq!(world.get::<WriteLedger>(e).unwrap().written, 1);
}
