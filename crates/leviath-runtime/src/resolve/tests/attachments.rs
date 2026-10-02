use leviath_core::mime::Delivery;

use super::*;
use crate::spec::graph::RegionKind;
use crate::spec::request::{Attachment, Bytes};
use crate::state::context::PartBody;

fn attach(name: &str, region: Option<&str>) -> Attachment {
    Attachment {
        name: name.into(),
        mime_type: None,
        region: region.map(n),
        deliver: None,
        caption: None,
        data: Bytes(b"\x89PNG\r\n\x1a\nbytes".to_vec()),
    }
}

fn file_input(name: &str, accepts: &[&str], region: &str) -> InputDecl {
    InputDecl {
        ty: InputType::File {
            accepts: accepts.iter().map(|a| n(a)).collect(),
        },
        ..text_input(name, region)
    }
}

#[tokio::test]
async fn an_attachment_with_no_region_goes_where_the_task_goes() {
    let mut request = raw(graph());
    let mut shot = attach("shot.png", None);
    shot.caption = Some("the login page".into());
    shot.deliver = Some(Delivery::Native);
    request.attachments.push(shot);
    let resolved = spawn(&request, &Fake::default()).await.unwrap();
    let task = &resolved.spec.seeded["task"];
    assert_eq!(task.text, "do the thing\n\nthe login page");
    assert_eq!(task.parts.len(), 1);
    let part = &task.parts[0];
    assert_eq!(part.mime_type, "image/png");
    assert_eq!(part.name.as_deref(), Some("shot.png"));
    assert_eq!(part.deliver, Some(Delivery::Native));
    let PartBody::Stored(blob) = &part.body else {
        panic!("an attachment is stored")
    };
    assert_eq!(blob.size, 13);
    assert!(blob.stand_in.contains("shot.png"), "{}", blob.stand_in);
    assert_eq!(resolved.blobs[&blob.digest], b"\x89PNG\r\n\x1a\nbytes");
}

#[tokio::test]
async fn the_task_region_is_the_first_pinned_one_when_none_is_named_task() {
    let mut g = graph();
    g.layout.regions[1].name = n("brief");
    g.inputs[0] = text_input("task", "brief");
    g.layout.regions[0].kind = RegionKind::Temporary;
    let mut request = raw(g);
    request.attachments.push(attach("a.txt", None));
    let resolved = spawn(&request, &Fake::default()).await.unwrap();
    assert_eq!(resolved.spec.seeded["brief"].parts.len(), 1);
}

#[tokio::test]
async fn an_attachment_goes_to_the_region_it_names() {
    let mut request = raw(graph());
    request.attachments.push(attach("sys.txt", Some("system")));
    let resolved = spawn(&request, &Fake::default()).await.unwrap();
    assert_eq!(resolved.spec.seeded["system"].parts.len(), 1);
    assert_eq!(resolved.spec.seeded["system"].text, "");
}

#[tokio::test]
async fn every_way_an_attachment_can_be_wrong_is_reported() {
    let mut g = graph();
    g.layout.regions[0].accepts = vec![n("text/*")];
    let mut request = raw(g);
    let mut declared = attach("pic", Some("system"));
    declared.mime_type = Some(n("image/png"));
    request.attachments = vec![
        attach("scan.bad", None),
        attach("scan.weird", None),
        attach("x.txt", Some("nowhere")),
        declared,
    ];
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(
        found(&issues),
        [
            "attachments[0].mime_type Invalid",
            "attachments[1].mime_type Invalid",
            "attachments[2].region Dangling",
            "attachments[3].region WrongType",
        ]
    );
    assert_eq!(issues.0[0].got.as_deref(), Some("no declared type"));
    assert_eq!(issues.0[2].known, ["system", "task"]);
    assert_eq!(issues.0[3].known, ["text/*"]);
    assert_eq!(
        issues.0[3].got.as_deref(),
        Some("\"pic\", a file of type image/png")
    );
}

#[tokio::test]
async fn an_attachment_with_nowhere_to_go_is_refused() {
    let mut g = graph();
    for region in &mut g.layout.regions {
        region.kind = RegionKind::Temporary;
    }
    let mut request = raw(g);
    request.attachments.push(attach("a.txt", None));
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(found(&issues), ["attachments[0].region Missing"]);
}

#[tokio::test]
async fn a_file_input_puts_its_attachment_in_its_region() {
    let mut g = graph();
    let mut mockup = file_input("mockup", &["image/*"], "system");
    mockup.required = true;
    g.inputs.push(mockup);
    let mut request = raw(g).input("mockup", RawInput::Text("m.png".into()));
    let mut m = attach("m.png", Some("task"));
    m.caption = Some("the new design".into());
    request.attachments.push(m);
    let resolved = spawn(&request, &Fake::default()).await.unwrap();
    let system = &resolved.spec.seeded["system"];
    assert_eq!(system.parts.len(), 1);
    assert_eq!(system.text, "m.png\n\nthe new design");
    assert!(
        resolved.spec.seeded["task"].parts.is_empty(),
        "a file input's attachment goes where the input says"
    );
}

#[tokio::test]
async fn a_file_of_the_wrong_type_is_refused_by_its_input_and_its_region() {
    let mut g = graph();
    g.inputs
        .push(file_input("diagram", &["image/svg+xml"], "system"));
    g.inputs.push(file_input("notes", &[], "system"));
    g.layout.regions[0].accepts = vec![n("image/*")];
    let request = raw(g)
        .input("diagram", RawInput::Text("d.png".into()))
        .input("notes", RawInput::Text("n.txt".into()));
    let mut request = request;
    request.attachments = vec![attach("d.png", None), attach("n.txt", None)];
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(
        found(&issues),
        ["inputs.diagram WrongType", "inputs.notes WrongType"]
    );
    assert_eq!(
        issues.0[0].got.as_deref(),
        Some("\"d.png\", a file of type image/png")
    );
}

/// Another input's problem does not hide a file of the wrong type: each
/// input that reads is still checked against its file.
#[tokio::test]
async fn a_file_of_the_wrong_type_is_refused_beside_other_input_problems() {
    let mut g = graph();
    g.inputs
        .push(file_input("diagram", &["image/svg+xml"], "system"));
    let mut request = raw(g)
        .input("diagram", RawInput::Text("d.png".into()))
        .input("colour", RawInput::Text("blue".into()));
    request.attachments = vec![attach("d.png", None)];
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(
        found(&issues),
        ["inputs.colour Unknown", "inputs.diagram WrongType"]
    );
}

#[tokio::test]
async fn a_file_input_naming_a_refused_attachment_adds_no_second_issue() {
    let mut g = graph();
    g.inputs.push(file_input("doc", &["text/*"], "system"));
    let mut request = raw(g).input("doc", RawInput::Text("doc.bad".into()));
    request.attachments.push(attach("doc.bad", None));
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(found(&issues), ["attachments[0].mime_type Invalid"]);
}
