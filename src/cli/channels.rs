//! Channel management CLI commands.
//!
//! Commands for listing and installing bundled WASM channels.

use std::path::PathBuf;

use clap::Subcommand;
use tokio::fs;

use crate::channels::wasm::{bundled_channel_names, install_bundled_channel};

#[derive(Subcommand, Debug, Clone)]
pub enum ChannelsCommand {
    /// List installed channels
    List {
        /// Directory to list channels from (default: ~/.ironclaw/channels/)
        #[arg(short, long)]
        dir: Option<PathBuf>,

        /// Show detailed information
        #[arg(short, long)]
        verbose: bool,
    },

    /// Install a bundled channel
    Install {
        /// Channel name (currently supported: telegram)
        name: String,

        /// Target channels directory (default: ~/.ironclaw/channels/)
        #[arg(short, long)]
        dir: Option<PathBuf>,

        /// Overwrite existing files
        #[arg(short, long)]
        force: bool,
    },
}

/// Run a channels command.
pub async fn run_channels_command(cmd: ChannelsCommand) -> anyhow::Result<()> {
    match cmd {
        ChannelsCommand::List { dir, verbose } => list_channels(dir, verbose).await,
        ChannelsCommand::Install { name, dir, force } => install_channel(&name, dir, force).await,
    }
}

/// Default channels directory.
fn default_channels_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".ironclaw").join("channels"))
        .unwrap_or_else(|| PathBuf::from(".ironclaw/channels"))
}

async fn list_channels(dir: Option<PathBuf>, verbose: bool) -> anyhow::Result<()> {
    let channels_dir = dir.unwrap_or_else(default_channels_dir);

    if !channels_dir.exists() {
        println!("No channels directory found at {}", channels_dir.display());
        println!("Install one with: ironclaw channels install telegram");
        println!("Or configure from onboarding: ironclaw onboard --channels-only");
        return Ok(());
    }

    let mut entries = fs::read_dir(&channels_dir).await?;
    let mut channels = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
            continue;
        }

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let caps_path = path.with_extension("capabilities.json");
        let has_caps = caps_path.exists();
        let size = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
        channels.push((name, path, caps_path, has_caps, size));
    }

    if channels.is_empty() {
        println!("No channels installed in {}", channels_dir.display());
        println!("Install one with: ironclaw channels install telegram");
        println!("Or configure from onboarding: ironclaw onboard --channels-only");
        return Ok(());
    }

    channels.sort_by(|a, b| a.0.cmp(&b.0));

    println!("Installed channels in {}:", channels_dir.display());
    println!();

    for (name, wasm_path, caps_path, has_caps, size) in channels {
        if verbose {
            println!("  {}", name);
            println!("    WASM: {}", wasm_path.display());
            println!("    Size: {} bytes", size);
            println!("    Caps: {}", if has_caps { "yes" } else { "no" });
            if has_caps {
                println!("    Caps Path: {}", caps_path.display());
            }
            println!();
        } else {
            let caps_marker = if has_caps { "✓" } else { "✗" };
            println!("  {} ({} bytes, caps: {})", name, size, caps_marker);
        }
    }

    Ok(())
}

async fn install_channel(name: &str, dir: Option<PathBuf>, force: bool) -> anyhow::Result<()> {
    let channels_dir = dir.unwrap_or_else(default_channels_dir);
    let lower = name.to_ascii_lowercase();
    let supported = bundled_channel_names().join(", ");

    install_bundled_channel(&lower, &channels_dir, force)
        .await
        .map_err(|e| anyhow::anyhow!("{} (supported: {})", e, supported))?;

    println!("Installed channel '{}':", lower);
    println!("  WASM: {}", channels_dir.join(format!("{}.wasm", lower)).display());
    println!(
        "  Caps: {}",
        channels_dir
            .join(format!("{}.capabilities.json", lower))
            .display()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use tempfile::tempdir;
    use tokio::fs;

    use super::*;

    #[test]
    fn test_channels_command_parsing() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(subcommand)]
            cmd: ChannelsCommand,
        }

        TestCli::command().debug_assert();
    }

    #[tokio::test]
    async fn test_install_channel_writes_files() {
        let dir = tempdir().unwrap();

        install_channel("telegram", Some(dir.path().to_path_buf()), false)
            .await
            .unwrap();

        assert!(dir.path().join("telegram.wasm").exists());
        assert!(dir.path().join("telegram.capabilities.json").exists());
    }

    #[tokio::test]
    async fn test_install_channel_refuses_overwrite_without_force() {
        let dir = tempdir().unwrap();
        let wasm_path = dir.path().join("telegram.wasm");
        fs::write(&wasm_path, b"custom").await.unwrap();

        let result = install_channel("telegram", Some(dir.path().to_path_buf()), false).await;
        assert!(result.is_err());

        let existing = fs::read(&wasm_path).await.unwrap();
        assert_eq!(existing, b"custom");
    }
}
