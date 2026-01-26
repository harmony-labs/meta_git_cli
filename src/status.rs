use crate::helpers::get_project_directories_with_fallback;
use meta_plugin_protocol::{CommandResult, PlannedCommand};
use std::collections::HashMap;
use std::path::Path;

pub(crate) fn execute_git_status(projects: &[String], cwd: &Path) -> anyhow::Result<CommandResult> {
    // Return an execution plan - let loop_lib handle execution, dry-run, and JSON output
    // Use projects from meta_cli if available (enables --recursive), otherwise read local .meta
    let dirs = get_project_directories_with_fallback(projects, cwd)?;

    // Disable pager for git commands run across repos to prevent blocking
    let git_env = Some(HashMap::from([("GIT_PAGER".to_string(), "cat".to_string())]));

    let commands: Vec<PlannedCommand> = dirs
        .into_iter()
        .map(|dir| PlannedCommand {
            dir,
            cmd: "git status".to_string(),
            env: git_env.clone(),
        })
        .collect();

    Ok(CommandResult::Plan(commands, Some(false))) // Sequential for status to keep output readable
}
