use crate::clone_queue::clone_with_queue;
use crate::clone_queue::CloneQueue;
use console::style;
use indicatif::MultiProgress;
use meta_cli::config;
use meta_plugin_protocol::CommandResult;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) fn execute_git_update(
    projects: &[String],
    dry_run: bool,
    cwd: &std::path::Path,
) -> anyhow::Result<CommandResult> {
    // Determine if we're in recursive mode (projects list provided by meta_cli)
    let recursive = !projects.is_empty();

    // Build list of directories to check for .meta files
    let dirs_to_check: Vec<PathBuf> = if recursive {
        // In recursive mode, check each directory that has a .meta file
        projects
            .iter()
            .map(|p| {
                if p == "." {
                    cwd.to_path_buf()
                } else {
                    cwd.join(p)
                }
            })
            .filter(|path| config::find_meta_config_in(path).is_some())
            .collect()
    } else {
        // Normal mode - just check current directory
        vec![cwd.to_path_buf()]
    };

    // First pass: check for orphaned repos and warn user
    for dir in &dirs_to_check {
        let Some((meta_path, _format)) = config::find_meta_config_in(dir) else {
            continue;
        };

        let (projects, _) = match config::parse_meta_config(&meta_path) {
            Ok(result) => result,
            Err(_) => continue,
        };

        // Check for orphaned repositories (exist locally but not in .meta)
        let config_projects: HashSet<String> = projects.iter().map(|p| p.path.clone()).collect();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    // Check if it's a git repo and not in config
                    if path.join(".git").exists()
                        && !name.starts_with('.')
                        && !config_projects.contains(name)
                    {
                        let relative_path = if dir.as_path() == cwd {
                            name.to_string()
                        } else {
                            dir.join(name).to_string_lossy().to_string()
                        };
                        eprintln!(
                            "{} {} exists locally but is not in .meta. To remove: rm -rf {}",
                            style("⚠").yellow(),
                            style(&relative_path).yellow().bold(),
                            relative_path
                        );
                    }
                }
            }
        }
    }

    // Create the clone queue - unlimited depth for recursive mode
    let meta_depth = if recursive { None } else { Some(0) };
    let queue = Arc::new(CloneQueue::new(None, meta_depth)); // No git depth for update

    // Seed the queue from all known .meta files
    for dir in &dirs_to_check {
        // Determine relative depth based on whether it's the cwd or nested
        let depth_level = if dir.as_path() == cwd { 0 } else { 1 };
        queue.push_from_meta(dir, depth_level)?;
    }

    let (_, initial_count) = queue.get_counts();

    if initial_count == 0 {
        println!("All repositories are already cloned.");
        return Ok(CommandResult::Message(String::new()));
    }

    if dry_run {
        println!(
            "{} Would clone {} missing repositories:",
            style("[DRY RUN]").cyan(),
            initial_count
        );
        let tasks = queue.drain_all();
        for task in tasks {
            println!("  git clone {} {}", task.url, task.target_path.display());
        }
        return Ok(CommandResult::Message(String::new()));
    }

    println!(
        "Cloning {} missing repositories{}",
        initial_count,
        if recursive { " (recursive mode)" } else { "" }
    );

    let mp = MultiProgress::new();

    // Use the queue-based cloning system (with parallelism of 4 to avoid SSH issues)
    clone_with_queue(Arc::clone(&queue), 4, &mp)?;

    let (completed, total) = queue.get_counts();
    if total > initial_count {
        println!(
            "Update completed ({} repos cloned, {} discovered via nested .meta files)",
            completed,
            total - initial_count
        );
    } else {
        println!("Update completed ({completed} repos cloned)");
    }

    Ok(CommandResult::Message(String::new()))
}
