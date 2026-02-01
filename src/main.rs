//! meta-git subprocess plugin
//!
//! Logging is handled by `run_plugin()` which initializes env_logger.
//! Use RUST_LOG=meta_git_cli=debug for debug output.

use meta_plugin_protocol::{
    run_plugin, CommandResult, PluginDefinition, PluginHelp, PluginInfo, PluginRequest,
};
use std::collections::HashMap;
use std::path::PathBuf;

fn main() {
    let mut help_commands = HashMap::new();
    help_commands.insert(
        "clone".to_string(),
        "Clone a meta repository and all child repos".to_string(),
    );
    help_commands.insert(
        "status".to_string(),
        "Show git status for all repos".to_string(),
    );
    help_commands.insert(
        "update".to_string(),
        "Pull latest changes and clone missing repos".to_string(),
    );
    help_commands.insert(
        "setup-ssh".to_string(),
        "Configure SSH multiplexing for faster operations".to_string(),
    );
    help_commands.insert(
        "commit".to_string(),
        "Commit changes with per-repo messages".to_string(),
    );
    help_commands.insert(
        "snapshot create".to_string(),
        "Create a snapshot of all repos' git state".to_string(),
    );
    help_commands.insert(
        "snapshot list".to_string(),
        "List all available snapshots".to_string(),
    );
    help_commands.insert(
        "snapshot show".to_string(),
        "Show details of a snapshot".to_string(),
    );
    help_commands.insert(
        "snapshot restore".to_string(),
        "Restore all repos to a snapshot state".to_string(),
    );
    help_commands.insert(
        "snapshot delete".to_string(),
        "Delete a snapshot".to_string(),
    );
    help_commands.insert(
        "worktree create".to_string(),
        "Create a new worktree set".to_string(),
    );
    help_commands.insert(
        "worktree add".to_string(),
        "Add a repo to an existing worktree set".to_string(),
    );
    help_commands.insert(
        "worktree destroy".to_string(),
        "Remove a worktree set".to_string(),
    );
    help_commands.insert(
        "worktree list".to_string(),
        "List all worktree sets".to_string(),
    );
    help_commands.insert(
        "worktree status".to_string(),
        "Show status of a worktree set".to_string(),
    );
    help_commands.insert(
        "worktree diff".to_string(),
        "Show cross-repo diff vs base branch".to_string(),
    );
    help_commands.insert(
        "worktree exec".to_string(),
        "Run a command across worktree repos".to_string(),
    );
    help_commands.insert(
        "worktree prune".to_string(),
        "Remove expired/orphaned worktrees".to_string(),
    );
    // Root description for promoted top-level command
    help_commands.insert(
        "worktree".to_string(),
        "Manage git worktrees across repos".to_string(),
    );

    run_plugin(PluginDefinition {
        info: PluginInfo {
            name: "git".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            commands: vec![
                "git clone".to_string(),
                "git status".to_string(),
                "git update".to_string(),
                "git setup-ssh".to_string(),
                "git commit".to_string(),
                "git snapshot".to_string(),
                "git snapshot create".to_string(),
                "git snapshot list".to_string(),
                "git snapshot show".to_string(),
                "git snapshot restore".to_string(),
                "git snapshot delete".to_string(),
                "worktree".to_string(),
                "worktree create".to_string(),
                "worktree add".to_string(),
                "worktree destroy".to_string(),
                "worktree list".to_string(),
                "worktree status".to_string(),
                "worktree diff".to_string(),
                "worktree exec".to_string(),
                "worktree prune".to_string(),
                "git worktree".to_string(),
                "git worktree create".to_string(),
                "git worktree add".to_string(),
                "git worktree destroy".to_string(),
                "git worktree list".to_string(),
                "git worktree status".to_string(),
                "git worktree diff".to_string(),
                "git worktree exec".to_string(),
                "git worktree prune".to_string(),
            ],
            description: Some("Git operations for meta repositories".to_string()),
            help: Some(PluginHelp {
                usage: "meta git <command> [args...]".to_string(),
                commands: help_commands,
                examples: vec![
                    "meta git clone https://github.com/org/meta-repo.git".to_string(),
                    "meta git status".to_string(),
                    "meta git update".to_string(),
                    "meta git commit --edit".to_string(),
                    "meta git commit -m \"Update all repos\"".to_string(),
                    "meta git snapshot create before-upgrade".to_string(),
                    "meta git snapshot restore before-upgrade".to_string(),
                    "meta worktree create my-task --repo api --repo web".to_string(),
                    "meta worktree list".to_string(),
                    "meta worktree exec my-task -- cargo test".to_string(),
                    "meta worktree destroy my-task".to_string(),
                ],
                note: Some(
                    "To run raw git commands across repos: meta exec -- git <command>".to_string(),
                ),
            }),
        },
        execute,
    });
}

fn execute(request: PluginRequest) -> CommandResult {
    let cwd = if request.cwd.is_empty() {
        match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => return CommandResult::Error(format!("Failed to get working directory: {e}")),
        }
    } else {
        PathBuf::from(&request.cwd)
    };

    meta_git_cli::execute_command(
        &request.command,
        &request.args,
        &request.projects,
        &request.options,
        &cwd,
    )
}
