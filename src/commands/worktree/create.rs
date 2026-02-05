use anyhow::Result;
use chrono::Utc;
use colored::*;
use std::collections::{HashMap, HashSet};

use meta_cli::dependency_graph::DependencyGraph;
use meta_git_lib::worktree::git_ops::*;
use meta_git_lib::worktree::helpers::*;
use meta_git_lib::worktree::hooks::fire_post_create;
use meta_git_lib::worktree::store::store_add;
use meta_git_lib::worktree::types::{
    CreateOutput, CreateRepoEntry, StoreRepoEntry, WorktreeStoreEntry,
};

use super::cli_types::CreateArgs;

pub(crate) fn handle_create(
    args: CreateArgs,
    verbose: bool,
    json: bool,
    global_strict: bool,
) -> Result<()> {
    // Merge global --strict with local --strict (either enables strict mode)
    let strict = args.strict || global_strict;

    let name = &args.name;
    validate_worktree_name(name)?;

    let branch_flag = args.branch.as_deref();
    let repo_specs = &args.repos;
    let use_all = args.all;
    let ephemeral = args.ephemeral;
    let ttl_seconds = args.ttl;
    // Parse custom metadata, collecting any invalid entries for strict mode
    let mut custom_meta = HashMap::new();
    for s in &args.custom_meta {
        if let Some(eq_pos) = s.find('=') {
            custom_meta.insert(s[..eq_pos].to_string(), s[eq_pos + 1..].to_string());
        } else {
            super::warn_or_bail(
                strict,
                format!("--meta value '{s}' missing '=' separator (expected key=value), skipping"),
            )?;
        }
    }
    let from_ref = args.from_ref.as_deref();
    let from_pr_spec = args.from_pr.as_deref();

    // Check mutual exclusion of --from-ref and --from-pr
    if from_ref.is_some() && from_pr_spec.is_some() {
        anyhow::bail!("--from-ref and --from-pr are mutually exclusive");
    }

    // Resolve --from-pr: get PR head branch and identify matching repo
    let from_pr_info = from_pr_spec.map(resolve_from_pr).transpose()?;

    let no_deps = args.no_deps;

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
    // When --all is specified, include root repo "." if it's a git repository
    let projects = meta_git_lib::worktree::helpers::load_projects_with_root(&meta_dir, use_all)?;

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
    } else if no_deps {
        // --no-deps: only include explicitly specified repos (legacy behavior)
        let mut list = Vec::new();
        for spec in repo_specs {
            if spec.alias == "." {
                list.push((
                    ".".to_string(),
                    meta_dir.clone(),
                    resolve_branch(name, branch_flag, spec.branch.as_deref()),
                ));
            } else {
                let (source, _project) = lookup_nested_project(&meta_dir, &spec.alias)?;
                list.push((
                    spec.alias.clone(),
                    source,
                    resolve_branch(name, branch_flag, spec.branch.as_deref()),
                ));
            }
        }
        list
    } else {
        // Default: auto-include root repo + resolve dependencies
        resolve_repos_with_dependencies(
            &meta_dir,
            &projects,
            repo_specs,
            name,
            branch_flag,
            verbose,
        )?
    };

    // Apply --from-pr: override branch for the matching repo and fetch
    let mut repos_to_create = repos_to_create;
    if let Some((ref pr_repo_spec, _pr_num, ref pr_branch)) = from_pr_info {
        let mut matched = false;
        for (alias, source, branch) in repos_to_create.iter_mut() {
            if *alias != "." && repo_matches_spec(source, pr_repo_spec) {
                // Fetch the PR branch
                if let Err(e) = git_fetch_branch(source, pr_branch) {
                    super::warn_or_bail(
                        strict,
                        format!("Failed to fetch PR branch '{pr_branch}': {e}"),
                    )?;
                }
                *branch = pr_branch.clone();
                matched = true;
                break;
            }
        }
        if !matched {
            super::warn_or_bail(
                strict,
                format!("No repo matches '{pr_repo_spec}'. PR branch '{pr_branch}' not applied."),
            )?;
        }
    }

    let dot_included = repos_to_create.iter().any(|(a, _, _)| a == ".");
    let mut created_repos = Vec::new();

    // If "." is included, create it first (it becomes the worktree root).
    // git worktree add creates the target dir, so we skip create_dir_all.
    let mut dot_created = false;
    if dot_included {
        let (_, source, branch) = repos_to_create.iter().find(|(a, _, _)| a == ".").unwrap();

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

        match git_worktree_add(source, &wt_dir, branch, from_ref) {
            Ok(created_branch) => {
                created_repos.push(CreateRepoEntry {
                    alias: ".".to_string(),
                    path: wt_dir.display().to_string(),
                    branch: branch.clone(),
                    created_branch,
                });
                dot_created = true;
            }
            Err(e) if from_ref.is_some() => {
                // --from-ref: skip root repo if ref doesn't exist (same as child repos)
                super::warn_or_bail(strict, format!("Skipping '.': {e}"))?;
            }
            Err(e) => return Err(e),
        }
    }

    // Ensure wt_dir exists for child repos (when "." isn't included or was skipped, it wasn't created by git)
    if !dot_created {
        std::fs::create_dir_all(&wt_dir)?;
    }

    // Create child repo worktrees
    for (alias, source, branch) in &repos_to_create {
        if alias == "." {
            continue;
        }

        // Use the last component of the alias for the destination directory
        // e.g., "vendor/nested-lib" -> "nested-lib"
        let dest_name = alias.rsplit('/').next().unwrap_or(alias);
        let dest = wt_dir.join(dest_name);

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
                super::warn_or_bail(strict, format!("Skipping '{alias}': {e}"))?;
                continue;
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
    super::warn_store_error(store_add(&wt_dir, store_entry), strict)?;

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

/// Resolve repos with automatic dependency resolution.
///
/// When --repo is specified without --no-deps:
/// 1. Always includes root repo "." (contains workspace Cargo.toml)
/// 2. Resolves transitive dependencies via provides/depends_on from .meta.yaml
fn resolve_repos_with_dependencies(
    meta_dir: &std::path::Path,
    projects: &[meta_cli::config::ProjectInfo],
    repo_specs: &[meta_git_lib::worktree::RepoSpec],
    worktree_name: &str,
    branch_flag: Option<&str>,
    verbose: bool,
) -> Result<Vec<(String, std::path::PathBuf, String)>> {
    // Build dependency graph from projects
    let project_deps: Vec<_> = projects.iter().map(|p| p.to_dependencies()).collect();
    let graph = DependencyGraph::build(project_deps)?;

    // Collect all repos to include (using HashSet for deduplication)
    let mut repos_to_include: HashSet<String> = HashSet::new();

    // Always include root repo "." unless user explicitly excluded it
    // (exclusion would need to be handled separately, for now always include)
    if meta_dir.join(".git").exists() {
        repos_to_include.insert(".".to_string());
    }

    // For each explicitly requested repo, add it and its transitive dependencies
    for spec in repo_specs {
        if spec.alias == "." {
            repos_to_include.insert(".".to_string());
            continue;
        }

        // Add the explicitly requested repo
        repos_to_include.insert(spec.alias.clone());

        // Get transitive dependencies
        let deps = graph.get_all_dependencies(&spec.alias);
        log::debug!(
            "Resolved transitive dependencies for '{}': {:?}",
            spec.alias,
            deps
        );
        for dep in deps {
            repos_to_include.insert(dep.to_string());
            if verbose {
                eprintln!("  Including '{}' (dependency of '{}')", dep, spec.alias);
            }
        }
    }

    // Build the final list with paths and branches
    let mut list = Vec::new();

    // Add root repo first if included
    if repos_to_include.contains(".") {
        let per_branch = repo_specs
            .iter()
            .find(|r| r.alias == ".")
            .and_then(|r| r.branch.as_deref());
        list.push((
            ".".to_string(),
            meta_dir.to_path_buf(),
            resolve_branch(worktree_name, branch_flag, per_branch),
        ));
    }

    // Add other repos
    for alias in &repos_to_include {
        if alias == "." {
            continue;
        }

        let (source, _project) = lookup_nested_project(meta_dir, alias)?;
        let per_branch = repo_specs
            .iter()
            .find(|r| r.alias == *alias)
            .and_then(|r| r.branch.as_deref());
        list.push((
            alias.clone(),
            source,
            resolve_branch(worktree_name, branch_flag, per_branch),
        ));
    }

    Ok(list)
}
