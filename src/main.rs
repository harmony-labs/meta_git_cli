//! meta-git subprocess plugin

use meta_plugin_protocol::{
    CommandResult, PluginDefinition, PluginHelp, PluginInfo, PluginRequest, run_plugin,
};
use std::collections::HashMap;

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
                ],
                note: Some(
                    "To run raw git commands across repos: meta exec -- git <command>".to_string(),
                ),
            }),
        },
        execute: execute,
    });
}

fn execute(request: PluginRequest) -> CommandResult {
    if !request.cwd.is_empty() {
        if let Err(e) = std::env::set_current_dir(&request.cwd) {
            return CommandResult::Error(format!("Failed to set working directory: {e}"));
        }
    }

    meta_git_cli::execute_command(
        &request.command,
        &request.args,
        &request.projects,
        request.options.dry_run,
    )
}
