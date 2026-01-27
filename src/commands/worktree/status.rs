use anyhow::Result;
use colored::*;
use rayon::prelude::*;

use meta_git_lib::worktree::git_ops::*;
use meta_git_lib::worktree::helpers::discover_and_validate_worktree;
use meta_git_lib::worktree::types::*;

use super::cli_types::StatusArgs;

pub(crate) fn handle_status(args: StatusArgs, _verbose: bool, json: bool) -> Result<()> {
    let name = &args.name;

    let repos = discover_and_validate_worktree(name)?;

    let statuses: Vec<StatusRepoEntry> = repos
        .par_iter()
        .map(|r| {
            let summary = git_status_summary(&r.path).unwrap_or(GitStatusSummary {
                dirty: false,
                modified_files: vec![],
                untracked_count: 0,
            });
            let (ahead, behind) = git_ahead_behind(&r.path).unwrap_or((0, 0));

            StatusRepoEntry {
                alias: r.alias.clone(),
                path: r.path.display().to_string(),
                branch: r.branch.clone(),
                dirty: summary.dirty,
                modified_count: summary.modified_files.len(),
                untracked_count: summary.untracked_count,
                ahead,
                behind,
                modified_files: summary.modified_files,
            }
        })
        .collect();

    if json {
        let output = StatusOutput {
            name: name.to_string(),
            repos: statuses,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("{}:", name.bold());
        for s in &statuses {
            let status_icon = if s.dirty {
                "●".yellow().to_string()
            } else {
                "✓".green().to_string()
            };
            let mut details = Vec::new();
            if s.modified_count > 0 {
                details.push(format!("{} modified", s.modified_count));
            }
            if s.untracked_count > 0 {
                details.push(format!("{} untracked", s.untracked_count));
            }
            if s.ahead > 0 {
                details.push(format!("↑{}", s.ahead));
            }
            if s.behind > 0 {
                details.push(format!("↓{}", s.behind));
            }
            let detail_str = if details.is_empty() {
                "clean".to_string()
            } else {
                details.join(", ")
            };
            println!(
                "  {} {:12} {:20} {}",
                status_icon, s.alias, s.branch, detail_str
            );
        }
    }

    Ok(())
}
