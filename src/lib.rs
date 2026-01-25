//! meta-git library
//!
//! Provides git operations optimized for meta repositories.

mod clone;
mod clone_queue;
mod commit;
mod helpers;
mod snapshot;
mod ssh;
mod status;
mod update;

use log::debug;
use meta_plugin_protocol::{CommandResult, PlannedCommand};

use helpers::get_project_directories_with_fallback;

/// Execute a git command for meta repositories
///
/// The `projects` parameter is the list of project directories passed from meta_cli.
/// When meta_cli runs with `--recursive`, it discovers nested .meta files and passes
/// all project directories here. If `projects` is empty, we fall back to reading
/// the local .meta file via `get_project_directories()`.
pub fn execute_command(command: &str, args: &[String], projects: &[String]) -> CommandResult {
    debug!("[meta_git_cli] Plugin invoked with command: '{command}'");
    debug!("[meta_git_cli] Args: {args:?}");
    debug!("[meta_git_cli] Projects from meta_cli: {projects:?}");

    let result = match command {
        "git status" => status::execute_git_status(projects),
        "git clone" => clone::execute_git_clone(args),
        "git update" => update::execute_git_update(projects),
        "git setup-ssh" => ssh::execute_git_setup_ssh(),
        "git commit" => commit::execute_git_commit(args, projects),
        "git snapshot" => snapshot::execute_snapshot_help(),
        "git snapshot create" => snapshot::execute_snapshot_create(args, projects),
        "git snapshot list" => snapshot::execute_snapshot_list(),
        "git snapshot show" => snapshot::execute_snapshot_show(args),
        "git snapshot restore" => snapshot::execute_snapshot_restore(args, projects),
        "git snapshot delete" => snapshot::execute_snapshot_delete(args),
        // Fallback: run raw git command across all repos
        _ => return execute_raw_git_command(command, args, projects),
    };

    match result {
        Ok(cmd_result) => cmd_result,
        Err(e) => CommandResult::Error(format!("{e}")),
    }
}

/// Execute a raw git command across all repos (fallback for unrecognized subcommands)
fn execute_raw_git_command(command: &str, args: &[String], projects: &[String]) -> CommandResult {
    // Get project directories
    let dirs = match get_project_directories_with_fallback(projects) {
        Ok(d) => d,
        Err(e) => return CommandResult::Error(format!("Failed to get project directories: {e}")),
    };

    // Build the full git command string
    // command is e.g. "git add ." and args contains any additional arguments
    let full_cmd = if args.is_empty() {
        command.to_string()
    } else {
        format!("{} {}", command, args.join(" "))
    };

    let commands: Vec<PlannedCommand> = dirs
        .into_iter()
        .map(|dir| PlannedCommand {
            dir,
            cmd: full_cmd.clone(),
        })
        .collect();

    CommandResult::Plan(commands, Some(false)) // Sequential for readable output
}

/// Get help text for the plugin
pub fn get_help_text() -> &'static str {
    r#"meta git - Meta CLI Git Plugin

SPECIAL COMMANDS:
  These commands have meta-specific implementations:

  meta git clone <meta-repo-url> [options]
    Clones the meta repository and all child repositories defined in its manifest.

    Options:
      --recursive       Clone nested meta repositories recursively
      --parallel N      Clone up to N repositories in parallel
      --depth N         Create a shallow clone with truncated history

  meta git update
    Updates all repositories by cloning any missing repos and pulling the latest
    changes. Runs in parallel for efficiency.

  meta git setup-ssh
    Configures SSH multiplexing for faster parallel git operations.

  meta git commit --edit
    Opens an editor to create different commit messages for each repo.

SNAPSHOT COMMANDS (EXPERIMENTAL - file format subject to change):
  Capture and restore workspace state for safe batch operations:

  meta git snapshot create <name>
    Record the current git state (SHA, branch, dirty status) of ALL repos.
    Snapshots are recursive by default - they capture the entire workspace.

  meta git snapshot list
    List all available snapshots with creation date and repo count.

  meta git snapshot show <name>
    Display details of a snapshot including per-repo state.

  meta git snapshot restore <name> [--force] [--dry-run]
    Restore all repos to the recorded snapshot state. Prompts for confirmation.
    Dirty repos are automatically stashed before restore.
    Use --force to skip confirmation, --dry-run to preview changes.

  meta git snapshot delete <name>
    Delete a snapshot file.

PASS-THROUGH COMMANDS:
  All other git commands are passed through to each repository:

    meta git status      - Run 'git status' in all repos
    meta git pull        - Run 'git pull' in all repos
    meta git push        - Run 'git push' in all repos
    meta git checkout    - Run 'git checkout' in all repos
    meta git <any>       - Run 'git <any>' in all repos

FILTERING OPTIONS:
  These meta/loop options work with all pass-through commands:

    --tag <tags>        Filter by project tag(s), comma-separated
    --include-only      Only run in specified directories
    --exclude           Skip specified directories
    --parallel          Run commands in parallel

Examples:
  meta git clone https://github.com/example/meta-repo.git
  meta git status
  meta git pull --rebase
  meta git pull --tag backend
  meta git commit --edit
  meta git checkout -b feature/new --include-only api,frontend
  meta git snapshot create before-upgrade
  meta git snapshot restore before-upgrade
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use commit::parse_multi_commit_file;
    use helpers::get_project_directories;
    use meta_plugin_protocol::{ExecutionPlan, PlannedCommand, PlanResponse};
    use tempfile::TempDir;

    #[test]
    fn test_execute_git_status_no_meta_file() {
        let temp_dir = TempDir::new().unwrap();
        let original_dir = std::env::current_dir().unwrap();

        // Change to temp directory that has no .meta file
        std::env::set_current_dir(temp_dir.path()).unwrap();

        let result = execute_command("git status", &[], &[]);

        // Restore original directory
        std::env::set_current_dir(original_dir).unwrap();

        // Should succeed (returns a Plan, not an Error)
        assert!(!matches!(result, CommandResult::Error(_)));
    }

    #[test]
    fn test_meta_config_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join(".meta");
        std::fs::write(
            &meta_path,
            r#"{"projects": {"foo": "git@github.com:org/foo.git", "bar": "git@github.com:org/bar.git"}}"#,
        )
        .unwrap();
        let (projects, _) = meta_cli::config::parse_meta_config(&meta_path).unwrap();
        assert_eq!(projects.len(), 2);
        let names: std::collections::HashSet<_> = projects.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains("foo"));
        assert!(names.contains("bar"));
    }

    #[test]
    fn test_unknown_command_falls_through_to_raw_git() {
        // Unknown git subcommands should fall through to raw git execution
        let result = execute_command("git unknown", &[], &[]);
        // Should return a Plan (to run `git unknown` in all repos), not ShowHelp
        assert!(matches!(result, CommandResult::Plan(_, _)));
    }

    #[test]
    fn test_get_help_text() {
        let help = get_help_text();
        assert!(help.contains("meta git clone"));
        assert!(help.contains("meta git update"));
        assert!(help.contains("meta git setup-ssh"));
    }

    #[test]
    fn test_parse_multi_commit_file() {
        let content = r#"# Meta Multi-Commit
# Each section represents one repository.

========== meta_cli ==========
# 3 file(s) staged: src/main.rs, src/lib.rs, Cargo.toml

feat: add new feature

========== meta_mcp ==========
# 2 file(s) staged: src/main.rs, Cargo.toml

fix: fix bug in MCP server

========== empty_repo ==========
# 1 file(s) staged: file.rs

# No message here - should be skipped

"#;

        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].0, "meta_cli");
        assert_eq!(commits[0].1, "feat: add new feature");
        assert_eq!(commits[1].0, "meta_mcp");
        assert_eq!(commits[1].1, "fix: fix bug in MCP server");
    }

    #[test]
    fn test_parse_multi_commit_file_multiline_message() {
        let content = r#"========== repo1 ==========
# 1 file staged

feat: add feature

This is a longer description
that spans multiple lines.

- bullet point 1
- bullet point 2
"#;

        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].0, "repo1");
        assert!(commits[0].1.contains("feat: add feature"));
        assert!(commits[0].1.contains("bullet point 1"));
    }

    #[test]
    fn test_parse_multi_commit_file_empty() {
        let content = "";
        let commits = parse_multi_commit_file(content);
        assert!(commits.is_empty());
    }

    #[test]
    fn test_parse_multi_commit_file_only_comments() {
        let content = r#"# Meta Multi-Commit
# Each section represents one repository.
# This file has only comments
"#;
        let commits = parse_multi_commit_file(content);
        assert!(commits.is_empty());
    }

    #[test]
    fn test_parse_multi_commit_file_whitespace_only_message() {
        let content = r#"========== repo1 ==========
# 1 file staged



========== repo2 ==========
# 1 file staged

valid message
"#;
        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].0, "repo2");
        assert_eq!(commits[0].1, "valid message");
    }

    #[test]
    fn test_parse_multi_commit_file_special_characters_in_repo_name() {
        let content = r#"========== my-repo_v2.0 ==========
# 1 file staged

fix: handle special chars
"#;
        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].0, "my-repo_v2.0");
    }

    #[test]
    fn test_parse_multi_commit_file_preserves_message_whitespace() {
        let content = r#"========== repo ==========
# staged files

first line
  indented line
    more indent

last line
"#;
        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 1);
        assert!(commits[0].1.contains("  indented line"));
        assert!(commits[0].1.contains("    more indent"));
    }

    #[test]
    fn test_parse_multi_commit_file_deleted_section() {
        // Simulates user deleting a section entirely
        let content = r#"========== repo1 ==========
# 1 file staged

first commit

========== repo3 ==========
# 1 file staged

third commit
"#;
        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].0, "repo1");
        assert_eq!(commits[1].0, "repo3");
    }

    #[test]
    fn test_parse_multi_commit_file_single_repo() {
        let content = r#"========== only_repo ==========
# 5 files staged

the only commit message
"#;
        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].0, "only_repo");
        assert_eq!(commits[0].1, "the only commit message");
    }

    #[test]
    fn test_help_text_contains_commit_edit() {
        let help = get_help_text();
        assert!(help.contains("meta git commit --edit"));
        assert!(help.contains("PASS-THROUGH COMMANDS"));
        assert!(help.contains("FILTERING OPTIONS"));
    }

    // ============ Execution Plan Tests ============

    #[test]
    fn test_execution_plan_serialization() {
        let plan = ExecutionPlan {
            commands: vec![
                PlannedCommand {
                    dir: "./repo1".to_string(),
                    cmd: "git status".to_string(),
                },
                PlannedCommand {
                    dir: "./repo2".to_string(),
                    cmd: "git status".to_string(),
                },
            ],
            parallel: Some(false),
        };

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"dir\":\"./repo1\""));
        assert!(json.contains("\"cmd\":\"git status\""));
        assert!(json.contains("\"parallel\":false"));
    }

    #[test]
    fn test_execution_plan_without_parallel() {
        let plan = ExecutionPlan {
            commands: vec![PlannedCommand {
                dir: ".".to_string(),
                cmd: "ls".to_string(),
            }],
            parallel: None,
        };

        let json = serde_json::to_string(&plan).unwrap();
        // parallel should be omitted when None due to skip_serializing_if
        assert!(!json.contains("parallel"));
    }

    #[test]
    fn test_planned_command_serialization() {
        let cmd = PlannedCommand {
            dir: "/absolute/path".to_string(),
            cmd: "git pull --rebase".to_string(),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"dir\":\"/absolute/path\""));
        assert!(json.contains("\"cmd\":\"git pull --rebase\""));
    }

    #[test]
    fn test_plan_response_serialization() {
        let response = PlanResponse {
            plan: ExecutionPlan {
                commands: vec![PlannedCommand {
                    dir: "project".to_string(),
                    cmd: "make build".to_string(),
                }],
                parallel: Some(true),
            },
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"plan\":"));
        assert!(json.contains("\"commands\":"));
        assert!(json.contains("\"dir\":\"project\""));
        assert!(json.contains("\"cmd\":\"make build\""));
        assert!(json.contains("\"parallel\":true"));
    }

    #[test]
    fn test_plan_response_structure() {
        // Test that the JSON structure matches what subprocess_plugins expects
        let response = PlanResponse {
            plan: ExecutionPlan {
                commands: vec![
                    PlannedCommand {
                        dir: "a".to_string(),
                        cmd: "cmd1".to_string(),
                    },
                    PlannedCommand {
                        dir: "b".to_string(),
                        cmd: "cmd2".to_string(),
                    },
                ],
                parallel: Some(false),
            },
        };

        let json = serde_json::to_string(&response).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Verify structure matches what shim expects
        assert!(parsed.get("plan").is_some());
        let plan = parsed.get("plan").unwrap();
        assert!(plan.get("commands").is_some());
        let commands = plan.get("commands").unwrap().as_array().unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].get("dir").unwrap().as_str().unwrap(), "a");
        assert_eq!(commands[0].get("cmd").unwrap().as_str().unwrap(), "cmd1");
        assert_eq!(plan.get("parallel").unwrap().as_bool().unwrap(), false);
    }

    #[test]
    fn test_execution_plan_empty_commands() {
        let plan = ExecutionPlan {
            commands: vec![],
            parallel: None,
        };

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"commands\":[]"));
    }

    #[test]
    fn test_planned_command_with_special_chars() {
        let cmd = PlannedCommand {
            dir: "./path with spaces".to_string(),
            cmd: "git commit -m \"feat: add feature\"".to_string(),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        // JSON should properly escape the string
        assert!(json.contains("path with spaces"));
        assert!(json.contains("\\\"feat: add feature\\\""));
    }

    #[test]
    fn test_planned_command_git_clone() {
        let cmd = PlannedCommand {
            dir: "/home/user/workspace".to_string(),
            cmd: "git clone git@github.com:org/repo.git my-repo".to_string(),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("git clone"));
        assert!(json.contains("git@github.com:org/repo.git"));
    }

    #[test]
    fn test_execution_plan_many_commands() {
        let commands: Vec<PlannedCommand> = (0..100)
            .map(|i| PlannedCommand {
                dir: format!("./repo_{}", i),
                cmd: "git status".to_string(),
            })
            .collect();

        let plan = ExecutionPlan {
            commands,
            parallel: Some(true),
        };

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("./repo_0"));
        assert!(json.contains("./repo_99"));
        assert!(json.contains("\"parallel\":true"));
    }

    // ============ get_project_directories Tests ============

    #[test]
    fn test_get_project_directories_no_meta_file() {
        let temp_dir = TempDir::new().unwrap();
        let original_dir = std::env::current_dir().unwrap();

        std::env::set_current_dir(temp_dir.path()).unwrap();

        let dirs = get_project_directories().unwrap();

        std::env::set_current_dir(original_dir).unwrap();

        // Should return just "." when no .meta file
        assert_eq!(dirs, vec!["."]);
    }

    #[test]
    fn test_get_project_directories_with_meta_file() {
        let temp_dir = TempDir::new().unwrap();
        let original_dir = std::env::current_dir().unwrap();

        // Create .meta file
        let meta_content = r#"{"projects": {"alpha": "url1", "beta": "url2", "gamma": "url3"}}"#;
        std::fs::write(temp_dir.path().join(".meta"), meta_content).unwrap();

        std::env::set_current_dir(temp_dir.path()).unwrap();

        let dirs = get_project_directories().unwrap();

        std::env::set_current_dir(original_dir).unwrap();

        // Should return "." plus sorted project names
        assert_eq!(dirs.len(), 4);
        assert_eq!(dirs[0], ".");
        assert_eq!(dirs[1], "alpha");
        assert_eq!(dirs[2], "beta");
        assert_eq!(dirs[3], "gamma");
    }

    #[test]
    fn test_get_project_directories_sorted() {
        let temp_dir = TempDir::new().unwrap();
        let original_dir = std::env::current_dir().unwrap();

        // Create .meta file with unsorted projects
        let meta_content = r#"{"projects": {"zebra": "url1", "alpha": "url2", "middle": "url3"}}"#;
        std::fs::write(temp_dir.path().join(".meta"), meta_content).unwrap();

        std::env::set_current_dir(temp_dir.path()).unwrap();

        let dirs = get_project_directories().unwrap();

        std::env::set_current_dir(original_dir).unwrap();

        // Projects should be sorted alphabetically
        assert_eq!(dirs[1], "alpha");
        assert_eq!(dirs[2], "middle");
        assert_eq!(dirs[3], "zebra");
    }

    #[test]
    fn test_get_project_directories_empty_projects() {
        let temp_dir = TempDir::new().unwrap();
        let original_dir = std::env::current_dir().unwrap();

        // Create .meta file with no projects
        let meta_content = r#"{"projects": {}}"#;
        std::fs::write(temp_dir.path().join(".meta"), meta_content).unwrap();

        std::env::set_current_dir(temp_dir.path()).unwrap();

        let dirs = get_project_directories().unwrap();

        std::env::set_current_dir(original_dir).unwrap();

        // Should return just "."
        assert_eq!(dirs, vec!["."]);
    }

    // ============ Integration-like Tests ============

    #[test]
    fn test_git_status_returns_execution_plan_format() {
        // We can't easily test the actual output, but we can verify
        // the plan structure by creating one and checking its JSON format
        let dirs = vec![".".to_string(), "repo1".to_string(), "repo2".to_string()];

        let commands: Vec<PlannedCommand> = dirs
            .into_iter()
            .map(|dir| PlannedCommand {
                dir,
                cmd: "git status".to_string(),
            })
            .collect();

        let response = PlanResponse {
            plan: ExecutionPlan {
                commands,
                parallel: Some(false),
            },
        };

        let json = serde_json::to_string(&response).unwrap();

        // Verify it can be parsed as expected by the shim
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["plan"]["commands"].is_array());
        assert_eq!(parsed["plan"]["commands"].as_array().unwrap().len(), 3);
        assert_eq!(parsed["plan"]["parallel"].as_bool().unwrap(), false);
    }

    #[test]
    fn test_git_update_plan_for_missing_repos() {
        // Simulate what execute_git_update would return for missing repos
        let missing_repos = vec![
            ("repo1", "git@github.com:org/repo1.git"),
            ("repo2", "git@github.com:org/repo2.git"),
        ];

        let cwd = "/home/user/workspace";
        let commands: Vec<PlannedCommand> = missing_repos
            .iter()
            .map(|(name, url)| PlannedCommand {
                dir: cwd.to_string(),
                cmd: format!("git clone {} {}", url, name),
            })
            .collect();

        let response = PlanResponse {
            plan: ExecutionPlan {
                commands,
                parallel: Some(false),
            },
        };

        let json = serde_json::to_string(&response).unwrap();

        // Verify clone commands are in the plan
        assert!(json.contains("git clone"));
        assert!(json.contains("repo1"));
        assert!(json.contains("repo2"));
    }
}
