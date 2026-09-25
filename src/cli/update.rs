use crate::updater::{InstallOutcome, UpdateExecutionStrategy, UpdateInfo, Updater, parse_highlights};
use anyhow::{Context, Result, anyhow};
use crossterm::style::Stylize;
use std::env;
use std::process::Command;
use vtcode_config::update::{ReleaseChannel, UpdateConfig};
use vtcode_core::ui::user_confirmation::UserConfirmation;

/// Options for the update command
#[derive(Debug, Clone)]
pub struct UpdateCommandOptions {
    /// Check for updates without installing
    pub check_only: bool,
    /// Force reinstall latest version
    pub force: bool,
    /// List available versions
    pub list: bool,
    /// Number of versions to list
    pub limit: usize,
    /// Pin to a specific version
    pub pin: Option<String>,
    /// Unpin version
    pub unpin: bool,
    /// Set release channel
    pub channel: Option<String>,
    /// Show update configuration
    pub show_config: bool,
}

/// Handle the update command
pub async fn handle_update_command(options: UpdateCommandOptions) -> Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");

    // Handle configuration operations first (don't need updater)
    if options.show_config {
        return handle_show_config();
    }

    if options.unpin {
        return handle_unpin();
    }

    if let Some(channel) = options.channel {
        return handle_set_channel(channel);
    }

    if options.list {
        return handle_list_versions(options.limit).await;
    }

    if let Some(version_str) = options.pin {
        return handle_pin_version(version_str);
    }

    println!("{} VT Code v{}", "Checking for updates...".cyan(), current_version);

    let mut updater = Updater::new(current_version).context("Failed to initialize updater")?;

    // Show pin status
    if let Some(pinned) = updater.pinned_version() {
        println!("{} Version pinned to {} (use --unpin to remove)", "ℹ".blue(), pinned.to_string().cyan());
    }

    match updater.check_for_updates().await {
        Ok(Some(update_info)) => handle_update_available(&updater, &update_info, options).await,
        Ok(None) => {
            println!("{} You're already on the latest version (v{})", "✓".green(), current_version);
            println!("{} vtcode init   # scaffold project if needed", "→".cyan());

            if options.force {
                println!("{} Force mode enabled, attempting reinstall...", "→".cyan());
                install_update(&updater, None, true).await?;
            }

            Ok(())
        }
        Err(err) => {
            tracing::error!("Failed to check for updates: {:#}", err);
            Err(err)
        }
    }
}

/// Show current update configuration
fn handle_show_config() -> Result<()> {
    let config = UpdateConfig::load().unwrap_or_default();

    println!("{}", "Update Configuration".bold());
    println!("{} Config file: {}", "→".cyan(), UpdateConfig::config_path()?.display());
    println!();
    println!("{} Channel: {}", "→".cyan(), config.channel.to_string().green());
    println!(
        "{} Pinned: {}",
        "→".cyan(),
        if let Some(pin) = config.pinned_version() {
            let reason = config
                .pin
                .as_ref()
                .and_then(|p| p.reason.as_deref())
                .unwrap_or("no reason specified");
            format!("{} ({})", pin.to_string().cyan(), reason)
        } else {
            "No".to_string()
        }
    );
    println!("{} Check interval: {} hours", "→".cyan(), config.check_interval_hours);
    println!("{} Download timeout: {} seconds", "→".cyan(), config.download_timeout_secs);

    Ok(())
}

/// Unpin version
fn handle_unpin() -> Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");
    let mut updater = Updater::new(current_version)?;

    if !updater.is_pinned() {
        println!("{} Version is not pinned", "ℹ".blue());
        return Ok(());
    }

    let pinned = updater
        .pinned_version()
        .ok_or_else(|| {
            anyhow!("is_pinned() returned true but pinned_version() returned None — updater state is inconsistent")
        })?
        .clone();
    updater.unpin_version()?;

    println!("{} Unpinned from v{}", "✓".green(), pinned);
    println!(
        "{} You will now receive updates from the {} channel",
        "→".cyan(),
        updater.config().channel.to_string().green()
    );

    Ok(())
}

/// Set release channel
fn handle_set_channel(channel_str: String) -> Result<()> {
    let channel = match channel_str.to_lowercase().as_str() {
        "stable" => ReleaseChannel::Stable,
        "beta" => ReleaseChannel::Beta,
        "nightly" => ReleaseChannel::Nightly,
        _ => {
            anyhow::bail!("Invalid channel '{channel_str}'. Must be one of: stable, beta, nightly");
        }
    };

    let mut config = UpdateConfig::load().unwrap_or_default();
    config.channel = channel.clone();
    config.save()?;

    println!("{} Release channel set to {}", "✓".green(), channel.to_string().green());
    println!("{} Future updates will follow the {} channel", "→".cyan(), channel.to_string().green());

    Ok(())
}

/// List available versions
async fn handle_list_versions(limit: usize) -> Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");
    let updater = Updater::new(current_version)?;

    println!("{} Fetching available versions...", "→".cyan());

    let versions = updater.list_versions(limit).await?;

    println!();
    println!("{}", "Available Versions".bold());
    println!("{}", "─".repeat(60));

    for (i, version_info) in versions.iter().enumerate() {
        let is_current = version_info.version == *updater.current_version();
        let marker = if is_current {
            "●".green()
        } else if version_info.is_prerelease {
            "○".yellow()
        } else {
            "○".white()
        };

        let prerelease_tag = if version_info.is_prerelease {
            " (pre-release)".yellow().to_string()
        } else {
            "".to_string()
        };

        println!(
            "{} {:<12} {}{}",
            marker,
            format!("v{}", version_info.version),
            version_info.tag.clone().dim(),
            prerelease_tag
        );

        if let Some(published) = &version_info.published_at {
            println!("   {}", format!("Published: {published}").dim());
        }

        if i < versions.len() - 1 {
            println!();
        }
    }

    println!("{}", "─".repeat(60));
    println!("{} Current version: v{}", "→".cyan(), current_version.green());
    println!("{} Use --pin VERSION to pin to a specific version", "→".cyan());

    Ok(())
}

/// Pin to a specific version
fn handle_pin_version(version_str: String) -> Result<()> {
    let version =
        semver::Version::parse(&version_str).with_context(|| format!("Invalid version format: {version_str}"))?;

    let current_version = env!("CARGO_PKG_VERSION");
    let mut updater = Updater::new(current_version)?;

    updater.pin_version(version.clone(), None, false)?;

    println!("{} Pinned to v{}", "✓".green(), version);
    println!("{} Auto-updates disabled until unpinned", "→".cyan());
    println!("{} Use --unpin to remove version pin", "→".cyan());

    Ok(())
}

/// Handle when an update is available
async fn handle_update_available(updater: &Updater, update: &UpdateInfo, options: UpdateCommandOptions) -> Result<()> {
    println!("\n{} New version available: v{}", "●".cyan(), update.version);

    println!("\n{}", "Release notes:".bold());
    println!("{}", update.release_notes);

    if options.check_only {
        let guidance = updater.update_guidance();
        println!("\n{} Update command: {}", "→".cyan(), guidance.command().green());
        println!("{} Release page: {}", "→".cyan(), Updater::release_url(&update.version).cyan());
        return Ok(());
    }

    let proceed = UserConfirmation::confirm_action(
        &format!("Install VT Code v{} now? This will replace the current binary.", update.version),
        true,
    )
    .context("Failed to read confirmation input")?;

    if !proceed {
        println!("{} Update canceled.", "✗".yellow());
        println!("{} You can run this again with `vtcode update`.", "→".cyan());
        return Ok(());
    }

    install_update(updater, Some(update), options.force).await
}

fn run_update_command(command: &str, strategy: UpdateExecutionStrategy) -> Result<std::process::ExitStatus> {
    let mut cmd = match strategy {
        UpdateExecutionStrategy::Shell => {
            #[cfg(target_os = "windows")]
            {
                let mut c = Command::new("cmd");
                c.arg("/C").arg(command);
                c
            }
            #[cfg(not(target_os = "windows"))]
            {
                let mut c = Command::new("/bin/sh");
                c.arg("-lc").arg(command);
                c
            }
        }
        UpdateExecutionStrategy::PowerShell => {
            let mut c = if cfg!(target_os = "windows") {
                Command::new("powershell")
            } else {
                Command::new("pwsh")
            };
            c.arg("-NoLogo").arg("-NoProfile").arg("-Command").arg(command);
            c
        }
    };

    cmd.status().with_context(|| format!("failed to run update command: {command}"))
}

async fn print_installed_release_notes(version: &semver::Version, release_notes: &str) {
    let parsed = parse_highlights(version, release_notes);
    println!("\n{}", format!("What's new in v{version}:").bold());
    if parsed.items.is_empty() {
        println!("{}", release_notes.trim());
    } else {
        for item in &parsed.items {
            println!(" • {item}");
        }
    }
    println!("{} {}", "→".cyan(), Updater::release_url(version).cyan());
}

async fn install_update(updater: &Updater, update_info: Option<&UpdateInfo>, force: bool) -> Result<()> {
    let guidance = updater.update_guidance();
    if guidance.source.is_managed() {
        println!("{} Managed install detected ({}).", "!".yellow(), guidance.source.label());
        println!("{} Running: {}", "→".cyan(), guidance.command().green());

        // `run_update_command` shells out and blocks until the package
        // manager exits; run it off the async executor so it cannot stall
        // Tokio workers. See `# Blocking` docs in `src/agent/runloop/git.rs`.
        let display_command = guidance.action.display_command.to_string();
        let execution = guidance.action.execution;
        let status = tokio::task::spawn_blocking(move || run_update_command(&display_command, execution))
            .await
            .context("update command task panicked")?
            .with_context(|| format!("failed to run update command for {}", guidance.source.label()))?;

        if status.success() {
            println!("{} Update command completed successfully!", "✓".green());
            // The managed installer replaced the binary outside of
            // `install_update_reported`, so seed the cache here. Otherwise the
            // next `vtcode` launch reads a stale `latest_version` and only
            // shows release notes on its second start (N+1).
            if let Some(info) = update_info {
                crate::updater::seed_release_notes_for_installed_version(&info.version, &info.release_notes);
                print_installed_release_notes(&info.version, &info.release_notes).await;
            }
            println!("\n{} Restart VT Code to use the new version", "→".cyan());
            println!("{} vtcode init   # scaffold project if needed", "→".cyan());
            println!("{} vtcode        # launch TUI", "→".cyan());
        } else {
            let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string());
            println!("{} Update command exited with status: {}", "!".yellow(), code);
            println!("{} You may need to run the command manually.", "→".cyan());
        }

        return Ok(());
    }

    println!("\n{} Installing update...", "→".cyan());

    match updater.install_update(force).await? {
        InstallOutcome::Updated(version) => {
            println!("{} {} installed successfully!", "✓".green(), version.bold());
            // `install_update_reported` already seeded the cache with the fresh
            // release metadata so the TUI shows notes on its first launch.
            // Echo the highlights here as well so they are visible right after
            // `vtcode update` succeeds, without waiting for a restart.
            if let Some(info) = update_info {
                print_installed_release_notes(&info.version, &info.release_notes).await;
            }
            println!("\n{} Restart VT Code to use the new version", "→".cyan());
            println!("{} vtcode init   # scaffold project if needed", "→".cyan());
            println!("{} vtcode        # launch TUI", "→".cyan());
        }
        InstallOutcome::UpToDate(version) => {
            println!("{} Already up to date ({})", "✓".green(), version.bold());
        }
    }

    Ok(())
}
