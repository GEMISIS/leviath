use std::path::Path;

use super::*;

/// Every manifest this crate's tests ship, each read key by key.
fn shipped() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = vec![(
        "coder".to_string(),
        include_str!("fixtures/coder.leviath").to_string(),
    )];
    let fixtures = root.join("tests/fixtures");
    for dir in [fixtures.clone(), fixtures.join("agents")] {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            for file in ["blueprint.leviath", "agent.leviath"] {
                if let Ok(text) = std::fs::read_to_string(entry.path().join(file)) {
                    out.push((entry.path().display().to_string(), text));
                }
            }
        }
    }
    out
}

#[test]
fn every_key_a_shipped_manifest_holds_is_read() {
    let all = shipped();
    assert!(all.len() > 5, "found {}", all.len());
    for (name, text) in all {
        let doc: Table = toml::from_str(&text).unwrap();
        assert_eq!(unread_keys(&doc), Vec::<String>::new(), "{name}");
    }
}

/// One stranger in every table the walk looks at, each reported where it is.
#[test]
fn a_stranger_is_found_in_every_table_that_ignores_one() {
    let manifest = r#"
odd = 1
nudge = { max = 3 }

[agent]
name = "t"
odd = 1
output = { odd = 1, artifacts = [{ name = "a", type = "text/plain", odd = 1 }] }
nudge = { odd = 1 }

[context]
odd = 1
file_tracking = { odd = 1 }
regions = { log = { kind = "pinned", odd = 1 } }

[compaction]
odd = 1

[security]
odd = 1

[read_paths]
odd = 1

[safe_commands]
odd = 1

[repetition_detection]
odd = 1

[[transforms]]
odd = 1
mappings = [{ odd = 1 }]

[[dependencies]]
odd = 1
install = { odd = 1, server = { odd = 1 } }

[stages.main]
model = { odd = 1 }
output = { odd = 1 }
nudge = { odd = 1 }
security = { odd = 1 }
context = { regions = { notes = { odd = 1 } } }
interaction_points = [{ odd = 1 }]
"#;
    let found = unread_keys(&toml::from_str(manifest).unwrap());
    let places: Vec<&str> = found
        .iter()
        .map(|line| line.split(": `").next().unwrap())
        .collect();
    assert_eq!(
        places,
        [
            "the manifest",
            "the manifest",
            "[agent]",
            "[agent.output]",
            "[agent.output] artifact 0",
            "[agent.nudge]",
            "[context]",
            "region 'log'",
            "[context.file_tracking]",
            "[compaction]",
            "[security]",
            "[read_paths]",
            "[safe_commands]",
            "[repetition_detection]",
            "[[transforms]] 0",
            "[[transforms]] 0 mapping 0",
            "[[dependencies]] 0",
            "[[dependencies]] 0 install",
            "[[dependencies]] 0 install.server",
            "[stages.main.model]",
            "[stages.main.output]",
            "[stages.main.nudge]",
            "[stages.main.security]",
            "stage 'main' region 'notes'",
            "[stages.main] interaction point 0",
        ]
    );
    assert!(found[1].ends_with("(an agent's nudge settings are [agent.nudge])"));
    assert!(!found[0].contains("[agent.nudge]"), "{}", found[0]);
}
