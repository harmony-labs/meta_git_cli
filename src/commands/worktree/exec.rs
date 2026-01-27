use anyhow::Result;
use colored::*;

use meta_cli::worktree::discover_worktree_repos;
use meta_git_lib::worktree::helpers::*;

use super::cli_types::{CreateArgs, DestroyArgs, ExecArgs};

fn build_loop_config(
    directories: Vec<String>,
    include_filters: Vec<String>,
    exclude_filters: Vec<String>,
    parallel: bool,
    verbose: bool,
    json: bool,
) -> loop_lib::LoopConfig {
    loop_lib::LoopConfig {
        directories,
        ignore: vec![],
        include_filters: if include_filters.is_empty() {
            None
        } else {
            Some(include_filters)
        },
        exclude_filters: if exclude_filters.is_empty() {
            None
        } else {
            Some(exclude_filters)
        },
        verbose,
        silent: false,
        parallel,
        dry_run: false,
        json_output: json,
        add_aliases_to_global_looprc: false,
        spawn_stagger_ms: 0,
    }
}

/// RAII guard that destroys an ephemeral worktree on drop.
/// Ensures cleanup even if the exec command panics.
struct EphemeralGuard {
    name: String,
    verbose: bool,
    json: bool,
}

impl Drop for EphemeralGuard {
    fn drop(&mut self) {
        if self.verbose {
            eprintln!("Destroying ephemeral worktree '{}'...", self.name);
        }
        let destroy_args = DestroyArgs {
            name: self.name.clone(),
            force: true,
        };
        if let Err(e) = super::destroy::handle_destroy(destroy_args, self.verbose, self.json) {
            eprintln!(
                "{} Failed to destroy ephemeral worktree '{}': {e}",
                "warning:".yellow().bold(),
                self.name
            );
            eprintln!(
                "  Run 'meta worktree destroy {} --force' or 'meta worktree prune' to clean up.",
                self.name
            );
        }
    }
}

pub(crate) fn handle_exec(args: ExecArgs, verbose: bool, json: bool) -> Result<()> {
    if args.ephemeral {
        return handle_ephemeral_exec(args, verbose, json);
    }

    let name = &args.name;
    let repos = discover_and_validate_worktree(name)?;

    let directories: Vec<String> = repos
        .iter()
        .map(|r| r.path.display().to_string())
        .collect();

    let command_str = args.command.join(" ");
    let config = build_loop_config(
        directories,
        args.include,
        args.exclude,
        args.parallel,
        verbose,
        json,
    );

    loop_lib::run(&config, &command_str)?;
    Ok(())
}

fn handle_ephemeral_exec(args: ExecArgs, verbose: bool, json: bool) -> Result<()> {
    let name = args.name.clone();
    validate_worktree_name(&name)?;

    let cmd_parts = args.command;
    if cmd_parts.is_empty() {
        anyhow::bail!("No command specified after --");
    }

    // Extract filters and parallel flag before moving remaining args into CreateArgs
    let include_filters = args.include;
    let exclude_filters = args.exclude;
    let parallel = args.parallel;

    // Build CreateArgs from the exec args
    let create_args = CreateArgs {
        name: name.clone(),
        branch: args.branch,
        repos: args.repos,
        all: args.all,
        from_ref: args.from_ref,
        from_pr: args.from_pr,
        ephemeral: true,
        ttl: None,
        custom_meta: args.custom_meta,
    };

    if verbose {
        eprintln!("Creating ephemeral worktree '{name}'...");
    }
    super::create::handle_create(create_args, verbose, json)?;

    // Guard ensures cleanup even on panic
    let guard = EphemeralGuard {
        name: name.clone(),
        verbose,
        json,
    };

    // Resolve worktree path for exec
    let meta_dir = find_meta_dir();
    let worktree_root = resolve_worktree_root(meta_dir.as_deref())?;
    let wt_dir = worktree_root.join(&name);

    // Run the command
    let repos = discover_worktree_repos(&wt_dir)?;
    let directories: Vec<String> = repos
        .iter()
        .map(|r| r.path.display().to_string())
        .collect();

    let command_str = cmd_parts.join(" ");
    let config = build_loop_config(directories, include_filters, exclude_filters, parallel, verbose, json);

    let exec_result = loop_lib::run(&config, &command_str);

    // Explicitly drop guard to trigger cleanup before propagating result
    drop(guard);

    // Propagate exec result
    exec_result?;
    Ok(())
}
