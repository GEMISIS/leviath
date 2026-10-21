//! Tests for [`super`]: families, the native check, and every fate a stored
//! part can meet.

use std::collections::HashMap;

use super::*;

fn mime(raw: &str) -> MimeType {
    MimeType::parse(raw).unwrap()
}

/// A reference to `size` bytes of `raw`, keyed by `sha`.
fn blob(sha: &str, raw: &str, size: u64) -> BlobRef {
    BlobRef {
        sha256: sha.to_string(),
        mime_type: mime(raw),
        size,
        width: None,
        height: None,
        duration_ms: None,
        tokens: 0,
        stand_in: format!("<{sha}>"),
    }
}

fn slot(part: &BlobRef) -> MimeSlot<'_> {
    MimeSlot {
        part,
        deliver: None,
        remote: false,
    }
}

fn input(patterns: &[&str]) -> Vec<String> {
    patterns.iter().map(|p| p.to_string()).collect()
}

/// Run [`hydrate`] with `store` as the bytes there are, everything else at
/// its most permissive.
fn run(
    slots: &[MimeSlot<'_>],
    accepts: &[String],
    store: &HashMap<&str, Vec<u8>>,
    tune: impl FnOnce(&mut Hydration<'_>),
) -> (Vec<Outcome>, HydrationReport) {
    let registry = MimeRegistry::builtin();
    let fetch = |b: &BlobRef| {
        store
            .get(b.sha256.as_str())
            .map(|v| Arc::from(v.as_slice()))
    };
    let mut h = Hydration {
        input: accepts,
        registry: &registry,
        max_media_bytes: u64::MAX,
        fetch: &fetch,
        inline_part_bytes: &[],
        file_only: &[],
        why_inline: "",
    };
    tune(&mut h);
    hydrate(slots, &h)
}

#[test]
fn a_type_belongs_to_the_family_vendors_group_it_in() {
    assert_eq!(family_of(&mime("image/png")), Family::Image);
    assert_eq!(family_of(&mime("audio/wav")), Family::Audio);
    assert_eq!(family_of(&mime("video/mp4")), Family::Video);
    assert_eq!(family_of(&mime("application/pdf")), Family::Document);
    assert_eq!(family_of(&mime("application/zip")), Family::Other);
}

#[test]
fn a_pattern_names_a_family_unless_every_shape_carries_it() {
    assert_eq!(family_of_pattern("text/*"), None);
    assert_eq!(family_of_pattern("*/*"), None);
    assert_eq!(family_of_pattern("noslash"), None);
    assert_eq!(family_of_pattern("image/*"), Some(Family::Image));
    assert_eq!(family_of_pattern("audio/wav"), Some(Family::Audio));
    assert_eq!(family_of_pattern("video/*"), Some(Family::Video));
    assert_eq!(family_of_pattern("application/pdf"), Some(Family::Document));
    assert_eq!(family_of_pattern("model/*"), Some(Family::Other));
    // A pattern that is not even a mime type still names a family: none.
    assert_eq!(family_of_pattern("a/b/c"), Some(Family::Other));
}

#[test]
fn a_data_uri_names_the_type_and_carries_base64() {
    assert_eq!(
        data_uri(&mime("image/png"), "AAEC"),
        "data:image/png;base64,AAEC"
    );
}

#[test]
fn the_first_matching_inline_limit_wins() {
    let table: &[(&str, u64)] = &[("image/png", 1), ("image/*", 2)];
    assert_eq!(inline_limit(table, &mime("image/png")), Some(1));
    assert_eq!(inline_limit(table, &mime("image/jpeg")), Some(2));
    assert_eq!(inline_limit(table, &mime("audio/wav")), None);
}

#[test]
fn a_part_goes_natively_only_when_the_model_takes_it_and_it_did_not_opt_out() {
    let png = blob("p", "image/png", 3);
    let takes = input(&["image/*"]);
    let reg = MimeRegistry::builtin();
    assert!(sends_natively(&png, None, &takes, &reg));
    assert!(sends_natively(&png, Some(Delivery::Native), &takes, &reg));
    assert!(!sends_natively(&png, Some(Delivery::Text), &takes, &reg));
    assert!(!sends_natively(&png, Some(Delivery::StandIn), &takes, &reg));
    assert!(!sends_natively(&png, None, &input(&["text/*"]), &reg));
}

/// A text file is never sent natively, even to a model whose inputs name
/// `text/*` and even when it asks to be: Meta answered a `text/markdown`
/// `input_file` with 400 "Invalid upload request", and no request shape has a
/// native block for one. Its text is what the model reads.
#[test]
fn a_text_type_goes_as_text_to_a_model_that_takes_text() {
    let reg = MimeRegistry::builtin();
    let takes = input(&["text/*", "image/*", "application/pdf"]);
    let brief = blob("brief", "text/markdown", 5);
    let data = blob("data", "application/json", 2);
    assert!(!sends_natively(&brief, None, &takes, &reg));
    assert!(!sends_natively(
        &brief,
        Some(Delivery::Native),
        &takes,
        &reg
    ));
    assert!(!sends_natively(&data, None, &input(&["*/*"]), &reg));
    let store = HashMap::from([("brief", b"# Hi\n".to_vec()), ("data", b"{}".to_vec())]);
    let slots = [
        slot(&brief),
        MimeSlot {
            deliver: Some(Delivery::Native),
            remote: true,
            ..slot(&data)
        },
    ];
    let (outcomes, report) = run(&slots, &takes, &store, |_| {});
    assert_eq!(
        outcomes,
        [Outcome::Text("# Hi\n".into()), Outcome::Text("{}".into())]
    );
    assert_eq!(report.as_text, 2);
    assert_eq!((report.sent, report.by_file), (0, 0));
}

#[test]
fn a_taken_part_is_sent_as_base64_and_a_missing_one_as_its_stand_in() {
    let here = blob("here", "image/png", 3);
    let gone = blob("gone", "image/png", 3);
    let store = HashMap::from([("here", vec![0, 1, 2])]);
    let (outcomes, report) = run(
        &[slot(&here), slot(&gone)],
        &input(&["image/*"]),
        &store,
        |_| {},
    );
    assert_eq!(
        outcomes,
        [Outcome::Data("AAEC".into()), Outcome::Text("<gone>".into())]
    );
    assert_eq!(report.sent, 1);
    assert_eq!(report.missing, ["gone"]);
}

#[test]
fn a_part_the_vendor_holds_is_left_to_be_named_by_id() {
    let held = blob("held", "image/png", 3);
    let slots = [MimeSlot {
        remote: true,
        ..slot(&held)
    }];
    let (outcomes, report) = run(&slots, &input(&["image/*"]), &HashMap::new(), |_| {});
    assert_eq!(outcomes, [Outcome::Remote]);
    assert_eq!(report.by_file, 1);
}

#[test]
fn a_part_over_the_inline_limit_says_why_it_was_not_sent() {
    let big = blob("big", "image/png", 3 * MIB);
    let slots = [slot(&big)];
    let accepts = input(&["image/*"]);
    let limits: &[(&str, u64)] = &[("image/*", MIB)];
    let (outcomes, report) = run(&slots, &accepts, &HashMap::new(), |h| {
        h.inline_part_bytes = limits;
    });
    assert_eq!(
        outcomes,
        [Outcome::Text(
            "<big> [not sent: 3.0 MiB is over the 1.0 MiB this provider takes inline]".into()
        )]
    );
    assert_eq!(report.too_large, 1);
    let (outcomes, _) = run(&slots, &accepts, &HashMap::new(), |h| {
        h.inline_part_bytes = limits;
        h.why_inline = "zero data retention is on";
    });
    assert_eq!(
        outcomes,
        [Outcome::Text(
            "<big> [not sent: 3.0 MiB is over the 1.0 MiB this provider takes inline; \
             zero data retention is on]"
                .into()
        )]
    );
}

#[test]
fn a_file_only_type_is_never_sent_inline() {
    let clip = blob("clip", "audio/wav", 3);
    let slots = [slot(&clip)];
    let accepts = input(&["audio/*"]);
    let (outcomes, report) = run(&slots, &accepts, &HashMap::new(), |h| {
        h.file_only = &["audio/*"];
    });
    assert_eq!(
        outcomes,
        [Outcome::Text(
            "<clip> [not sent: this provider takes audio/wav only as an uploaded file, \
             because inline it is dropped unread (meta-model-cookbook#59), and it could \
             not be uploaded]"
                .into()
        )]
    );
    assert_eq!(report.not_uploaded, 1);
    let (outcomes, _) = run(&slots, &accepts, &HashMap::new(), |h| {
        h.file_only = &["audio/*"];
        h.why_inline = "uploads are off";
    });
    assert!(matches!(&outcomes[0], Outcome::Text(t) if t.ends_with(", and uploads are off]")));
}

#[test]
fn the_byte_cap_sheds_the_oldest_media_first() {
    let old = blob("old", "image/png", 3);
    let new = blob("new", "image/png", 3);
    let store = HashMap::from([("old", vec![0, 1, 2]), ("new", vec![0, 1, 2])]);
    let (outcomes, report) = run(
        &[slot(&old), slot(&new)],
        &input(&["image/*"]),
        &store,
        |h| h.max_media_bytes = 4,
    );
    assert_eq!(
        outcomes,
        [Outcome::Text("<old>".into()), Outcome::Data("AAEC".into())]
    );
    assert_eq!(report.capped, 1);
    // Shed parts are counted as stand-ins too, as they always were.
    assert_eq!(report.stand_ins, 1);
}

#[test]
fn a_text_type_is_sent_as_its_text_whatever_the_model_takes() {
    let note = blob("note", "text/plain", 2);
    let gone = blob("gone", "text/plain", 2);
    let store = HashMap::from([("note", b"hi".to_vec())]);
    let (outcomes, report) = run(
        &[slot(&note), slot(&gone)],
        &input(&["image/*"]),
        &store,
        |_| {},
    );
    assert_eq!(
        outcomes,
        [Outcome::Text("hi".into()), Outcome::Text("<gone>".into())]
    );
    assert_eq!(report.as_text, 1);
    assert_eq!(report.missing, ["gone"]);
}

#[test]
fn a_part_that_asks_for_text_and_is_not_utf8_falls_back_to_its_stand_in() {
    let png = blob("png", "image/png", 2);
    let store = HashMap::from([("png", vec![0xff, 0xfe])]);
    let slots = [MimeSlot {
        deliver: Some(Delivery::Text),
        ..slot(&png)
    }];
    let (outcomes, report) = run(&slots, &input(&["text/*"]), &store, |_| {});
    assert_eq!(outcomes, [Outcome::Text("<png>".into())]);
    assert_eq!(report.stand_ins, 1);
}

#[test]
fn a_part_the_model_does_not_take_or_that_asks_for_its_stand_in_is_one() {
    let png = blob("png", "image/png", 2);
    let note = blob("note", "text/plain", 2);
    let store = HashMap::from([("note", b"hi".to_vec())]);
    let slots = [
        slot(&png),
        MimeSlot {
            deliver: Some(Delivery::StandIn),
            ..slot(&note)
        },
    ];
    let (outcomes, report) = run(&slots, &input(&["text/*"]), &store, |_| {});
    assert_eq!(
        outcomes,
        [
            Outcome::Text("<png>".into()),
            Outcome::Text("<note>".into())
        ]
    );
    assert_eq!(report.stand_ins, 2);
}
