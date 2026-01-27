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

    for (path_key, entry) in &store.worktrees {
        let wt_path = Path::new(path_key);

        // Check if path exists (orphaned detection)
        if !wt_path.exists() {
            to_remove.push(PruneEntry {
                name: entry.name.clone(),
                path: path_key.clone(),
                reason: "orphaned".to_string(),
                age_seconds: None,
            });
            continue;
        }

        // Check TTL expiration
        if let Some(remaining) = entry_ttl_remaining(entry, now) {
            if remaining <= 0 {
                // Total age = configured TTL + seconds past expiry
                let overdue = (-remaining) as u64;
                let age = entry.ttl_seconds.unwrap() + overdue;
                to_remove.push(PruneEntry {
                    name: entry.name.clone(),
                    path: path_key.clone(),
                    reason: "ttl_expired".to_string(),
                    age_seconds: Some(age),
                });
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
