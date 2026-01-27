use anyhow::Result;
use colored::*;

use meta_cli::worktree::discover_worktree_repos;
use meta_git_lib::worktree::git_ops::*;
use meta_git_lib::worktree::helpers::*;
use meta_git_lib::worktree::hooks::fire_post_destroy;
use meta_git_lib::worktree::store::store_remove;
use meta_git_lib::worktree::types::*;

use super::cli_types::DestroyArgs;

pub(crate) fn handle_destroy(args: DestroyArgs, verbose: bool, json: bool) -> Result<()> {
    let name = &args.name;
    validate_worktree_name(name)?;
    let force = args.force;

    let meta_dir = find_meta_dir();
    let worktree_root = resolve_worktree_root(meta_dir.as_deref())?;
    let wt_dir = worktree_root.join(name);

    if !wt_dir.exists() {
        anyhow::bail!("Worktree '{}' not found at {}", name, wt_dir.display());
    }

    let repos = discover_worktree_repos(&wt_dir)?;

    // Check for dirty repos (unless --force)
    if !force {
        let dirty_repos: Vec<&str> = repos
            .iter()
            .filter(|r| {
                git_status_summary(&r.path)
                    .map(|s| s.dirty)
                    .unwrap_or(false)
            })
            .map(|r| r.alias.as_str())
            .collect();

        if !dirty_repos.is_empty() {
            anyhow::bail!(
                "Worktree '{}' has uncommitted changes in: {}.\nUse --force to remove anyway.",
                name,
                dirty_repos.join(", ")
            );
        }
    }

    // Remove worktrees in correct order (children first, "." last)
    remove_worktree_repos(&repos, force, verbose)?;

    // Clean up directory if it still exists
    if wt_dir.exists() {
        std::fs::remove_dir_all(&wt_dir).ok();
    }

    // Remove from centralized store
    super::warn_store_error(store_remove(&wt_dir));

    // Fire post-destroy hook
    fire_post_destroy(name, &wt_dir, force, meta_dir.as_deref());

    if json {
        let output = DestroyOutput {
            name: name.to_string(),
            path: wt_dir.display().to_string(),
            repos_removed: repos.len(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "{} Destroyed worktree '{}'",
            "✓".green(),
            name.bold()
        );
    }
    Ok(())
}
