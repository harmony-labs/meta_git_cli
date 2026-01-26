use console::style;
use meta_plugin_protocol::CommandResult;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;

pub(crate) fn execute_git_setup_ssh(cwd: &Path) -> anyhow::Result<CommandResult> {
    // Step 1: Check and fix remote URL mismatches
    check_and_fix_remotes(cwd);

    // Step 2: SSH multiplexing setup (existing behavior)
    let hosts = discover_ssh_hosts(cwd);
    let host_refs: Vec<&str> = hosts.iter().map(|s| s.as_str()).collect();

    if meta_git_lib::is_multiplexing_configured(&host_refs) {
        println!(
            "{} SSH multiplexing is already configured.",
            style("✓").green()
        );
        println!("  Your parallel git operations should work efficiently.");
    } else {
        match meta_git_lib::prompt_and_setup_multiplexing(&host_refs) {
            Ok(true) => {
                println!();
                println!(
                    "You can now run {} without SSH rate limiting issues.",
                    style("meta git update").cyan()
                );
            }
            Ok(false) => {
                // User declined, message already shown
            }
            Err(e) => {
                return Ok(CommandResult::Error(format!(
                    "Failed to set up SSH multiplexing: {e}"
                )));
            }
        }
    }
    Ok(CommandResult::Message(String::new()))
}

/// A remote URL mismatch between .meta config and the actual repo.
struct RemoteMismatch {
    name: String,
    expected: String,
    actual: String,
}

/// Check child repos for remote URL mismatches against .meta config,
/// and offer to fix them.
fn check_and_fix_remotes(cwd: &Path) {
    let Some((config_path, _format)) = meta_cli::config::find_meta_config(cwd, None) else {
        return;
    };

    let Ok((projects, _ignore)) = meta_cli::config::parse_meta_config(&config_path) else {
        return;
    };

    let mut mismatches = Vec::new();

    for project in &projects {
        let repo_path = cwd.join(&project.path);
        if !repo_path.join(".git").exists() && !repo_path.exists() {
            // Not cloned yet — skip gracefully
            continue;
        }

        let Some(actual_url) = meta_git_lib::get_remote_url(&repo_path) else {
            continue;
        };

        if !meta_git_lib::urls_match(&actual_url, &project.repo) {
            mismatches.push(RemoteMismatch {
                name: project.name.clone(),
                expected: project.repo.clone(),
                actual: actual_url,
            });
        }
    }

    if mismatches.is_empty() {
        println!(
            "{} All remote URLs match .meta config.",
            style("✓").green()
        );
        return;
    }

    println!();
    println!(
        "{} Found {} remote URL mismatch{}:",
        style("!").yellow().bold(),
        mismatches.len(),
        if mismatches.len() == 1 { "" } else { "es" }
    );
    println!();

    for m in &mismatches {
        println!("  {}", style(&m.name).bold());
        println!("    actual:   {}", style(&m.actual).red());
        println!("    expected: {}", style(&m.expected).green());
    }

    println!();
    print!(
        "Fix {} to match .meta? [y/N]: ",
        if mismatches.len() == 1 {
            "this remote"
        } else {
            "these remotes"
        }
    );
    io::stdout().flush().ok();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return;
    }

    if input.trim().to_lowercase() != "y" {
        println!("Skipped. You can fix remotes manually with:");
        for m in &mismatches {
            println!(
                "  git -C {} remote set-url origin {}",
                m.name, m.expected
            );
        }
        return;
    }

    for m in &mismatches {
        let repo_path = cwd.join(&m.name);
        let output = std::process::Command::new("git")
            .args(["remote", "set-url", "origin", &m.expected])
            .current_dir(&repo_path)
            .output();

        match output {
            Ok(o) if o.status.success() => {
                println!(
                    "  {} {} → {}",
                    style("✓").green(),
                    style(&m.name).bold(),
                    &m.expected
                );
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                println!(
                    "  {} {} failed: {}",
                    style("✗").red(),
                    &m.name,
                    stderr.trim()
                );
            }
            Err(e) => {
                println!(
                    "  {} {} failed: {}",
                    style("✗").red(),
                    &m.name,
                    e
                );
            }
        }
    }

    println!();
}

/// Discover unique SSH hosts from the .meta config in the current directory.
/// Falls back to ["github.com"] if no .meta config is found or no SSH URLs exist.
fn discover_ssh_hosts(cwd: &Path) -> Vec<String> {
    let Some((config_path, _format)) = meta_cli::config::find_meta_config(cwd, None) else {
        return vec!["github.com".to_string()];
    };

    let Ok((projects, _ignore)) = meta_cli::config::parse_meta_config(&config_path) else {
        return vec!["github.com".to_string()];
    };

    let hosts: BTreeSet<String> = projects
        .iter()
        .filter_map(|p| meta_git_lib::extract_ssh_host(&p.repo))
        .collect();

    if hosts.is_empty() {
        vec!["github.com".to_string()]
    } else {
        hosts.into_iter().collect()
    }
}
