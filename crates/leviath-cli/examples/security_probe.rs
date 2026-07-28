//! Live probe: drive the *real* `DaemonScriptHost` against the *real* shipped
//! `.rhai` tool scripts, with no model in the loop.
//!
//! Unit tests exercise these guards against a fake `ScriptIo`. This runs the
//! production host with the production I/O backend — real DNS, a real HTTP
//! client, the real environment — which is the only way to know the checks fire
//! on the path a user's agent actually takes.
//!
//! Run with:  cargo run -p leviath-cli --example security_probe

use std::collections::BTreeMap;

use leviath_cli::daemon::script_host::{DaemonScriptHost, ScriptAllow};
use leviath_scripting::ScriptHost;

fn all_allowed() -> ScriptAllow {
    // Deliberately the most permissive Layer-3 grant. Everything refused below
    // is refused by a *different* control, not by the permission bits.
    ScriptAllow {
        http_get: true,
        http_post: true,
        shell: true,
        read_file: true,
        write_file: true,
        env_var: true,
    }
}

fn check(label: &str, result: Result<String, String>, want_denied: bool) -> bool {
    let denied = result
        .as_ref()
        .err()
        .is_some_and(|e| e.contains("[denied]"));
    let ok = denied == want_denied;
    let verdict = if ok { "PASS" } else { "FAIL" };
    match &result {
        Ok(v) => println!("  [{verdict}] {label}\n           -> allowed: {v:.80}"),
        Err(e) => println!("  [{verdict}] {label}\n           -> {e:.140}"),
    }
    ok
}

fn main() {
    let workdir = std::env::temp_dir();
    let host = DaemonScriptHost::new(all_allowed(), workdir.clone());
    let mut all_pass = true;

    println!("\n=== SSRF guard (every host function permission is ON) ===");
    for url in [
        "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
        "http://metadata.google.internal/computeMetadata/v1/",
        "http://127.0.0.1:3000/api/agents",
        "http://localhost:3000/api/agents",
        "http://192.168.1.1/",
        "http://10.0.0.1/",
        "file:///etc/passwd",
    ] {
        all_pass &= check(url, host.http_get(url, BTreeMap::new()), true);
    }

    println!("\n=== credential-shaped env reads ===");
    for name in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "GITHUB_TOKEN",
        "LEVIATH_API_TOKEN",
    ] {
        all_pass &= check(name, host.env_var(name), true);
    }

    println!("\n=== ordinary env reads still work ===");
    all_pass &= check("PATH", host.env_var("PATH"), false);

    println!("\n=== the opt-out re-opens the local network ===");
    let permissive = DaemonScriptHost::new(all_allowed(), workdir.clone()).with_local_network(true);
    // Nothing is listening, so a *connection* error (not a `[denied]`) is the
    // proof that the policy let it through to the network layer.
    all_pass &= check(
        "127.0.0.1:9 with allow_local_network",
        permissive.http_get("http://127.0.0.1:9/", BTreeMap::new()),
        false,
    );

    println!("\n=== allowlisted env var is readable ===");
    let allowed = DaemonScriptHost::new(all_allowed(), workdir.clone())
        .with_env_allowlist(vec!["MY_PROVIDER_KEY".to_string()]);
    // `temp_env`, not `std::env::set_var`: the workspace forbids `unsafe`, and
    // the setter is unsafe from edition 2024 on. (The lint caught this.)
    let (a, b) = temp_env::with_var("MY_PROVIDER_KEY", Some("value-here"), || {
        (
            allowed.env_var("MY_PROVIDER_KEY"),
            allowed.env_var("ANTHROPIC_API_KEY"),
        )
    });
    all_pass &= check("MY_PROVIDER_KEY", a, false);
    all_pass &= check("ANTHROPIC_API_KEY (not on the allowlist)", b, true);

    println!("\n=== workdir containment (symlink escape) ===");
    let dir = std::env::temp_dir().join("lev-probe-workdir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp workdir");
    #[cfg(unix)]
    std::os::unix::fs::symlink("/", dir.join("link")).expect("symlink");
    let confined = DaemonScriptHost::new(all_allowed(), dir.clone());
    let escape = confined.read_file("link/etc/hosts");
    let blocked = escape.as_ref().err().is_some_and(|e| e.contains("symlink"));
    println!(
        "  [{}] link/etc/hosts\n           -> {:.140}",
        if blocked { "PASS" } else { "FAIL" },
        escape
            .as_ref()
            .err()
            .map(String::as_str)
            .unwrap_or("ALLOWED")
    );
    all_pass &= blocked;
    let _ = std::fs::remove_dir_all(&dir);

    println!(
        "\n{}",
        if all_pass {
            "ALL PROBES PASSED"
        } else {
            "SOME PROBES FAILED"
        }
    );
    std::process::exit(if all_pass { 0 } else { 1 });
}
