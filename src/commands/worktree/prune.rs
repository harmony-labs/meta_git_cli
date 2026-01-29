use anyhow::Result;
use chrono::Utc;
use colored::*;
use std::path::Path;

use meta_cli::worktree::discover_worktree_repos;
use meta_git_lib::worktree::git_ops::remove_worktree_repos;
use meta_git_lib::worktree::helpers::find_meta_dir;
use meta_git_lib::worktree::hooks::fire_post_prune;
use meta_git_lib::worktree::store::{entry_ttl_remaining, store_list, store_remove_batch};
use meta_git_lib::worktree::types::*;

use super::cli_types::PruneArgs;

/// Helper to create a PruneEntry with consistent structure.
fn create_prune_entry(
    name: String,
    path: String,
    reason: impl Into<String>,
    age_seconds: Option<u64>,
) -> PruneEntry {
    PruneEntry {
        name,
        path,
        reason: reason.into(),
        age_seconds,
    }
}

/// Check if a worktree entry is orphaned due to missing source repos.
/// Uses a config cache to avoid redundant file I/O when multiple worktrees
/// share the same source project.
fn check_repo_orphaned(
    entry: &WorktreeStoreEntry,
    config_cache: &mut std::collections::HashMap<String, Option<Vec<meta_cli::config::ProjectInfo>>>,
) -> Option<String> {
    let config = config_cache.entry(entry.project.clone()).or_insert_with(|| {
        let project_path = Path::new(&entry.project);
        meta_cli::config::find_meta_config_in(project_path).and_then(|(meta_path, _)| {
            meta_cli::config::parse_meta_config(&meta_path)
                .ok()
                .map(|(projects, _)| projects)
        })
    });

    let Some(projects) = config else {
        return None; // Can't check without config
    };

    let missing_count = entry
        .repos
        .iter()
        .filter(|store_repo| !projects.iter().any(|p| p.name == store_repo.alias))
        .count();

    if missing_count > 0 && missing_count == entry.repos.len() {
        Some("orphaned (all source repos removed from project)".to_string())
    } else {
        None
    }
}

pub(crate) fn handle_prune(args: PruneArgs, _verbose: bool, json: bool) -> Result<()> {
    let dry_run = args.dry_run;

    let store: WorktreeStoreData = store_list()?;
    if store.worktrees.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&PruneOutput {
                    removed: vec![],
                    dry_run,
                })?
            );
        } else {
            println!("No worktrees in store. Nothing to prune.");
        }
        return Ok(());
    }

    let now = Utc::now().timestamp();
    let mut to_remove: Vec<PruneEntry> = Vec::new();
    let mut config_cache: std::collections::HashMap<String, Option<Vec<meta_cli::config::ProjectInfo>>> =
        std::collections::HashMap::new();

    for (path_key, entry) in &store.worktrees {
        let wt_path = Path::new(path_key);

        // Check if path exists (orphaned detection)
        if !wt_path.exists() {
            to_remove.push(create_prune_entry(
                entry.name.clone(),
                path_key.clone(),
                "orphaned (missing directory)",
                None,
            ));
            continue;
        }

        // Check if source project directory still exists
        let project_path = Path::new(&entry.project);
        if !project_path.exists() {
            to_remove.push(create_prune_entry(
                entry.name.clone(),
                path_key.clone(),
                "orphaned (source project missing)",
                None,
            ));
            continue;
        }

        // Check if source repos still exist in project (with config caching)
        if let Some(reason) = check_repo_orphaned(entry, &mut config_cache) {
            to_remove.push(create_prune_entry(
                entry.name.clone(),
                path_key.clone(),
                reason,
                None,
            ));
            continue;
        }

        // Check TTL expiration
        if let Some(remaining) = entry_ttl_remaining(entry, now) {
            if remaining <= 0 {
                // Total age = configured TTL + seconds past expiry
                let overdue = (-remaining) as u64;
                let age = entry.ttl_seconds.unwrap() + overdue;
                to_remove.push(create_prune_entry(
                    entry.name.clone(),
                    path_key.clone(),
                    "ttl_expired",
                    Some(age),
                ));
            }
        }
    }

    if to_remove.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&PruneOutput {
                    removed: vec![],
                    dry_run,
                })?
            );
        } else {
            println!("Nothing to prune.");
        }
        return Ok(());
    }

    if dry_run {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&PruneOutput {
                    removed: to_remove,
                    dry_run: true,
                })?
            );
        } else {
            println!("Would prune {} worktree(s):", to_remove.len());
            for entry in &to_remove {
                println!(
                    "  {} ({}) — {}",
                    entry.name, entry.reason, entry.path
                );
            }
        }
        return Ok(());
    }

    // Actually remove: physical cleanup first, then batch store update.
    let mut removed = Vec::new();
    for prune_entry in &to_remove {
        let wt_path = Path::new(&prune_entry.path);

        if wt_path.exists() {
            // Try to properly remove via git worktree remove
            let repos = discover_worktree_repos(wt_path).unwrap_or_default();
            let _ = remove_worktree_repos(&repos, true, false);

            // Clean up directory
            let _ = std::fs::remove_dir_all(wt_path);

            // Only record as removed if directory is actually gone
            if wt_path.exists() {
                eprintln!(
                    "{} Failed to remove directory: {}",
                    "warning:".yellow().bold(),
                    wt_path.display()
                );
                continue;
            }
        }

        removed.push(prune_entry.clone());
    }

    // Batch-remove all pruned entries from store in a single lock cycle
    let keys_to_remove: Vec<String> = removed.iter().map(|e| e.path.clone()).collect();
    super::warn_store_error(store_remove_batch(&keys_to_remove));

    // Fire post-prune hook
    let meta_dir = find_meta_dir();
    fire_post_prune(&removed, meta_dir.as_deref());

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&PruneOutput {
                removed,
                dry_run: false,
            })?
        );
    } else {
        println!(
            "{} Pruned {} worktree(s):",
            "✓".green(),
            removed.len()
        );
        for entry in &removed {
            println!(
                "  {} ({}) — {}",
                entry.name, entry.reason, entry.path
            );
        }
    }

    Ok(())
}
