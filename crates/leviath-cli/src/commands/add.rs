//! `lev add` - Install an agent package

use clap::Args;
use std::path::Path;

#[derive(Args)]
pub struct AddArgs {
    /// Path to agent directory, .leviath-bundle file, or registry package name
    #[arg(value_name = "PACKAGE")]
    pub package: String,

    /// Install from registry (URL override)
    #[arg(short, long)]
    pub registry: Option<String>,
}

pub async fn execute(args: AddArgs) -> anyhow::Result<()> {
    tracing::info!(package = %args.package, "Installing agent package");

    let installer = leviath_package::AgentInstaller::new();
    let package_path = Path::new(&args.package);

    if package_path.is_dir() {
        // Directory install: copy directory into ~/.leviath/agents/<name>/
        install_from_dir(package_path)?;
    } else if package_path.exists() || args.package.ends_with(".leviath-bundle") {
        // Bundle file installation
        if !package_path.exists() {
            anyhow::bail!("Package file not found: {}", args.package);
        }
        println!("Installing from bundle: {}", args.package);
        let installed = installer.install(package_path)?;
        println!(
            "Installed agent '{}' v{} to {}",
            installed.name,
            installed.version,
            installed.path.display()
        );
    } else {
        // Registry installation
        let config = crate::config::Config::load()?;
        let registry_url = args
            .registry
            .or_else(|| config.registries.first().cloned())
            .unwrap_or_else(|| "https://leviath.dev/registry".to_string());

        println!(
            "Searching registry {} for '{}'...",
            registry_url, args.package
        );

        let registry = leviath_package::PackageRegistry::new(registry_url);

        let info = registry.get_info(&args.package).await?;
        println!(
            "Found: {} v{} - {}",
            info.name, info.version, info.description
        );

        println!("Downloading...");
        let data = registry.download(&info.name, &info.version).await?;

        let installed = installer.install_from_bytes(&info.name, &data)?;
        println!(
            "Installed agent '{}' v{} to {}",
            installed.name,
            installed.version,
            installed.path.display()
        );
    }

    Ok(())
}

/// Copy a plain agent directory into `~/.leviath/agents/<name>/`.
///
/// The agent name is read from `agent.leviath` in the directory (falling back
/// to the directory's own name).
fn install_from_dir(src: &Path) -> anyhow::Result<()> {
    let manifest_path = src.join("agent.leviath");
    if !manifest_path.exists() {
        anyhow::bail!(
            "No agent.leviath found in '{}'. Is this an agent directory?",
            src.display()
        );
    }

    // Read the manifest to extract the agent name
    let content = std::fs::read_to_string(&manifest_path)?;
    let name = parse_agent_name(&content).unwrap_or_else(|| {
        src.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    });

    let install_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".leviath")
        .join("agents")
        .join(&name);

    if install_dir.exists() {
        println!("Reinstalling agent '{}' (replacing existing)", name);
        std::fs::remove_dir_all(&install_dir)?;
    }

    copy_dir_recursive(src, &install_dir)?;
    println!("Installed agent '{}' to {}", name, install_dir.display());
    println!("Run with:  lev run {} --task \"...\"", name);
    Ok(())
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Parse the agent name from an `agent.leviath` manifest (first `name = "..."` line).
fn parse_agent_name(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name") {
            let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
            let name = rest.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── parse_agent_name ──────────────────────────────────────────────────

    #[test]
    fn parse_agent_name_standard() {
        let content = r#"
name = "my-agent"
version = "1.0"
"#;
        assert_eq!(parse_agent_name(content), Some("my-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_no_quotes() {
        let content = r#"name = my-agent"#;
        assert_eq!(parse_agent_name(content), Some("my-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_extra_whitespace() {
        let content = r#"  name   =   "spacy-agent"  "#;
        assert_eq!(parse_agent_name(content), Some("spacy-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_missing() {
        let content = r#"
version = "1.0"
description = "test"
"#;
        assert_eq!(parse_agent_name(content), None);
    }

    #[test]
    fn parse_agent_name_empty_value() {
        let content = r#"name = """#;
        assert_eq!(parse_agent_name(content), None);
    }

    // ─── copy_dir_recursive ────────────────────────────────────────────────

    #[test]
    fn copy_dir_recursive_copies_files() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");

        std::fs::write(src_dir.path().join("file1.txt"), "hello").unwrap();
        std::fs::create_dir_all(src_dir.path().join("sub")).unwrap();
        std::fs::write(src_dir.path().join("sub/file2.txt"), "world").unwrap();

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();

        assert!(dst_path.join("file1.txt").exists());
        assert!(dst_path.join("sub/file2.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst_path.join("file1.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst_path.join("sub/file2.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn copy_dir_recursive_empty_dir() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("empty-copy");

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();
        assert!(dst_path.exists());
        assert!(dst_path.is_dir());
    }

    // ─── install_from_dir ──────────────────────────────────────────────────

    #[test]
    fn install_from_dir_no_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = install_from_dir(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("agent.leviath"));
    }

    // ─── path detection ────────────────────────────────────────────────────

    #[test]
    fn bundle_extension_detected() {
        let package = "my-agent-1.0.leviath-bundle";
        assert!(package.ends_with(".leviath-bundle"));
    }

    #[test]
    fn directory_path_detected() {
        let dir = tempfile::tempdir().unwrap();
        let package_path = Path::new(dir.path().to_str().unwrap());
        assert!(package_path.is_dir());
    }

    #[test]
    fn registry_name_not_dir_not_bundle() {
        let package = "my-cool-agent";
        let package_path = Path::new(package);
        assert!(!package_path.is_dir());
        assert!(!package.ends_with(".leviath-bundle"));
    }

    // ─── parse_agent_name additional ──────────────────────────────────────

    #[test]
    fn parse_agent_name_in_section() {
        let content = r#"
[agent]
name = "my-agent"
version = "1.0"
"#;
        assert_eq!(parse_agent_name(content), Some("my-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_with_single_quotes() {
        // toml uses double quotes, but our parser uses trim_matches('"')
        let content = r#"name = my-agent-no-quotes"#;
        assert_eq!(
            parse_agent_name(content),
            Some("my-agent-no-quotes".to_string())
        );
    }

    #[test]
    fn parse_agent_name_multiple_name_fields_returns_first() {
        let content = r#"
name = "first"
name = "second"
"#;
        assert_eq!(parse_agent_name(content), Some("first".to_string()));
    }

    // ─── copy_dir_recursive with nested dirs ──────────────────────────────

    #[test]
    fn copy_dir_recursive_deeply_nested() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("deep-copy");

        std::fs::create_dir_all(src_dir.path().join("a/b/c")).unwrap();
        std::fs::write(src_dir.path().join("a/b/c/deep.txt"), "deep").unwrap();

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();

        assert!(dst_path.join("a/b/c/deep.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst_path.join("a/b/c/deep.txt")).unwrap(),
            "deep"
        );
    }

    // ─── install_from_dir with valid manifest ─────────────────────────────

    #[test]
    fn install_from_dir_with_manifest_runs() {
        // This will try to install to ~/.leviath/agents/ which may or may not exist
        // but the important thing is it parses the manifest correctly
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[agent]
name = "test-install-agent-xyz"
version = "0.1.0"
description = "test"
"#;
        std::fs::write(dir.path().join("agent.leviath"), manifest).unwrap();
        std::fs::write(dir.path().join("readme.txt"), "hello").unwrap();

        // The install will succeed or fail depending on home dir access
        // but it should not panic
        let result = install_from_dir(dir.path());
        // Clean up if it succeeded
        if result.is_ok() {
            if let Some(home) = dirs::home_dir() {
                let install_dir = home
                    .join(".leviath")
                    .join("agents")
                    .join("test-install-agent-xyz");
                let _ = std::fs::remove_dir_all(install_dir);
            }
        }
    }
}
