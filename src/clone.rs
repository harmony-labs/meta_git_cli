use crate::clone_queue::CloneQueue;
use crate::clone_queue::clone_with_queue;
use console::style;
use indicatif::MultiProgress;
use meta_plugin_protocol::CommandResult;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

pub(crate) fn execute_git_clone(args: &[String]) -> anyhow::Result<CommandResult> {
    // Check for dry-run mode
    let dry_run = std::env::var("META_DRY_RUN").is_ok();

    // Default options - limit to 4 concurrent clones to avoid SSH multiplexing issues
    let mut recursive = false;
    let mut parallel = 4_usize;
    let mut depth: Option<String> = None;
    let mut meta_depth: Option<usize> = None; // Limit recursion depth for nested .meta files

    let mut url = String::new();
    let mut dir_arg: Option<String> = None;
    let mut idx = 0;
    let mut git_clone_args: Vec<String> = Vec::new();

    while idx < args.len() {
        match args[idx].as_str() {
            "--recursive" | "-r" => {
                recursive = true;
                idx += 1;
            }
            "--meta-depth" => {
                if idx + 1 < args.len() {
                    meta_depth = args[idx + 1].parse().ok();
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            "--parallel" => {
                if idx + 1 < args.len() {
                    parallel = args[idx + 1].parse().unwrap_or(4);
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            "--depth" => {
                if idx + 1 < args.len() {
                    let d = args[idx + 1].clone();
                    depth = Some(d.clone());
                    git_clone_args.push("--depth".to_string());
                    git_clone_args.push(d);
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            s if s.starts_with('-') => {
                idx += 1; // skip unknown option
            }
            s => {
                if url.is_empty() {
                    url = s.to_string();
                } else if dir_arg.is_none() {
                    dir_arg = Some(s.to_string());
                }
                idx += 1;
            }
        }
    }

    if url.is_empty() {
        return Ok(CommandResult::Error("No repository URL provided".to_string()));
    }

    // Derive directory name
    let clone_dir = if let Some(ref dir) = dir_arg {
        dir.clone()
    } else {
        url.trim_end_matches(".git")
            .rsplit('/')
            .next()
            .unwrap_or("meta")
            .to_string()
    };

    // Build the git clone command string for display/dry-run
    let mut clone_cmd_str = "git clone".to_string();
    for arg in &git_clone_args {
        clone_cmd_str.push(' ');
        clone_cmd_str.push_str(arg);
    }
    clone_cmd_str.push(' ');
    clone_cmd_str.push_str(&url);
    clone_cmd_str.push(' ');
    clone_cmd_str.push_str(&clone_dir);

    if dry_run {
        // Output what we know - just the meta repo clone command
        // (Child repos are in .meta file which hasn't been cloned yet)
        println!("{} Would clone meta repository:", style("[DRY RUN]").cyan());
        println!("  {}", clone_cmd_str);
        return Ok(CommandResult::Message(String::new()));
    }

    println!("Cloning meta repository: {url}");
    let mut clone_cmd = Command::new("git");
    clone_cmd.arg("clone").args(&git_clone_args).arg(&url);
    if let Some(ref dir) = dir_arg {
        clone_cmd.arg(dir);
    }
    let status = clone_cmd.status()?;
    if !status.success() {
        return Ok(CommandResult::Error("Failed to clone meta repository".to_string()));
    }

    // Parse .meta file inside cloned repo
    let clone_dir_path = PathBuf::from(&clone_dir);
    let meta_path = clone_dir_path.join(".meta");
    if !meta_path.exists() {
        return Ok(CommandResult::Message("No .meta file found in cloned repository".to_string()));
    }

    // Create the clone queue with depth settings
    // For non-recursive mode, set meta_depth to 0 (only first level)
    let effective_meta_depth = if recursive { meta_depth } else { Some(0) };
    let queue = Arc::new(CloneQueue::new(depth.clone(), effective_meta_depth));

    // Seed the queue with first-level children
    let initial_count = queue.push_from_meta(&clone_dir_path, 0)?;

    if initial_count == 0 {
        return Ok(CommandResult::Message("No child repositories to clone".to_string()));
    }

    println!(
        "Cloning {} child repositories{}",
        initial_count,
        if recursive { " (recursive mode)" } else { "" }
    );

    let mp = MultiProgress::new();

    // Use the queue-based cloning system
    clone_with_queue(Arc::clone(&queue), parallel, &mp)?;

    let (completed, total) = queue.get_counts();
    if total > initial_count {
        println!(
            "Meta-repo clone completed ({} repos cloned, {} discovered via nested .meta files)",
            completed,
            total - initial_count
        );
    } else {
        println!("Meta-repo clone completed ({} repos cloned)", completed);
    }

    Ok(CommandResult::Message(String::new()))
}
