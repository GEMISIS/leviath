use super::RunFailureKind;

#[test]
fn every_kind_has_its_own_kebab_case_label() {
    let labels: std::collections::BTreeSet<&str> =
        RunFailureKind::ALL.iter().map(|k| k.label()).collect();
    assert_eq!(labels.len(), RunFailureKind::ALL.len());
    for label in labels {
        assert!(
            label.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
            "{label}"
        );
    }
}

#[test]
fn a_label_reads_back_as_its_kind_and_an_unknown_one_as_none() {
    for kind in RunFailureKind::ALL {
        assert_eq!(RunFailureKind::from_label(kind.label()), Some(kind));
        assert_eq!(kind.to_string(), kind.label());
    }
    assert_eq!(RunFailureKind::from_label("not-a-kind"), None);
    assert_eq!(RunFailureKind::from_label(""), None);
}

#[test]
fn serde_writes_the_label() {
    for kind in RunFailureKind::ALL {
        let written = serde_json::to_string(&kind).unwrap();
        assert_eq!(written, format!("\"{}\"", kind.label()));
        let read: RunFailureKind = serde_json::from_str(&written).unwrap();
        assert_eq!(read, kind);
    }
}
