//! The bundled `web_search` / `web_fetch` scripts must compile.
//!
//! They ship inside four agents and are only ever exercised by a live run, so a
//! syntax slip in one of them is invisible until an agent's search silently
//! stops working - which is exactly the failure this pair was rewritten to make
//! impossible.

use std::path::Path;

/// Every bundled copy of the two web tools parses its annotations and compiles.
#[test]
fn the_bundled_web_tools_compile() {
    let agents = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .join("leviath-cli")
        .join("agents");

    let mut checked = 0;
    for agent in [
        "researcher",
        "deep-researcher",
        "wide-researcher",
        "data-analyst",
    ] {
        for tool in ["web_search.rhai", "web_fetch.rhai"] {
            let path = agents.join(agent).join("tools").join(tool);
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            leviath_scripting::tool::check_source(&format!("{agent}/{tool}"), &src)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            checked += 1;
        }
    }
    assert_eq!(checked, 8, "four agents ship two web tools each");
}
