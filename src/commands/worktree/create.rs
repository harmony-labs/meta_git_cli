use anyhow::Result;
use chrono::Utc;
use colored::*;
use std::collections::{HashMap, HashSet};

use meta_cli::dependency_graph::{DependencyGraph, ProjectDependencies};
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
    // Merge positional <commit-ish> with hidden --from-ref (clap prevents both)
    if args.from_ref.is_some() {
        log::warn!(
            "--from-ref is deprecated; use positional: meta worktree create <name> <commit-ish>"
        );
    }
    let from_ref_merged = args.commit_ish.or(args.from_ref);
    let from_ref = from_ref_merged.as_deref();
    let from_pr_spec = args.from_pr.as_deref();

    // Belt-and-suspenders: clap enforces conflicts, but guard against programmatic construction
    if from_ref.is_some() && from_pr_spec.is_some() {
        anyhow::bail!("Cannot specify both a commit-ish and --from-pr");
    }

    // Resolve --from-pr: get PR head branch and identify matching repo
    let from_pr_info = from_pr_spec.map(resolve_from_pr).transpose()?;

    let no_deps = args.no_deps;
    let recursive = args.recursive;

    if repo_specs.is_empty() && !use_all {
        anyhow::bail!("Specify repos with --repo <alias> or use --all");
    }

    let nearest_meta_dir = require_meta_dir()?;
    let meta_dir = if recursive {
        meta_core::config::find_root_meta_dir(&nearest_meta_dir)
    } else {
        nearest_meta_dir
    };
    let worktree_root = resolve_worktree_root(Some(&meta_dir))?;

    // Check if worktree already exists
    let wt_dir = worktree_root.join(name);
    if wt_dir.exists() {
        anyhow::bail!(
            "Worktree '{}' already exists at {}. Use 'meta worktree remove {}' first.",
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
            recursive,
        )?
    };

    // Expand meta: true repos to include their child repos in the worktree
    let repos_to_create = expand_meta_children(repos_to_create, strict)?;

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

        // Preserve the full alias as the destination path so that relative
        // references (e.g., Cargo.toml workspace members) remain valid.
        // "vendor/tree-sitter-markdown" -> "vendor/tree-sitter-markdown"
        // Simple aliases are unchanged: "core" -> "core"
        let dest = wt_dir.join(alias);

        // Ensure parent directories exist for nested paths (e.g., "vendor/")
        if let Some(parent) = dest.parent() {
            if parent != wt_dir {
                std::fs::create_dir_all(parent)?;
            }
        }

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
///
/// When `recursive` is true, walks the full nested meta tree from `meta_dir`
/// (which should already be the root), building a dependency graph where all
/// project names and deps use full paths (e.g., "open-source/gitkb/core").
fn resolve_repos_with_dependencies(
    meta_dir: &std::path::Path,
    projects: &[meta_core::config::ProjectInfo],
    repo_specs: &[meta_git_lib::worktree::RepoSpec],
    worktree_name: &str,
    branch_flag: Option<&str>,
    verbose: bool,
    recursive: bool,
) -> Result<Vec<(String, std::path::PathBuf, String)>> {
    // Build dependency graph — either flat (current level) or nested (full tree)
    let graph = if recursive {
        build_nested_dep_graph(meta_dir)?
    } else {
        let project_deps: Vec<_> = projects.iter().map(|p| p.clone().into()).collect();
        DependencyGraph::build(project_deps)?
    };

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

        // When recursive, graph keys are full paths (e.g., "open-source/gitkb/core").
        // Resolve spec.alias against the graph to handle both full-path and short aliases.
        let resolved_alias = if recursive {
            resolve_alias_in_graph(&graph, &spec.alias)?
        } else {
            spec.alias.clone()
        };

        // Add the explicitly requested repo
        repos_to_include.insert(resolved_alias.clone());

        // Get transitive dependencies
        let deps = graph.get_all_dependencies(&resolved_alias);
        log::debug!(
            "Resolved transitive dependencies for '{}': {:?}",
            resolved_alias,
            deps
        );
        for dep in deps {
            repos_to_include.insert(dep.to_string());
            if verbose {
                eprintln!("  Including '{}' (dependency of '{}')", dep, resolved_alias);
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

    // Precompute a mapping from resolved alias to per-repo branch so we avoid
    // repeated graph scans inside the per-repo loop below.
    let resolved_branch_map: HashMap<String, Option<&str>> = repo_specs
        .iter()
        .filter_map(|r| {
            let resolved = resolve_alias_in_graph(&graph, &r.alias).ok()?;
            Some((resolved, r.branch.as_deref()))
        })
        .collect();

    // Add other repos
    for alias in &repos_to_include {
        if alias == "." {
            continue;
        }

        let (source, _project) = lookup_nested_project(meta_dir, alias)?;
        let per_branch = resolved_branch_map.get(alias.as_str()).copied().flatten();
        list.push((
            alias.clone(),
            source,
            resolve_branch(worktree_name, branch_flag, per_branch),
        ));
    }

    Ok(list)
}

/// Resolve a short alias (e.g., "core") to its full graph key (e.g., "open-source/gitkb/core").
///
/// If the alias directly matches a graph key, returns it as-is.
/// Otherwise, looks for a unique graph key that ends with "/<alias>".
/// Errors if no match or multiple matches are found.
fn resolve_alias_in_graph(graph: &DependencyGraph, alias: &str) -> Result<String> {
    // Direct match — already a full path
    if graph.get_project(alias).is_some() {
        return Ok(alias.to_string());
    }

    // Try suffix match: find graph keys ending with "/<alias>"
    let suffix = format!("/{}", alias);
    let matches: Vec<&str> = graph
        .all_projects()
        .iter()
        .filter(|p| p.name.ends_with(&suffix) || p.name == alias)
        .map(|p| p.name.as_str())
        .collect();

    match matches.len() {
        1 => Ok(matches[0].to_string()),
        0 => anyhow::bail!(
            "Repo '{}' not found in the recursive project graph. \
             With --recursive, use the full path from root (e.g., 'open-source/gitkb/core').",
            alias
        ),
        _ => anyhow::bail!(
            "Repo alias '{}' is ambiguous — matches: {}. \
             Use the full path to disambiguate.",
            alias,
            matches.join(", ")
        ),
    }
}

/// Build a dependency graph from the full nested meta tree.
///
/// Walks `meta_dir`'s meta tree recursively, builds a project map with
/// fully-qualified paths (e.g., "open-source/gitkb/core"), then prefixes
/// each project's `depends_on` and `provides` with its parent path so
/// cross-workspace dependencies resolve correctly.
///
/// Example: project "core" at path "open-source/gitkb/core" with
/// `depends_on: ["vendor/tree-sitter-markdown"]` becomes
/// `depends_on: ["open-source/gitkb/vendor/tree-sitter-markdown"]`.
fn build_nested_dep_graph(meta_dir: &std::path::Path) -> Result<DependencyGraph> {
    let tree = meta_core::config::walk_meta_tree(meta_dir, None)?;
    let project_map = meta_core::config::build_project_map(&tree, meta_dir, "");

    let mut project_deps = Vec::new();
    for (full_path, (_fs_path, info)) in &project_map {
        // Compute the parent prefix: strip the local path from the full path.
        // e.g., full_path="open-source/gitkb/core", info.path="core"
        //        → parent_prefix="open-source/gitkb/"
        let parent_prefix = full_path
            .strip_suffix(info.path.as_str())
            .filter(|prefix| prefix.is_empty() || prefix.ends_with('/'))
            .unwrap_or("");

        let qualify = |name: &str| {
            if project_map.contains_key(name) || name == "." {
                name.to_string()
            } else {
                format!("{}{}", parent_prefix, name)
            }
        };

        let prefixed_deps: Vec<String> = info.depends_on.iter().map(|dep| qualify(dep)).collect();
        let prefixed_provides: Vec<String> = info.provides.iter().map(|p| qualify(p)).collect();

        project_deps.push(ProjectDependencies {
            name: full_path.clone(),
            path: full_path.clone(),
            repo: info.repo.clone(),
            tags: info.tags.clone(),
            provides: prefixed_provides,
            depends_on: prefixed_deps,
        });
    }

    DependencyGraph::build(project_deps)
}

/// Expand `repos_to_create` to include children of `meta: true` projects.
///
/// When a repo in the list has its own `.meta.yaml` (i.e., it's a nested meta
/// workspace), its child projects are added to the list so they get worktrees
/// too. Without this, the worktree for a `meta: true` repo like `gitkb/` would
/// be missing `gitkb/core/`, `gitkb/ui/`, etc.
///
/// Children inherit the task branch so the entire workspace is on the same branch.
/// Uses `walk_meta_tree` which handles full recursive descent — if a child is
/// itself `meta: true`, its children are included too.
fn expand_meta_children(
    repos: Vec<(String, std::path::PathBuf, String)>,
    strict: bool,
) -> Result<Vec<(String, std::path::PathBuf, String)>> {
    // Two-pass approach: first collect all explicit aliases so they take priority
    // over synthesized children (prevents order-dependent override issues when
    // both a parent and an explicit child with a custom branch are in the list).
    let explicit_aliases: HashSet<String> = repos.iter().map(|(a, _, _)| a.clone()).collect();

    let mut expanded = Vec::new();
    let mut seen = HashSet::new();

    for (alias, source, branch) in repos {
        if !seen.insert(alias.clone()) {
            continue;
        }
        expanded.push((alias.clone(), source.clone(), branch.clone()));

        // Skip root repo
        if alias == "." {
            continue;
        }

        // Check if this project has its own .meta config (meta: true).
        // Important: use find_meta_config_in (checks only IN this dir),
        // NOT find_meta_config (which walks UP to parent dirs).
        if meta_core::config::find_meta_config_in(&source).is_none() {
            continue;
        }

        // Walk the child's meta tree to find its sub-projects
        let tree = match meta_core::config::walk_meta_tree(&source, None) {
            Ok(t) => t,
            Err(e) => {
                super::warn_or_bail(
                    strict,
                    format!("Failed to walk meta tree for '{alias}': {e}"),
                )?;
                continue;
            }
        };

        let project_map = meta_core::config::build_project_map(&tree, &source, "");
        let child_count = project_map.len();
        for (child_path, (child_fs_path, _info)) in &project_map {
            let full_alias = format!("{}/{}", alias, child_path);
            // Skip if this alias was explicitly provided (it will be processed
            // with its own branch/source from the input list)
            if explicit_aliases.contains(&full_alias) {
                continue;
            }
            if seen.insert(full_alias.clone()) {
                // Children inherit the parent's already-resolved branch
                let child_branch = branch.clone();
                expanded.push((full_alias, child_fs_path.clone(), child_branch));
            }
        }

        if child_count > 0 {
            log::info!(
                "Expanded '{}' (meta: true): added {} child repo{}",
                alias,
                child_count,
                if child_count == 1 { "" } else { "s" }
            );
        }
    }

    Ok(expanded)
}
