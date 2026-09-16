//! The scrubber, shape by shape.

use super::scrub::*;
use leviath_core::run_archive::{
    MessageRecord, RunIdentity, RunRecord, read_archive, write_archive_start, write_record,
};
use leviath_core::run_meta::RunMeta;

fn scrubber(known: &[&str]) -> Scrubber {
    Scrubber::new(known.iter().map(|s| s.to_string()))
}

#[test]
fn a_known_value_is_replaced_everywhere_and_counted() {
    let s = scrubber(&["planted-secret-value-1"]);
    let (out, count) = s.scrub("a planted-secret-value-1 b planted-secret-value-1");
    assert_eq!(out, "a [REDACTED] b [REDACTED]");
    assert_eq!(count, 2);
}

#[test]
fn a_short_value_is_not_searched_for() {
    // "task" would erase every ordinary word that happens to match it.
    let s = scrubber(&["task"]);
    let (out, count) = s.scrub("the task text");
    assert_eq!(out, "the task text");
    assert_eq!(count, 0);
}

#[test]
fn two_values_of_one_length_are_both_known() {
    let s = scrubber(&["secret-value-b", "secret-value-a", "secret-value-a"]);
    let (out, count) = s.scrub("secret-value-a secret-value-b");
    assert_eq!(out, "[REDACTED] [REDACTED]");
    assert_eq!(count, 2);
}

#[test]
fn a_longer_value_that_contains_a_shorter_one_goes_whole() {
    let s = scrubber(&["secretpart1", "secretpart1-and-more"]);
    let (out, count) = s.scrub("x secretpart1-and-more y");
    assert_eq!(out, "x [REDACTED] y");
    assert_eq!(count, 1);
}

#[test]
fn every_token_shape_is_recognised() {
    let s = scrubber(&[]);
    let cases = [
        (
            "key sk-ant-api03-abcdefghijklmnopqrstuvwxyz end",
            "[REDACTED:api-key]",
        ),
        ("aws AKIAIOSFODNN7EXAMPLE end", "[REDACTED:aws-key]"),
        (
            "gh ghp_abcdefghijklmnopqrstuvwxyz0123456789 end",
            "[REDACTED:github-token]",
        ),
        (
            "gh github_pat_abcdefghijklmnopqrstuvwxyz0123 end",
            "[REDACTED:github-token]",
        ),
        (
            "slack xoxb-123456789012-abcdefghij end",
            "[REDACTED:slack-token]",
        ),
        (
            "google AIzaSyA1234567890abcdefghijklmnopqrstuvw end",
            "[REDACTED:google-key]",
        ),
        (
            "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJwbGFudGVkIn0.signaturepart123 end",
            "[REDACTED:jwt]",
        ),
        ("?api_key=abcdef123456&x=1", "[REDACTED:query]"),
    ];
    for (input, label) in cases {
        let (out, count) = s.scrub(input);
        assert!(out.contains(label), "{input:?} -> {out:?}");
        assert!(count >= 1, "{input:?}");
    }
}

#[test]
fn a_bearer_header_keeps_its_prefix() {
    let s = scrubber(&[]);
    let (out, count) = s.scrub("Authorization: Bearer abcdefghijklmnop.qrstuvwxyz");
    // The header pattern runs after the bearer one and takes the whole value.
    assert!(out.starts_with("Authorization: "), "{out}");
    assert!(!out.contains("abcdefghijklmnop"), "{out}");
    assert!(count >= 1);
    let (out, _) = s.scrub("token: bearer 0123456789abcdefghij");
    assert_eq!(out, "token: bearer [REDACTED:bearer]");
}

#[test]
fn a_private_key_block_goes_whole() {
    let s = scrubber(&[]);
    let text = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIE\nline2\n-----END RSA PRIVATE KEY-----\nafter";
    let (out, count) = s.scrub(text);
    assert_eq!(out, "before\n[REDACTED:private-key]\nafter");
    assert_eq!(count, 1);
}

#[test]
fn an_env_assignment_is_scrubbed_by_its_name() {
    let s = scrubber(&[]);
    let (out, count) =
        s.scrub("export ANTHROPIC_API_KEY=abc\nPATH=/usr/bin\n\"OPENAI_API_KEY\": \"xyz\"");
    assert!(out.contains("ANTHROPIC_API_KEY=[REDACTED:env]"), "{out}");
    assert!(out.contains("PATH=/usr/bin"), "{out}");
    assert!(
        out.contains("\"OPENAI_API_KEY\": \"[REDACTED:env]"),
        "{out}"
    );
    assert_eq!(count, 2);
}

#[test]
fn scrub_json_walks_strings_keys_and_the_callback_secret() {
    let s = scrubber(&["planted-secret-value-1"]);
    let mut value = serde_json::json!({
        "callback_secret": "hmac-signing-key",
        "callback_url": "https://example.test/hook",
        "api_key": "a-long-key-value-under-a-secret-key",
        "key": "task",
        "nested": [{"content": "planted-secret-value-1 inside"}, 7, null],
        "count": 3,
    });
    let count = s.scrub_json(&mut value);
    assert_eq!(value["callback_secret"], serde_json::Value::Null);
    assert_eq!(value["callback_url"], "https://example.test/hook");
    assert_eq!(value["api_key"], REDACTED);
    assert_eq!(
        value["key"], "task",
        "a short label under a secret-shaped key stays"
    );
    assert_eq!(value["nested"][0]["content"], "[REDACTED] inside");
    assert_eq!(value["count"], 3);
    assert_eq!(count, 3);
    // Already blank: nothing to count.
    let mut blank = serde_json::json!({"callback_secret": null});
    assert_eq!(s.scrub_json(&mut blank), 0);
}

#[test]
fn scrub_toml_takes_keys_headers_env_and_extras_and_keeps_the_layout() {
    let s = scrubber(&[]);
    let text = r#"# my config
default_provider = "anthropic"
credential_store = "keychain"
allowed = ["a", "b"]

[providers]
anthropic_api_key = "sk-ant-short"
anthropic_headers = { "x-planted" = "value-one", other = 3 }

[[mcp_servers]]
name = "s"
command = "echo"
[mcp_servers.env]
PLANTED = "env-value"

[model_providers.gw]
kind = "openai-compatible"
base_url = "http://localhost:9"
api_token = "extra-token"
[model_providers.gw.headers]
authorization = "Bearer abc"
"#;
    let (out, count) = s.scrub_toml(text);
    assert!(out.starts_with("# my config\n"), "comments survive: {out}");
    assert!(out.contains("default_provider = \"anthropic\""), "{out}");
    assert!(
        out.contains("credential_store = \"keychain\""),
        "a mode, not a secret: {out}"
    );
    assert!(out.contains("anthropic_api_key = \"<redacted>\""), "{out}");
    assert!(
        out.contains("\"x-planted\" = \"<redacted>\""),
        "header names stay, values go: {out}"
    );
    assert!(out.contains("other = 3"), "{out}");
    assert!(out.contains("PLANTED = \"<redacted>\""), "{out}");
    assert!(out.contains("base_url = \"http://localhost:9\""), "{out}");
    assert!(out.contains("api_token = \"<redacted>\""), "{out}");
    assert!(out.contains("authorization = \"<redacted>\""), "{out}");
    assert!(!out.contains("value-one"), "{out}");
    assert!(!out.contains("env-value"), "{out}");
    assert!(!out.contains("extra-token"), "{out}");
    assert!(!out.contains("Bearer abc"), "{out}");
    assert_eq!(count, 5);
}

#[test]
fn a_file_that_is_not_toml_gets_the_textual_pass_alone() {
    let s = scrubber(&["planted-secret-value-1"]);
    let (out, count) = s.scrub_toml("this is not = = toml planted-secret-value-1");
    assert_eq!(out, "this is not = = toml [REDACTED]");
    assert_eq!(count, 1);
}

#[test]
fn every_toml_shape_is_walked() {
    // An empty item, an array of strings under a secret key, an array of
    // plain values, a nested inline table marked secret by its parent.
    assert_eq!(scrub_item("k", &mut toml_edit::Item::None, false), 0);
    let mut arr = toml_edit::Value::from_iter(["a-b-c-d-e-f-g-h", "x"]);
    assert_eq!(scrub_value("api_key", &mut arr, false), 2);
    let mut nums = toml_edit::Value::from_iter([1, 2]);
    assert_eq!(scrub_value("api_key", &mut nums, false), 0);
    let mut inline = toml_edit::Value::from_iter([("inner", toml_edit::Value::from("v"))]);
    assert_eq!(scrub_value("env", &mut inline, false), 1);
    let mut plain = toml_edit::Value::from_iter([("inner", toml_edit::Value::from("v"))]);
    assert_eq!(scrub_value("plain", &mut plain, false), 0);
}

fn archive_with(records: &[RunRecord]) -> Vec<u8> {
    let mut out = Vec::new();
    write_archive_start(&mut out, 1).unwrap();
    for record in records {
        write_record(&mut out, record).unwrap();
    }
    out
}

fn header(secret: &str) -> RunRecord {
    let mut meta = RunMeta::new(
        "run-1".to_string(),
        "coder".to_string(),
        "/tmp/coder".to_string(),
        "do the thing".to_string(),
        None,
        "/tmp".to_string(),
        1,
    );
    meta.callback_secret = Some(secret.to_string());
    RunRecord::Header {
        identity: RunIdentity {
            run_id: "run-1".to_string(),
            machine_id: "m".to_string(),
            world_id: "w".to_string(),
            created_at: 0,
        },
        meta: Box::new(meta),
    }
}

#[test]
fn a_run_archive_is_re_encoded_with_its_secrets_out() {
    let s = scrubber(&["planted-secret-value-1"]);
    let records = [
        header("planted-callback-secret"),
        RunRecord::Message {
            message: MessageRecord {
                role: "user".to_string(),
                content: "planted-secret-value-1 and AKIAIOSFODNN7EXAMPLE".to_string(),
            },
            at: 1,
        },
    ];
    let scrubbed = s.scrub_run_archive(&archive_with(&records)).unwrap();
    assert_eq!(scrubbed.skipped, 0);
    assert_eq!(scrubbed.redactions, 3);
    let (version, back) = read_archive(&mut scrubbed.bytes.as_slice()).unwrap();
    assert_eq!(version, 1);
    assert_eq!(back.len(), 2);
    match &back[0] {
        RunRecord::Header { meta, .. } => {
            assert!(meta.callback_secret.is_none());
            assert_eq!(meta.task, "do the thing");
        }
        other => panic!("{other:?}"),
    }
    match &back[1] {
        RunRecord::Message { message, .. } => {
            assert_eq!(message.content, "[REDACTED] and [REDACTED:aws-key]");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_frame_this_build_cannot_read_is_dropped_and_counted() {
    let s = scrubber(&[]);
    let mut bytes = archive_with(&[header("x")]);
    let bogus = br#"{"NoSuchRecord":{"at":1}}"#;
    bytes.extend_from_slice(&(bogus.len() as u64).to_be_bytes());
    bytes.extend_from_slice(bogus);
    let scrubbed = s.scrub_run_archive(&bytes).unwrap();
    assert_eq!(scrubbed.skipped, 1);
    let (_, back) = read_archive(&mut scrubbed.bytes.as_slice()).unwrap();
    assert_eq!(back.len(), 1);
}

#[test]
fn a_torn_or_foreign_archive_is_an_error() {
    let s = scrubber(&[]);
    assert!(s.scrub_run_archive(b"not an archive").is_err());
    let mut torn = archive_with(&[]);
    torn.extend_from_slice(&64u64.to_be_bytes());
    torn.extend_from_slice(b"short");
    assert!(s.scrub_run_archive(&torn).is_err());
}

#[test]
fn secret_strings_are_read_out_of_a_json_document() {
    let value = serde_json::json!({
        "servers": {"s": {"access_token": "tok-1", "resource": "https://x", "nested": [{"client_secret": "tok-2"}]}},
        "keychain_providers": ["names only"],
        "count": 1,
    });
    let mut out = Vec::new();
    secret_strings_in(&value, &mut out);
    out.sort();
    assert_eq!(out, vec!["tok-1", "tok-2"]);
}

#[test]
fn config_secrets_covers_every_field_that_holds_one() {
    let toml = r#"
agent_paths = []
openrouter_api_key = "or-key-value-1"
[providers]
anthropic_api_key = "ant-key-value-1"
openai_api_key = "oai-key-value-1"
google_api_key = "goo-key-value-1"
meshy_api_key = "mesh-key-value-1"
bedrock_api_key = "bed-key-value-1"
anthropic_headers = { a = "hdr-a" }
openai_headers = { b = "hdr-b" }
google_headers = { c = "hdr-c" }
openrouter_headers = { d = "hdr-d" }
meshy_headers = { e = "hdr-e" }
bedrock_headers = { f = "hdr-f" }
[[mcp_servers]]
name = "s"
command = "echo"
env = { T = "mcp-env-1" }
headers = { h = "mcp-hdr-1" }
[model_providers.gw]
kind = "openai-compatible"
base_url = "http://localhost:9"
api_key = "gw-key-1"
headers = { g = "gw-hdr-1" }
api_token = "gw-extra-1"
region = "not-a-secret"
"#;
    let config: crate::config::Config = toml::from_str(toml).unwrap();
    let mut out = config_secrets(&config);
    out.sort();
    let expected = [
        "ant-key-value-1",
        "bed-key-value-1",
        "goo-key-value-1",
        "gw-extra-1",
        "gw-hdr-1",
        "gw-key-1",
        "hdr-a",
        "hdr-b",
        "hdr-c",
        "hdr-d",
        "hdr-e",
        "hdr-f",
        "mcp-env-1",
        "mcp-hdr-1",
        "mesh-key-value-1",
        "oai-key-value-1",
        "or-key-value-1",
    ];
    assert_eq!(out, expected);
}

#[test]
fn env_secrets_and_present_names_follow_the_naming_rule() {
    let names = vec![
        "ANTHROPIC_API_KEY".to_string(),
        "PATH".to_string(),
        "UNSET_TOKEN".to_string(),
    ];
    let lookup = |name: &str| match name {
        "ANTHROPIC_API_KEY" => Some("the-key".to_string()),
        "PATH" => Some("/bin".to_string()),
        _ => None,
    };
    assert_eq!(env_secrets(&names, &lookup), vec!["the-key"]);
    assert_eq!(
        present_names(&names, &lookup),
        vec!["ANTHROPIC_API_KEY", "PATH"]
    );
}
