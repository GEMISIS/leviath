use super::*;
use crate::region::RegionKind;

fn canon(name: &str) -> &str {
    match name {
        "bash" => "shell",
        other => other,
    }
}

fn routing() -> ToolRouting {
    ToolRouting {
        default_region: "findings".into(),
        tool_regions: [("bash".to_string(), "shell_out".to_string())].into(),
        keep_results: true,
        max_result_tokens: Some(100),
        tool_max_result_tokens: [("read_file".to_string(), 10)].into(),
    }
}

#[test]
fn a_tool_routes_to_its_own_region_through_its_alias() {
    let routing = routing();
    assert_eq!(routing.region_for("shell", canon), "shell_out");
    assert_eq!(routing.region_for("bash", canon), "shell_out");
    assert_eq!(routing.region_for("grep", canon), "findings");
    // Without the alias table, `shell` does not match a `bash` entry.
    assert_eq!(routing.region_for("shell", |n| n), "findings");
}

#[test]
fn a_tool_cap_overrides_the_stage_cap() {
    let routing = routing();
    assert_eq!(routing.result_cap("read_file", canon), Some(10));
    assert_eq!(routing.result_cap("grep", canon), Some(100));
    let uncapped = ToolRouting::default();
    assert_eq!(uncapped.result_cap("grep", canon), None);
}

#[test]
fn unkept_results_go_to_scratch_only_when_there_is_one() {
    let mut routing = routing();
    assert_eq!(routing.target("grep", canon, true), "findings");
    routing.keep_results = false;
    assert_eq!(routing.target("grep", canon, true), SCRATCH);
    assert_eq!(routing.target("grep", canon, false), "findings");
}

#[test]
fn a_capped_result_is_cut_and_marked() {
    assert_eq!(cap_result("short".into(), 10), "short");
    let long = "x".repeat(50);
    assert_eq!(
        cap_result(long, 10),
        format!("{}\n[...truncated]", "x".repeat(40))
    );
}

#[test]
fn a_result_that_fits_is_stored_whole() {
    let mut region = Region::new("findings".into(), RegionKind::Temporary, 1_000);
    let routed = route_into(&mut region, "findings", "all of it".into(), 3, false);
    assert_eq!(routed.stored, Stored::Whole);
    assert_eq!(
        routed.pointer,
        "[output (3 tokens) is in your context under the 'findings' heading - it is already in this prompt, so no tool call is needed to see it. Preview: all of it]"
    );
    assert_eq!(
        routed.pointer_tokens,
        crate::estimate_tokens(&routed.pointer)
    );
    assert_eq!(region.content[0].content, "all of it");
}

#[test]
fn a_result_too_big_for_the_room_left_is_truncated() {
    // A pinned region does not roll off, so the write has to be cut.
    let mut region = Region::new("findings".into(), RegionKind::Pinned, 200);
    let body = "y".repeat(2_000);
    let routed = route_into(&mut region, "findings", body.as_str().into(), 500, false);
    let omitted = 2_000 - (200 - 10) * 4;
    assert_eq!(routed.stored, Stored::Truncated { omitted });
    assert!(routed.pointer.starts_with(&format!(
        "[output was too large for context region 'findings': the start of it is in this prompt under that heading and {omitted} characters were dropped."
    )));
    assert!(
        routed
            .pointer
            .ends_with(&format!("Preview: {}…]", "y".repeat(160)))
    );
    assert!(
        region.content[0]
            .content
            .ends_with(&format!("... [truncated, {omitted} chars omitted]"))
    );
}

#[test]
fn a_region_with_almost_no_room_keeps_a_marker() {
    let mut region = Region::new("findings".into(), RegionKind::Pinned, 50);
    let routed = route_into(&mut region, "findings", "z".repeat(400).into(), 100, false);
    assert_eq!(routed.stored, Stored::Truncated { omitted: 400 });
    assert_eq!(
        region.content[0].content,
        "[tool result truncated - context window full]"
    );
}

#[test]
fn a_region_with_no_room_at_all_is_dropped() {
    let mut region = Region::new("findings".into(), RegionKind::Pinned, 6);
    let routed = route_into(&mut region, "findings", "w".repeat(400).into(), 100, false);
    assert_eq!(routed.stored, Stored::Dropped);
    assert!(routed.pointer.starts_with(
        "[output could NOT be stored - context region 'findings' is full and refused it"
    ));
    assert_eq!(region.content[0].content, "[result omitted]");
}

/// A refusal comes back as one, from the first write or the truncated one.
#[test]
fn a_refusal_is_reported_with_its_reason() {
    struct Refuses {
        on_attempt: usize,
        attempts: usize,
    }
    impl ResultSink for Refuses {
        fn put(&mut self, _: EntryContent, _: usize) -> crate::Result<()> {
            self.attempts += 1;
            match self.attempts == self.on_attempt {
                true => Err(Error::RegionRefusedWrite {
                    region: "r".into(),
                    reason: "no thanks".into(),
                }),
                false => Err(Error::TokenBudgetExceeded { used: 9, max: 1 }),
            }
        }
        fn room(&self) -> usize {
            500
        }
    }
    for on_attempt in [1, 2] {
        let mut sink = Refuses {
            on_attempt,
            attempts: 0,
        };
        let routed = route_into(&mut sink, "r", "body".into(), 1, false);
        assert_eq!(routed.stored, Stored::Rejected("no thanks".into()));
        assert_eq!(
            routed.pointer,
            "[output was refused by context region 'r': no thanks. Nothing was stored there; only this preview survives. Preview: body]"
        );
    }
}

#[test]
fn a_hidden_region_says_it_cannot_be_read_from_here() {
    let mut region = Region::new("later".into(), RegionKind::Temporary, 1_000);
    let routed = route_into(&mut region, "later", "kept".into(), 2, true);
    assert_eq!(
        routed.pointer,
        "[output stored in context region 'later' (2 tokens), which this stage does not carry - it is kept for a later stage and cannot be read from here. Preview: kept]"
    );
}

#[test]
fn a_region_reports_its_room() {
    let mut region = Region::new("r".into(), RegionKind::Temporary, 10);
    region.add_entry("a", 4).unwrap();
    assert_eq!(ResultSink::room(&region), 6);
    assert_eq!(route_target(CONVERSATION, true, true), CONVERSATION);
}
