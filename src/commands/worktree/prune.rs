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
    config_cache: &mut std::collections::HashMap<
        String,
        Option<Vec<meta_core::config::ProjectInfo>>,
    >,
) -> Option<String> {
    let config = config_cache
        .entry(entry.project.clone())
        .or_insert_with(|| {
            let project_path = Path::new(&entry.project);
            meta_core::config::find_meta_config_in(project_path).and_then(|(meta_path, _)| {
                meta_core::config::parse_meta_config(&meta_path)
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

pub(crate) fn handle_prune(
    args: PruneArgs,
    _verbose: bool,
    json: bool,
    strict: bool,
) -> Result<()> {
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
    let mut config_cache: std::collections::HashMap<
        String,
        Option<Vec<meta_core::config::ProjectInfo>>,
    > = std::collections::HashMap::new();

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
                println!("  {} ({}) — {}", entry.name, entry.reason, entry.path);
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
                super::warn_or_bail(
                    strict,
                    format!("Failed to remove directory: {}", wt_path.display()),
                )?;
                continue;
            }
        }

        removed.push(prune_entry.clone());
    }

    // Batch-remove all pruned entries from store in a single lock cycle
    let keys_to_remove: Vec<String> = removed.iter().map(|e| e.path.clone()).collect();
    super::warn_store_error(store_remove_batch(&keys_to_remove), strict)?;

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
        println!("{} Pruned {} worktree(s):", "✓".green(), removed.len());
        for entry in &removed {
            println!("  {} ({}) — {}", entry.name, entry.reason, entry.path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn make_store_entry(name: &str, project: &str, repos: Vec<(&str, &str)>) -> WorktreeStoreEntry {
        WorktreeStoreEntry {
            name: name.to_string(),
            project: project.to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            ephemeral: false,
            ttl_seconds: None,
            repos: repos
                .into_iter()
                .map(|(alias, branch)| StoreRepoEntry {
                    alias: alias.to_string(),
                    branch: branch.to_string(),
                    created_branch: false,
                })
                .collect(),
            custom: HashMap::new(),
        }
    }

    // ── create_prune_entry ──────────────────────────────

    #[test]
    fn create_prune_entry_with_age() {
        let entry = create_prune_entry(
            "test-wt".to_string(),
            "/path/to/wt".to_string(),
            "ttl_expired",
            Some(3600),
        );
        assert_eq!(entry.name, "test-wt");
        assert_eq!(entry.path, "/path/to/wt");
        assert_eq!(entry.reason, "ttl_expired");
        assert_eq!(entry.age_seconds, Some(3600));
    }

    #[test]
    fn create_prune_entry_without_age() {
        let entry = create_prune_entry(
            "test-wt".to_string(),
            "/path/to/wt".to_string(),
            "orphaned (missing directory)",
            None,
        );
        assert_eq!(entry.name, "test-wt");
        assert_eq!(entry.reason, "orphaned (missing directory)");
        assert_eq!(entry.age_seconds, None);
    }

    #[test]
    fn create_prune_entry_converts_into_string() {
        let entry = create_prune_entry(
            "test-wt".to_string(),
            "/path/to/wt".to_string(),
            String::from("test reason"),
            None,
        );
        assert_eq!(entry.reason, "test reason");
    }

    // ── check_repo_orphaned ─────────────────────────────

    #[test]
    fn check_repo_orphaned_returns_none_when_all_repos_exist() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create .meta config
        let meta_config = r#"{"projects":{"repo1":"git@github.com:org/repo1.git","repo2":"git@github.com:org/repo2.git"}}"#;
        std::fs::write(project_path.join(".meta"), meta_config).unwrap();

        let entry = make_store_entry(
            "test-wt",
            project_path.to_str().unwrap(),
            vec![("repo1", "main"), ("repo2", "feat-x")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry, &mut cache);

        assert!(result.is_none());
    }

    #[test]
    fn check_repo_orphaned_detects_when_all_repos_missing() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create .meta config with only repo3 (repos 1 and 2 are missing)
        let meta_config = r#"{"projects":{"repo3":"git@github.com:org/repo3.git"}}"#;
        std::fs::write(project_path.join(".meta"), meta_config).unwrap();

        let entry = make_store_entry(
            "test-wt",
            project_path.to_str().unwrap(),
            vec![("repo1", "main"), ("repo2", "feat-x")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry, &mut cache);

        assert_eq!(
            result,
            Some("orphaned (all source repos removed from project)".to_string())
        );
    }

    #[test]
    fn check_repo_orphaned_returns_none_when_some_repos_missing() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create .meta config with repo1 but not repo2
        let meta_config = r#"{"projects":{"repo1":"git@github.com:org/repo1.git"}}"#;
        std::fs::write(project_path.join(".meta"), meta_config).unwrap();

        let entry = make_store_entry(
            "test-wt",
            project_path.to_str().unwrap(),
            vec![("repo1", "main"), ("repo2", "feat-x")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry, &mut cache);

        // Only partial orphan - should return None (worktree is still partially valid)
        assert!(result.is_none());
    }

    #[test]
    fn check_repo_orphaned_returns_none_when_no_config() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // No .meta config file
        let entry = make_store_entry(
            "test-wt",
            project_path.to_str().unwrap(),
            vec![("repo1", "main")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry, &mut cache);

        // Can't determine without config
        assert!(result.is_none());
    }

    #[test]
    fn check_repo_orphaned_uses_cache() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create .meta config
        let meta_config = r#"{"projects":{"repo1":"git@github.com:org/repo1.git"}}"#;
        std::fs::write(project_path.join(".meta"), meta_config).unwrap();

        let entry1 = make_store_entry(
            "test-wt-1",
            project_path.to_str().unwrap(),
            vec![("repo1", "main")],
        );

        let entry2 = make_store_entry(
            "test-wt-2",
            project_path.to_str().unwrap(),
            vec![("repo1", "feat-x")],
        );

        let mut cache = HashMap::new();

        // First call should populate cache
        let result1 = check_repo_orphaned(&entry1, &mut cache);
        assert!(result1.is_none());
        assert_eq!(cache.len(), 1);

        // Second call with same project should use cache
        let result2 = check_repo_orphaned(&entry2, &mut cache);
        assert!(result2.is_none());
        assert_eq!(cache.len(), 1); // Still 1 entry (cached)

        // Verify cache contains the correct project
        assert!(cache.contains_key(project_path.to_str().unwrap()));
    }

    #[test]
    fn check_repo_orphaned_handles_invalid_json() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create invalid .meta config
        std::fs::write(project_path.join(".meta"), "invalid json").unwrap();

        let entry = make_store_entry(
            "test-wt",
            project_path.to_str().unwrap(),
            vec![("repo1", "main")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry, &mut cache);

        // Should return None (can't parse config)
        assert!(result.is_none());
    }
}
