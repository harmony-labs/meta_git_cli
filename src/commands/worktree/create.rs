use anyhow::Result;
use chrono::Utc;
use colored::*;
use std::collections::HashMap;

use meta_git_lib::worktree::git_ops::*;
use meta_git_lib::worktree::helpers::*;
use meta_git_lib::worktree::hooks::fire_post_create;
use meta_git_lib::worktree::store::store_add;
use meta_git_lib::worktree::types::{
    CreateOutput, CreateRepoEntry, StoreRepoEntry, WorktreeStoreEntry,
};

use super::cli_types::CreateArgs;

pub(crate) fn handle_create(args: CreateArgs, verbose: bool, json: bool) -> Result<()> {
    let name = &args.name;
    validate_worktree_name(name)?;

    let branch_flag = args.branch.as_deref();
    let repo_specs = &args.repos;
    let use_all = args.all;
    let ephemeral = args.ephemeral;
    let ttl_seconds = args.ttl;
    let custom_meta: HashMap<String, String> = args
        .custom_meta
        .iter()
        .filter_map(|s| {
            if let Some(eq_pos) = s.find('=') {
                Some((s[..eq_pos].to_string(), s[eq_pos + 1..].to_string()))
            } else {
                eprintln!(
                    "{} --meta value '{}' missing '=' separator (expected key=value), skipping",
                    "warning:".yellow().bold(),
                    s
                );
                None
            }
        })
        .collect();
    let from_ref = args.from_ref.as_deref();
    let from_pr_spec = args.from_pr.as_deref();
    let strict = args.strict;

    // Check mutual exclusion of --from-ref and --from-pr
    if from_ref.is_some() && from_pr_spec.is_some() {
        anyhow::bail!("--from-ref and --from-pr are mutually exclusive");
    }

    // Resolve --from-pr: get PR head branch and identify matching repo
    let from_pr_info = from_pr_spec.map(resolve_from_pr).transpose()?;

    if repo_specs.is_empty() && !use_all {
        anyhow::bail!("Specify repos with --repo <alias> or use --all");
    }

    let meta_dir = require_meta_dir()?;
    let worktree_root = resolve_worktree_root(Some(&meta_dir))?;

    // Check if worktree already exists
    let wt_dir = worktree_root.join(name);
    if wt_dir.exists() {
        anyhow::bail!(
            "Worktree '{}' already exists at {}. Use 'meta worktree destroy {}' first.",
            name,
            wt_dir.display(),
            name
        );
    }

    // Parse .meta to get project list
    let projects = load_projects(&meta_dir)?;

    // Determine which repos to include: Vec<(alias, source_path, branch)>
    let repos_to_create: Vec<(String, std::path::PathBuf, String)> = if use_all {
        projects
            .iter()
            .map(|p| {
                let per_branch = repo_specs
                    .iter()
                    .find(|r| r.alias == p.name)
                    .and_then(|r| r.branch.as_deref());
                (
                    p.name.clone(),
                    meta_dir.join(&p.path),
                    resolve_branch(name, branch_flag, per_branch),
                )
            })
            .collect()
    } else {
        let mut list = Vec::new();
        for spec in repo_specs {
            if spec.alias == "." {
                list.push((
                    ".".to_string(),
                    meta_dir.clone(),
                    resolve_branch(name, branch_flag, spec.branch.as_deref()),
                ));
            } else {
                let project = lookup_project(&projects, &spec.alias)?;
                list.push((
                    spec.alias.clone(),
                    meta_dir.join(&project.path),
                    resolve_branch(name, branch_flag, spec.branch.as_deref()),
                ));
            }
        }
        list
    };

    // Apply --from-pr: override branch for the matching repo and fetch
    let mut repos_to_create = repos_to_create;
    if let Some((ref pr_repo_spec, _pr_num, ref pr_branch)) = from_pr_info {
        let mut matched = false;
        for (alias, source, branch) in repos_to_create.iter_mut() {
            if *alias != "." && repo_matches_spec(source, pr_repo_spec) {
                // Fetch the PR branch
                if let Err(e) = git_fetch_branch(source, pr_branch) {
                    eprintln!(
                        "{} Failed to fetch PR branch '{}': {}",
                        "warning:".yellow().bold(),
                        pr_branch,
                        e
                    );
                }
                *branch = pr_branch.clone();
                matched = true;
                break;
            }
        }
        if !matched {
            eprintln!(
                "{} No repo matches '{}'. PR branch '{}' not applied.",
                "warning:".yellow().bold(),
                pr_repo_spec,
                pr_branch
            );
        }
    }

    let dot_included = repos_to_create.iter().any(|(a, _, _)| a == ".");
    let mut created_repos = Vec::new();

    // If "." is included, create it first (it becomes the worktree root).
    // git worktree add creates the target dir, so we skip create_dir_all.
    if dot_included {
        let (_, source, branch) = repos_to_create
            .iter()
            .find(|(a, _, _)| a == ".")
            .unwrap();

        if verbose {
            eprintln!(
                "Creating meta repo worktree at {} (branch: {})",
                wt_dir.display(),
                branch
            );
        }

        // Ensure parent exists (git worktree add creates the leaf dir)
        if let Some(parent) = wt_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let created_branch = git_worktree_add(source, &wt_dir, branch, from_ref)?;
        created_repos.push(CreateRepoEntry {
            alias: ".".to_string(),
            path: wt_dir.display().to_string(),
            branch: branch.clone(),
            created_branch,
        });
    }

    // Ensure wt_dir exists for child repos (when "." isn't included, it wasn't created by git)
    if !dot_included {
        std::fs::create_dir_all(&wt_dir)?;
    }

    // Create child repo worktrees
    for (alias, source, branch) in &repos_to_create {
        if alias == "." {
            continue;
        }

        let dest = wt_dir.join(alias);

        if verbose {
            eprintln!(
                "Creating worktree for '{}' at {} (branch: {})",
                alias,
                dest.display(),
                branch
            );
        }

        match git_worktree_add(source, &dest, branch, from_ref) {
            Ok(created_branch) => {
                created_repos.push(CreateRepoEntry {
                    alias: alias.clone(),
                    path: dest.display().to_string(),
                    branch: branch.clone(),
                    created_branch,
                });
            }
            Err(e) if from_ref.is_some() => {
                // --from-ref: skip repos where ref doesn't exist
                if strict {
                    // --strict: error instead of warning
                    anyhow::bail!(
                        "Repo '{}' skipped due to missing ref (strict mode enabled): {}",
                        alias,
                        e
                    );
                } else {
                    eprintln!(
                        "{} Skipping '{}': {}",
                        "warning:".yellow().bold(),
                        alias,
                        e
                    );
                    continue;
                }
            }
            Err(e) => return Err(e),
        }
    }

    // Ensure .worktrees/ is in .gitignore
    let dirname = worktree_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(".worktrees");
    ensure_worktrees_in_gitignore(&meta_dir, dirname, json)?;

    // Add to centralized store
    let store_entry = WorktreeStoreEntry {
        name: name.to_string(),
        project: meta_dir.to_string_lossy().to_string(),
        created_at: Utc::now().to_rfc3339(),
        ephemeral,
        ttl_seconds,
        repos: created_repos.iter().map(StoreRepoEntry::from).collect(),
        custom: custom_meta.clone(),
    };
    super::warn_store_error(store_add(&wt_dir, store_entry));

    // Fire post-create hook
    fire_post_create(
        name,
        &wt_dir,
        &created_repos,
        ephemeral,
        ttl_seconds,
        &custom_meta,
        Some(&meta_dir),
    );

    // Output
    if json {
        let output = CreateOutput {
            name: name.to_string(),
            root: wt_dir.display().to_string(),
            repos: created_repos,
            ephemeral,
            ttl_seconds,
            custom: custom_meta,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "{} Created worktree '{}' at {}",
            "✓".green(),
            name.bold(),
            wt_dir.display()
        );
        for r in &created_repos {
            let branch_note = if r.created_branch { " (new)" } else { "" };
            println!("  {} -> {}{}", r.alias, r.branch, branch_note);
        }
        if ephemeral {
            println!("  {}", "[ephemeral]".dimmed());
        }
        if let Some(ttl) = ttl_seconds {
            println!(
                "  {}",
                format!("[TTL: {}]", format_duration(ttl as i64)).dimmed()
            );
        }
    }

    Ok(())
}
