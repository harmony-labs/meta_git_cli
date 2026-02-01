use crate::helpers::get_all_repo_directories;
use chrono::Utc;
use console::style;
use dialoguer::Confirm;
use meta_git_lib::snapshot::{self, RepoState, Snapshot};
use meta_plugin_protocol::CommandResult;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;

/// Show snapshot help text
pub(crate) fn execute_snapshot_help() -> anyhow::Result<CommandResult> {
    println!(
        r#"{}

{}

Usage: meta git snapshot <command> [args]

Commands:
  {}      Create a snapshot of all repos' git state
  {}        List all available snapshots
  {}        Show details of a snapshot
  {}     Restore all repos to a snapshot state
  {}      Delete a snapshot

Examples:
  meta git snapshot create before-upgrade
  meta git snapshot list
  meta git snapshot show before-upgrade
  meta git snapshot restore before-upgrade --dry-run
  meta git snapshot restore before-upgrade --force
  meta git snapshot delete before-upgrade

Snapshots capture the entire workspace state (recursive by default).
Use --force to skip confirmation on restore, --dry-run to preview."#,
        style("meta git snapshot - Workspace State Management").bold(),
        style("[EXPERIMENTAL] File format is subject to change.").yellow(),
        style("create <name>").cyan(),
        style("list").cyan(),
        style("show <name>").cyan(),
        style("restore <name>").cyan(),
        style("delete <name>").cyan(),
    );
    Ok(CommandResult::Message(String::new()))
}

/// Create a snapshot of the current workspace state
pub(crate) fn execute_snapshot_create(
    args: &[String],
    projects: &[String],
    cwd: &Path,
) -> anyhow::Result<CommandResult> {
    // Parse snapshot name from args
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("Usage: meta git snapshot create <name>"))?;

    // Get all repos (recursive by default)
    let dirs = get_all_repo_directories(projects, cwd)?;

    println!(
        "Creating snapshot '{}' of {} repos...",
        style(name).cyan(),
        dirs.len()
    );

    // Capture repo states in parallel
    let results: Vec<_> = dirs
        .par_iter()
        .map(|dir| {
            let path = if dir == "." {
                cwd.to_path_buf()
            } else {
                cwd.join(dir)
            };

            if !path.exists() || !snapshot::is_git_repo(&path) {
                return (dir.clone(), None);
            }

            let state = snapshot::capture_repo_state(&path);
            (dir.clone(), Some(state))
        })
        .collect();

    // Process results sequentially for display
    let mut repos = HashMap::new();
    let mut dirty_count = 0;

    for (dir, result) in &results {
        match result {
            None => {
                println!(
                    "  {} {} (not a git repo, skipping)",
                    style("⚠").yellow(),
                    dir
                );
            }
            Some(Ok(state)) => {
                if state.dirty {
                    dirty_count += 1;
                    println!("  {} {} (dirty)", style("○").yellow(), dir);
                } else {
                    println!("  {} {}", style("✓").green(), dir);
                }
                repos.insert(dir.clone(), state.clone());
            }
            Some(Err(e)) => {
                println!("  {} {} (error: {})", style("✗").red(), dir, e);
            }
        }
    }

    if repos.is_empty() {
        anyhow::bail!("No repos captured");
    }

    let snap = Snapshot {
        name: name.clone(),
        created: Utc::now(),
        repos,
    };

    snapshot::save_snapshot(cwd, &snap)?;

    println!();
    println!(
        "{} Captured state of {} repos",
        style("✓").green(),
        snap.repos.len()
    );
    if dirty_count > 0 {
        println!(
            "{} {} repo(s) have uncommitted changes (recorded as dirty)",
            style("⚠").yellow(),
            dirty_count
        );
    }
    println!(
        "Snapshot saved: {}",
        style(format!(".meta-snapshots/{name}.json")).dim()
    );

    Ok(CommandResult::Message(String::new()))
}

/// List all snapshots
pub(crate) fn execute_snapshot_list(cwd: &Path) -> anyhow::Result<CommandResult> {
    let snapshots = snapshot::list_snapshots(cwd)?;

    if snapshots.is_empty() {
        println!("No snapshots found.");
        println!(
            "Create one with: {}",
            style("meta git snapshot create <name>").cyan()
        );
        return Ok(CommandResult::Message(String::new()));
    }

    println!("Snapshots:\n");
    for info in snapshots {
        let dirty_note = if info.dirty_count > 0 {
            format!(" ({} dirty)", info.dirty_count)
        } else {
            String::new()
        };
        println!(
            "  {} - {} repos{} - {}",
            style(&info.name).cyan().bold(),
            info.repo_count,
            style(dirty_note).yellow(),
            style(info.created.format("%Y-%m-%d %H:%M:%S")).dim()
        );
    }
    println!();

    Ok(CommandResult::Message(String::new()))
}

/// Show details of a snapshot
pub(crate) fn execute_snapshot_show(args: &[String], cwd: &Path) -> anyhow::Result<CommandResult> {
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("Usage: meta git snapshot show <name>"))?;

    let snap = snapshot::load_snapshot(cwd, name)?;

    println!("Snapshot: {}", style(&snap.name).cyan().bold());
    println!("Created:  {}", snap.created.format("%Y-%m-%d %H:%M:%S UTC"));
    println!("Repos:    {}", snap.repos.len());
    println!();

    // Sort repos by name for consistent output
    let mut repos: Vec<_> = snap.repos.iter().collect();
    repos.sort_by(|a, b| a.0.cmp(b.0));

    for (name, state) in repos {
        let branch_info = state
            .branch
            .as_ref()
            .map(|b| format!(" -> {b}"))
            .unwrap_or_else(|| " (detached)".to_string());

        let dirty_marker = if state.dirty {
            format!(" {}", style("(dirty)").yellow())
        } else {
            String::new()
        };

        println!(
            "  {} {}{}{}",
            style(&state.sha[..8]).dim(),
            name,
            style(branch_info).cyan(),
            dirty_marker
        );
    }

    Ok(CommandResult::Message(String::new()))
}

/// Restore workspace to a snapshot state
pub(crate) fn execute_snapshot_restore(
    args: &[String],
    _projects: &[String],
    dry_run: bool,
    cwd: &Path,
) -> anyhow::Result<CommandResult> {
    // Parse args
    let mut name: Option<&str> = None;
    let mut force = false;
    let mut dry_run = dry_run;

    for arg in args {
        match arg.as_str() {
            "--force" | "-f" => force = true,
            "--dry-run" => dry_run = true,
            s if !s.starts_with('-') => name = Some(s),
            _ => {}
        }
    }

    let name = name.ok_or_else(|| {
        anyhow::anyhow!("Usage: meta git snapshot restore <name> [--force] [--dry-run]")
    })?;

    let snap = snapshot::load_snapshot(cwd, name)?;

    // Analyze what would change
    let mut repos_to_restore: Vec<(&str, &RepoState, bool)> = Vec::new();
    let mut missing_repos = Vec::new();

    for (repo_name, state) in &snap.repos {
        let path = if repo_name == "." {
            cwd.to_path_buf()
        } else {
            cwd.join(repo_name)
        };

        if !path.exists() || !snapshot::is_git_repo(&path) {
            missing_repos.push(repo_name.as_str());
            continue;
        }

        // Check if current state is dirty
        let current_state = snapshot::capture_repo_state(&path)?;
        repos_to_restore.push((repo_name, state, current_state.dirty));
    }

    let dirty_count = repos_to_restore.iter().filter(|(_, _, d)| *d).count();

    // Show preview
    println!(
        "Restore {} repos to snapshot '{}':",
        repos_to_restore.len(),
        style(name).cyan()
    );
    println!(
        "  - {} repos will checkout to their recorded SHA",
        repos_to_restore.len() - dirty_count
    );
    if dirty_count > 0 {
        println!(
            "  - {} repos have uncommitted changes (will be stashed)",
            style(dirty_count).yellow()
        );
    }
    if !missing_repos.is_empty() {
        println!(
            "  - {} repos missing (will be skipped): {}",
            style(missing_repos.len()).red(),
            missing_repos.join(", ")
        );
    }
    println!();

    if dry_run {
        println!("{} Dry run - no changes made", style("[DRY RUN]").cyan());
        return Ok(CommandResult::Message(String::new()));
    }

    // Confirm unless --force
    if !force {
        let proceed = Confirm::new()
            .with_prompt("Proceed?")
            .default(false)
            .interact()?;

        if !proceed {
            println!("Aborted.");
            return Ok(CommandResult::Message(String::new()));
        }
    }

    // Execute restore
    println!("Restoring...");
    let mut success_count = 0;
    let mut fail_count = 0;

    for (repo_name, state, _is_dirty) in &repos_to_restore {
        let path = if *repo_name == "." {
            cwd.to_path_buf()
        } else {
            cwd.join(repo_name)
        };

        let result = snapshot::restore_repo_state(&path, state, force)?;

        if result.success {
            let stash_note = if result.stashed {
                format!(" {}", style("(stashed changes)").yellow())
            } else {
                String::new()
            };
            println!(
                "  {} {} {}{}",
                style("✓").green(),
                repo_name,
                result.message,
                stash_note
            );
            success_count += 1;
        } else {
            println!("  {} {} {}", style("✗").red(), repo_name, result.message);
            fail_count += 1;
        }
    }

    println!();
    if fail_count > 0 {
        println!(
            "Restored {} repo(s), {} failed",
            style(success_count).green(),
            style(fail_count).red()
        );
    } else {
        println!("{} Restored {} repo(s)", style("✓").green(), success_count);
    }

    Ok(CommandResult::Message(String::new()))
}

/// Delete a snapshot
pub(crate) fn execute_snapshot_delete(
    args: &[String],
    cwd: &Path,
) -> anyhow::Result<CommandResult> {
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("Usage: meta git snapshot delete <name>"))?;

    snapshot::delete_snapshot(cwd, name)?;

    println!(
        "{} Deleted snapshot '{}'",
        style("✓").green(),
        style(name).cyan()
    );

    Ok(CommandResult::Message(String::new()))
}
