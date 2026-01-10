//! meta-git library
//!
//! Provides git operations optimized for meta repositories.

use chrono::Utc;
use console::style;
use dialoguer::Confirm;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::debug;
use meta_git_lib::snapshot::{self, RepoState, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ============================================================================
// Queue-based Recursive Cloning System
// ============================================================================

/// A clone task representing a single repository to clone
#[derive(Debug, Clone)]
struct CloneTask {
    /// Display name for progress output
    name: String,
    /// Git URL to clone from
    url: String,
    /// Target path to clone into
    target_path: PathBuf,
    /// Depth level (for display purposes)
    depth_level: usize,
}

/// Thread-safe queue for managing clone tasks with dynamic discovery
struct CloneQueue {
    /// Pending tasks to process
    pending: Mutex<Vec<CloneTask>>,
    /// Completed task paths (to avoid duplicates)
    completed: Mutex<HashSet<PathBuf>>,
    /// Failed task paths
    failed: Mutex<HashSet<PathBuf>>,
    /// Total tasks discovered (for progress display)
    total_discovered: AtomicUsize,
    /// Total tasks completed
    total_completed: AtomicUsize,
    /// Git depth argument (if any)
    git_depth: Option<String>,
    /// Max meta depth for recursion (None = unlimited)
    meta_depth: Option<usize>,
}

impl CloneQueue {
    fn new(git_depth: Option<String>, meta_depth: Option<usize>) -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            completed: Mutex::new(HashSet::new()),
            failed: Mutex::new(HashSet::new()),
            total_discovered: AtomicUsize::new(0),
            total_completed: AtomicUsize::new(0),
            git_depth,
            meta_depth,
        }
    }

    /// Add a task to the queue if not already completed or pending
    fn push(&self, task: CloneTask) -> bool {
        let path = task.target_path.clone();

        // Check if already completed
        {
            let completed = self.completed.lock().unwrap();
            if completed.contains(&path) {
                return false;
            }
        }

        // Add to pending
        {
            let mut pending = self.pending.lock().unwrap();
            // Check if already in pending
            if pending.iter().any(|t| t.target_path == path) {
                return false;
            }
            pending.push(task);
            self.total_discovered.fetch_add(1, Ordering::SeqCst);
        }

        true
    }

    /// Add multiple tasks from a .meta file
    fn push_from_meta(&self, base_dir: &Path, depth_level: usize) -> anyhow::Result<usize> {
        // Check meta depth limit
        if let Some(max_depth) = self.meta_depth {
            if depth_level > max_depth {
                return Ok(0);
            }
        }

        let meta_path = base_dir.join(".meta");
        if !meta_path.exists() {
            return Ok(0);
        }

        let meta_content = fs::read_to_string(&meta_path)?;
        let meta_config: MetaConfig = serde_json::from_str(&meta_content)?;

        let mut added = 0;
        for (name, url) in meta_config.projects {
            let target_path = base_dir.join(&name);

            // Skip if already exists
            if target_path.exists() {
                // But still check if it has a .meta file for nested discovery
                let nested_meta = target_path.join(".meta");
                if nested_meta.exists() {
                    // Queue it for .meta discovery even though it's already cloned
                    added += self.push_from_meta(&target_path, depth_level + 1)?;
                }
                continue;
            }

            let task = CloneTask {
                name,
                url,
                target_path,
                depth_level,
            };

            if self.push(task) {
                added += 1;
            }
        }

        Ok(added)
    }

    /// Take a single task from the queue (for worker threads)
    fn take_one(&self) -> Option<CloneTask> {
        let mut pending = self.pending.lock().unwrap();
        pending.pop()
    }

    /// Check if queue is finished (no pending and no active workers)
    fn is_finished(&self, active_workers: &AtomicUsize) -> bool {
        let pending = self.pending.lock().unwrap();
        pending.is_empty() && active_workers.load(Ordering::SeqCst) == 0
    }

    /// Drain all pending tasks (for dry-run display)
    fn drain_all(&self) -> Vec<CloneTask> {
        let mut pending = self.pending.lock().unwrap();
        pending.drain(..).collect()
    }

    /// Get current counts for display
    fn get_counts(&self) -> (usize, usize) {
        (
            self.total_completed.load(Ordering::SeqCst),
            self.total_discovered.load(Ordering::SeqCst),
        )
    }

    /// Mark a task as completed and check for nested .meta files
    fn mark_completed(&self, task: &CloneTask) -> anyhow::Result<usize> {
        self.total_completed.fetch_add(1, Ordering::SeqCst);

        {
            let mut completed = self.completed.lock().unwrap();
            completed.insert(task.target_path.clone());
        }

        // Check for nested .meta file and add children to queue
        self.push_from_meta(&task.target_path, task.depth_level + 1)
    }

    /// Mark a task as failed
    fn mark_failed(&self, task: &CloneTask) {
        self.total_completed.fetch_add(1, Ordering::SeqCst);

        let mut failed = self.failed.lock().unwrap();
        failed.insert(task.target_path.clone());
    }

}

/// Clone repositories using a worker pool where each worker continuously pulls from the queue
fn clone_with_queue(
    queue: Arc<CloneQueue>,
    parallelism: usize,
    mp: &MultiProgress,
) -> anyhow::Result<()> {
    use std::sync::Condvar;

    let spinner_style = ProgressStyle::with_template("{prefix:.bold.dim} {spinner} {wide_msg}")
        .unwrap()
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

    // Track active workers for termination detection
    let active_workers = Arc::new(AtomicUsize::new(0));
    // Condition variable to signal when work might be available or workers finish
    let work_signal = Arc::new((Mutex::new(false), Condvar::new()));

    // Spawn worker threads
    let handles: Vec<_> = (0..parallelism)
        .map(|_worker_id| {
            let queue = Arc::clone(&queue);
            let active = Arc::clone(&active_workers);
            let signal = Arc::clone(&work_signal);
            let mp = mp.clone();
            let style = spinner_style.clone();

            std::thread::spawn(move || {
                loop {
                    // Try to get a task
                    let task = queue.take_one();

                    match task {
                        Some(task) => {
                            // Mark worker as active
                            active.fetch_add(1, Ordering::SeqCst);

                            // Create progress bar for this task
                            let (completed, total) = queue.get_counts();
                            let pb = mp.add(ProgressBar::new_spinner());
                            pb.set_style(style.clone());
                            pb.set_prefix(format!("[{}/{}]", completed + 1, total));
                            pb.set_message(format!("Cloning {}", task.name));
                            pb.enable_steady_tick(Duration::from_millis(100));

                            // Clone the repo (this may add new tasks to queue)
                            clone_single_repo(&task, &queue, &pb);

                            // Mark worker as inactive
                            active.fetch_sub(1, Ordering::SeqCst);

                            // Signal that we're done (might enable termination check)
                            let (lock, cvar) = &*signal;
                            let mut done = lock.lock().unwrap();
                            *done = true;
                            cvar.notify_all();
                        }
                        None => {
                            // No task available - check if we should terminate
                            if queue.is_finished(&active) {
                                break;
                            }

                            // Wait for signal that work might be available
                            let (lock, cvar) = &*signal;
                            let done = lock.lock().unwrap();
                            // Wait with timeout to periodically recheck
                            let _ = cvar.wait_timeout(done, Duration::from_millis(50));
                        }
                    }
                }
            })
        })
        .collect();

    // Wait for all workers to finish
    for handle in handles {
        handle.join().expect("Worker thread panicked");
    }

    Ok(())
}

/// Clone a single repository and handle .meta discovery
fn clone_single_repo(task: &CloneTask, queue: &Arc<CloneQueue>, pb: &ProgressBar) {
    // Skip if target exists
    if task.target_path.exists()
        && task
            .target_path
            .read_dir()
            .map(|mut iter| iter.next().is_some())
            .unwrap_or(false)
    {
        pb.finish_with_message(format!(
            "{}",
            style(format!("Skipped {} (exists)", task.name)).yellow()
        ));
        // Still mark as completed and check for nested .meta
        if let Err(e) = queue.mark_completed(task) {
            debug!("Failed to check nested .meta for {}: {}", task.name, e);
        }
        return;
    }

    // Build git clone command
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg(&task.url).arg(&task.target_path);
    if let Some(ref d) = queue.git_depth {
        cmd.arg("--depth").arg(d);
    }

    // Run clone
    match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            // Stream stderr for progress updates
            let stderr = child.stderr.take();
            let pb_clone = pb.clone();
            let task_name = task.name.clone();
            if let Some(stderr) = stderr {
                std::thread::spawn(move || {
                    use std::io::{BufRead, BufReader};
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        pb_clone.set_message(format!("{}: {}", task_name, line));
                    }
                });
            }

            match child.wait() {
                Ok(status) if status.success() => {
                    // Check for nested .meta and report new discoveries
                    match queue.mark_completed(task) {
                        Ok(added) if added > 0 => {
                            let (_, total) = queue.get_counts();
                            pb.finish_with_message(format!(
                                "{}",
                                style(format!("Cloned {} (+{} nested)", task.name, added)).green()
                            ));
                            // Update for new total
                            debug!("Discovered {} more repos in {}, total now {}", added, task.name, total);
                        }
                        Ok(_) => {
                            pb.finish_with_message(format!(
                                "{}",
                                style(format!("Cloned {}", task.name)).green()
                            ));
                        }
                        Err(e) => {
                            pb.finish_with_message(format!(
                                "{}",
                                style(format!("Cloned {} (meta parse error: {})", task.name, e)).yellow()
                            ));
                        }
                    }
                }
                _ => {
                    queue.mark_failed(task);
                    pb.finish_with_message(format!(
                        "{}",
                        style(format!("Failed to clone {}", task.name)).red()
                    ));
                }
            }
        }
        Err(_) => {
            queue.mark_failed(task);
            pb.finish_with_message(format!(
                "{}",
                style(format!("Failed to spawn git for {}", task.name)).red()
            ));
        }
    }
}

// ============================================================================
// Execution Plan types for plugin shim protocol
// ============================================================================

/// An execution plan that tells the shim what commands to run via loop_lib
#[derive(Debug, Serialize)]
struct ExecutionPlan {
    commands: Vec<PlannedCommand>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel: Option<bool>,
}

/// A single command in an execution plan
#[derive(Debug, Serialize)]
struct PlannedCommand {
    dir: String,
    cmd: String,
}

/// Response wrapper for execution plans
#[derive(Debug, Serialize)]
struct PlanResponse {
    plan: ExecutionPlan,
}

/// Output an execution plan to stdout for the shim to execute
fn output_execution_plan(commands: Vec<PlannedCommand>, parallel: Option<bool>) {
    let response = PlanResponse {
        plan: ExecutionPlan { commands, parallel },
    };
    println!("{}", serde_json::to_string(&response).unwrap());
}

/// Get all project directories from .meta config (including root ".")
/// Get project directories - uses passed-in list if non-empty, otherwise reads local .meta
fn get_project_directories_with_fallback(projects: &[String]) -> anyhow::Result<Vec<String>> {
    if !projects.is_empty() {
        // Use the projects list from meta_cli (supports --recursive)
        Ok(projects.to_vec())
    } else {
        // Fall back to reading local .meta file
        get_project_directories()
    }
}

fn get_project_directories() -> anyhow::Result<Vec<String>> {
    let cwd = std::env::current_dir()?;
    let meta_path = cwd.join(".meta");

    if !meta_path.exists() {
        return Ok(vec![".".to_string()]);
    }

    let meta_content = std::fs::read_to_string(&meta_path)?;
    let meta_config: MetaConfig = serde_json::from_str(&meta_content)?;

    // Start with root directory
    let mut dirs = vec![".".to_string()];

    // Add child projects (sorted for consistency)
    let mut projects: Vec<String> = meta_config.projects.keys().cloned().collect();
    projects.sort();
    dirs.extend(projects);

    Ok(dirs)
}

#[derive(Debug, Deserialize)]
struct MetaConfig {
    projects: HashMap<String, String>,
}

/// Execute a git command for meta repositories
///
/// The `projects` parameter is the list of project directories passed from meta_cli.
/// When meta_cli runs with `--recursive`, it discovers nested .meta files and passes
/// all project directories here. If `projects` is empty, we fall back to reading
/// the local .meta file via `get_project_directories()`.
pub fn execute_command(command: &str, args: &[String], projects: &[String]) -> anyhow::Result<()> {
    debug!("[meta_git_cli] Plugin invoked with command: '{command}'");
    debug!("[meta_git_cli] Args: {args:?}");
    debug!("[meta_git_cli] Projects from meta_cli: {projects:?}");

    match command {
        "git status" => execute_git_status(projects),
        "git clone" => execute_git_clone(args),
        "git update" => execute_git_update(projects),
        "git setup-ssh" => execute_git_setup_ssh(),
        "git commit" => execute_git_commit(args, projects),
        "git snapshot" => execute_snapshot_help(),
        "git snapshot create" => execute_snapshot_create(args, projects),
        "git snapshot list" => execute_snapshot_list(),
        "git snapshot show" => execute_snapshot_show(args),
        "git snapshot restore" => execute_snapshot_restore(args, projects),
        "git snapshot delete" => execute_snapshot_delete(args),
        _ => Err(anyhow::anyhow!("Unknown command: {}", command)),
    }
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

fn execute_git_status(projects: &[String]) -> anyhow::Result<()> {
    // Return an execution plan - let loop_lib handle execution, dry-run, and JSON output
    // Use projects from meta_cli if available (enables --recursive), otherwise read local .meta
    let dirs = get_project_directories_with_fallback(projects)?;

    let commands: Vec<PlannedCommand> = dirs
        .into_iter()
        .map(|dir| PlannedCommand {
            dir,
            cmd: "git status".to_string(),
        })
        .collect();

    output_execution_plan(commands, Some(false)); // Sequential for status to keep output readable
    Ok(())
}

fn execute_git_clone(args: &[String]) -> anyhow::Result<()> {
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
        println!("Error: No repository URL provided");
        return Ok(());
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
        return Ok(());
    }

    println!("Cloning meta repository: {url}");
    let mut clone_cmd = Command::new("git");
    clone_cmd.arg("clone").args(&git_clone_args).arg(&url);
    if let Some(ref dir) = dir_arg {
        clone_cmd.arg(dir);
    }
    let status = clone_cmd.status()?;
    if !status.success() {
        println!("Failed to clone meta repository");
        return Ok(());
    }

    // Parse .meta file inside cloned repo
    let clone_dir_path = PathBuf::from(&clone_dir);
    let meta_path = clone_dir_path.join(".meta");
    if !meta_path.exists() {
        println!("No .meta file found in cloned repository");
        return Ok(());
    }

    // Create the clone queue with depth settings
    // For non-recursive mode, set meta_depth to 0 (only first level)
    let effective_meta_depth = if recursive { meta_depth } else { Some(0) };
    let queue = Arc::new(CloneQueue::new(depth.clone(), effective_meta_depth));

    // Seed the queue with first-level children
    let initial_count = queue.push_from_meta(&clone_dir_path, 0)?;

    if initial_count == 0 {
        println!("No child repositories to clone");
        return Ok(());
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

    Ok(())
}

fn execute_git_update(projects: &[String]) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;

    // Check for dry-run mode
    let dry_run = std::env::var("META_DRY_RUN").is_ok();

    // Determine if we're in recursive mode (projects list provided by meta_cli)
    let recursive = !projects.is_empty();

    // Build list of directories to check for .meta files
    let dirs_to_check: Vec<PathBuf> = if recursive {
        // In recursive mode, check each directory that has a .meta file
        projects
            .iter()
            .map(|p| {
                if p == "." {
                    cwd.clone()
                } else {
                    cwd.join(p)
                }
            })
            .filter(|path| path.join(".meta").exists())
            .collect()
    } else {
        // Normal mode - just check current directory
        vec![cwd.clone()]
    };

    // First pass: check for orphaned repos and warn user
    for dir in &dirs_to_check {
        let meta_path = dir.join(".meta");
        if !meta_path.exists() {
            continue;
        }

        let meta_content = match std::fs::read_to_string(&meta_path) {
            Ok(content) => content,
            Err(_) => continue,
        };

        let meta_config: MetaConfig = match serde_json::from_str(&meta_content) {
            Ok(config) => config,
            Err(_) => continue,
        };

        // Check for orphaned repositories (exist locally but not in .meta)
        let config_projects: HashSet<_> = meta_config.projects.keys().collect();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    // Check if it's a git repo and not in config
                    if path.join(".git").exists()
                        && !name.starts_with('.')
                        && !config_projects.contains(&name.to_string())
                    {
                        let relative_path = if dir == &cwd {
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
        let depth_level = if dir == &cwd { 0 } else { 1 };
        queue.push_from_meta(dir, depth_level)?;
    }

    let (_, initial_count) = queue.get_counts();

    if initial_count == 0 {
        println!("All repositories are already cloned.");
        return Ok(());
    }

    if dry_run {
        println!("{} Would clone {} missing repositories:", style("[DRY RUN]").cyan(), initial_count);
        let tasks = queue.drain_all();
        for task in tasks {
            println!("  git clone {} {}", task.url, task.target_path.display());
        }
        return Ok(());
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
        println!("Update completed ({} repos cloned)", completed);
    }

    Ok(())
}

fn execute_git_setup_ssh() -> anyhow::Result<()> {
    if meta_git_lib::is_multiplexing_configured() {
        println!(
            "{} SSH multiplexing is already configured.",
            style("✓").green()
        );
        println!("  Your parallel git operations should work efficiently.");
    } else {
        match meta_git_lib::prompt_and_setup_multiplexing() {
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
                println!(
                    "{} Failed to set up SSH multiplexing: {}",
                    style("✗").red(),
                    e
                );
            }
        }
    }
    Ok(())
}

/// Execute git commit with optional --edit flag for per-repo messages
fn execute_git_commit(args: &[String], projects: &[String]) -> anyhow::Result<()> {
    // Parse arguments
    let mut use_editor = false;
    let mut message: Option<String> = None;
    let mut idx = 0;

    while idx < args.len() {
        match args[idx].as_str() {
            "--edit" | "-e" => {
                use_editor = true;
                idx += 1;
            }
            "-m" | "--message" => {
                if idx + 1 < args.len() {
                    message = Some(args[idx + 1].clone());
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            // Skip other args like "git", "commit"
            _ => idx += 1,
        }
    }

    let cwd = std::env::current_dir()?;

    // Get list of directories to check for staged changes
    let dirs_to_check: Vec<String> = if !projects.is_empty() {
        // Use projects from meta_cli (supports --recursive)
        projects.to_vec()
    } else {
        // Fall back to reading local .meta file
        let meta_path = cwd.join(".meta");
        if !meta_path.exists() {
            println!("No .meta file found in {}", cwd.display());
            return Ok(());
        }
        let meta_content = std::fs::read_to_string(&meta_path)?;
        let meta_config: MetaConfig = serde_json::from_str(&meta_content)?;

        let mut dirs = vec![".".to_string()];
        dirs.extend(meta_config.projects.keys().cloned());
        dirs
    };

    // Find repos with staged changes
    let mut repos_with_changes: Vec<(String, String, Vec<String>)> = Vec::new();

    for dir in &dirs_to_check {
        let path = if dir == "." {
            cwd.clone()
        } else {
            cwd.join(dir)
        };

        if path.exists() && has_staged_changes(&path.to_string_lossy()) {
            let staged = get_staged_files(&path.to_string_lossy());
            repos_with_changes.push((
                dir.clone(),
                path.to_string_lossy().to_string(),
                staged,
            ));
        }
    }

    if repos_with_changes.is_empty() {
        println!("No staged changes found in any repository.");
        return Ok(());
    }

    if use_editor {
        // Open editor for per-repo messages (interactive, cannot use ExecutionPlan)
        execute_editor_commit(&repos_with_changes)?;
    } else if let Some(msg) = message {
        // Apply same message to all repos - use ExecutionPlan for proper dry-run support
        // Escape the message for shell (replace single quotes)
        let escaped_msg = msg.replace('\'', "'\\''");
        let commands: Vec<PlannedCommand> = repos_with_changes
            .iter()
            .map(|(name, path, _files)| {
                // For "." use "." as dir, otherwise use the path
                let dir = if name == "." {
                    ".".to_string()
                } else {
                    path.clone()
                };
                PlannedCommand {
                    dir,
                    cmd: format!("git commit -m '{}'", escaped_msg),
                }
            })
            .collect();

        output_execution_plan(commands, Some(false)); // Sequential for commit
        return Ok(());
    } else {
        // No message provided, show what would be committed
        println!("Repositories with staged changes:");
        for (name, _path, files) in &repos_with_changes {
            println!("  {} ({} files)", style(name).cyan(), files.len());
        }
        println!();
        println!(
            "Use {} to create per-repo commit messages",
            style("--edit").yellow()
        );
        println!(
            "Use {} to apply the same message to all",
            style("-m \"message\"").yellow()
        );
    }

    Ok(())
}

/// Check if a repo has staged changes
fn has_staged_changes(path: &str) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["diff", "--cached", "--quiet"])
        .status();

    match output {
        Ok(status) => !status.success(), // Non-zero exit means there are changes
        Err(_) => false,
    }
}

/// Get list of staged files in a repo
fn get_staged_files(path: &str) -> Vec<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["diff", "--cached", "--name-only"])
        .output();

    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(String::from)
            .collect(),
        _ => vec![],
    }
}

/// Execute commit with editor for per-repo messages
fn execute_editor_commit(repos: &[(String, String, Vec<String>)]) -> anyhow::Result<()> {
    use std::io::Write;

    // Create temp file with commit template
    let mut template = String::new();
    template.push_str("# Meta Multi-Commit\n");
    template.push_str("# Each section represents one repository.\n");
    template.push_str("# Edit the message below each header.\n");
    template.push_str("# Delete a section entirely or leave message empty to skip that repo.\n");
    template.push_str("#\n\n");

    for (name, _path, files) in repos {
        template.push_str(&format!("========== {name} ==========\n"));
        let file_count = files.len();
        let file_list = files.join(", ");
        template.push_str(&format!("# {file_count} file(s) staged: {file_list}\n"));
        template.push('\n');
        template.push_str("# Enter commit message above this line\n\n");
    }

    // Write to temp file
    let temp_dir = std::env::temp_dir();
    let temp_file = temp_dir.join("META_COMMIT_EDITMSG");
    let mut file = std::fs::File::create(&temp_file)?;
    file.write_all(template.as_bytes())?;
    drop(file);

    // Get editor from environment
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());

    // Open editor
    let status = Command::new(&editor).arg(&temp_file).status()?;

    if !status.success() {
        anyhow::bail!("Editor exited with non-zero status");
    }

    // Read and parse the edited file
    let content = std::fs::read_to_string(&temp_file)?;
    let commits = parse_multi_commit_file(&content);

    // Clean up temp file
    let _ = std::fs::remove_file(&temp_file);

    if commits.is_empty() {
        println!("No commits to make (all messages were empty or deleted).");
        return Ok(());
    }

    // Execute commits
    let mut succeeded = 0;
    let mut failed = 0;

    for (repo_name, message) in &commits {
        // Find the path for this repo
        let path = repos
            .iter()
            .find(|(name, _, _)| name == repo_name)
            .map(|(_, path, _)| path.as_str())
            .unwrap_or(repo_name);

        println!(
            "{} Committing {}...",
            style("→").cyan(),
            style(repo_name).bold()
        );

        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("commit")
            .arg("-m")
            .arg(message)
            .status();

        match status {
            Ok(s) if s.success() => {
                println!(
                    "  {} {}",
                    style("✓").green(),
                    message.lines().next().unwrap_or("")
                );
                succeeded += 1;
            }
            _ => {
                println!("  {} Failed to commit", style("✗").red());
                failed += 1;
            }
        }
    }

    println!();
    if failed > 0 {
        println!(
            "Committed {} repo(s), {} failed",
            style(succeeded).green(),
            style(failed).red()
        );
    } else {
        println!("Committed {} repo(s)", style(succeeded).green());
    }

    Ok(())
}

/// Parse the multi-commit file content
fn parse_multi_commit_file(content: &str) -> Vec<(String, String)> {
    let mut commits = Vec::new();
    let mut current_repo: Option<String> = None;
    let mut current_message = String::new();

    for line in content.lines() {
        if line.starts_with("==========") && line.ends_with("==========") {
            // Save previous repo if it had a message
            if let Some(repo) = current_repo.take() {
                let msg = current_message.trim().to_string();
                if !msg.is_empty() {
                    commits.push((repo, msg));
                }
            }
            // Parse new repo name
            let repo = line
                .trim_start_matches('=')
                .trim_end_matches('=')
                .trim()
                .to_string();
            current_repo = Some(repo);
            current_message.clear();
        } else if !line.starts_with('#') && current_repo.is_some() {
            // Add non-comment lines to current message
            current_message.push_str(line);
            current_message.push('\n');
        }
    }

    // Don't forget the last repo
    if let Some(repo) = current_repo {
        let msg = current_message.trim().to_string();
        if !msg.is_empty() {
            commits.push((repo, msg));
        }
    }

    commits
}

// ============================================================================
// Snapshot Commands
// ============================================================================

/// Show snapshot help text
fn execute_snapshot_help() -> anyhow::Result<()> {
    println!(
        r#"{}

{}

Usage: meta git snapshot <command> [args]

Commands:
  {}      Create a snapshot of all repos' git state
  {}        List all available snapshots
  {}        Show details of a snapshot
  {}     Restore all repos to a snapshot state
  {}      Delete a snapshot

Examples:
  meta git snapshot create before-upgrade
  meta git snapshot list
  meta git snapshot show before-upgrade
  meta git snapshot restore before-upgrade --dry-run
  meta git snapshot restore before-upgrade --force
  meta git snapshot delete before-upgrade

Snapshots capture the entire workspace state (recursive by default).
Use --force to skip confirmation on restore, --dry-run to preview."#,
        style("meta git snapshot - Workspace State Management").bold(),
        style("[EXPERIMENTAL] File format is subject to change.").yellow(),
        style("create <name>").cyan(),
        style("list").cyan(),
        style("show <name>").cyan(),
        style("restore <name>").cyan(),
        style("delete <name>").cyan(),
    );
    Ok(())
}

/// Create a snapshot of the current workspace state
fn execute_snapshot_create(args: &[String], projects: &[String]) -> anyhow::Result<()> {
    // Parse snapshot name from args
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("Usage: meta git snapshot create <name>"))?;

    let cwd = std::env::current_dir()?;

    // Get all repos (recursive by default)
    let dirs = get_all_repo_directories(projects)?;

    println!(
        "Creating snapshot '{}' of {} repos...",
        style(name).cyan(),
        dirs.len()
    );

    let mut repos = HashMap::new();
    let mut dirty_count = 0;

    for dir in &dirs {
        let path = if dir == "." {
            cwd.clone()
        } else {
            cwd.join(dir)
        };

        if !path.exists() || !snapshot::is_git_repo(&path) {
            println!(
                "  {} {} (not a git repo, skipping)",
                style("⚠").yellow(),
                dir
            );
            continue;
        }

        match snapshot::capture_repo_state(&path) {
            Ok(state) => {
                if state.dirty {
                    dirty_count += 1;
                    println!(
                        "  {} {} (dirty)",
                        style("○").yellow(),
                        dir
                    );
                } else {
                    println!("  {} {}", style("✓").green(), dir);
                }
                repos.insert(dir.clone(), state);
            }
            Err(e) => {
                println!(
                    "  {} {} (error: {})",
                    style("✗").red(),
                    dir,
                    e
                );
            }
        }
    }

    if repos.is_empty() {
        anyhow::bail!("No repos captured");
    }

    let snap = Snapshot {
        name: name.clone(),
        created: Utc::now(),
        repos,
    };

    snapshot::save_snapshot(&cwd, &snap)?;

    println!();
    println!(
        "{} Captured state of {} repos",
        style("✓").green(),
        snap.repos.len()
    );
    if dirty_count > 0 {
        println!(
            "{} {} repo(s) have uncommitted changes (recorded as dirty)",
            style("⚠").yellow(),
            dirty_count
        );
    }
    println!(
        "Snapshot saved: {}",
        style(format!(".meta-snapshots/{}.json", name)).dim()
    );

    Ok(())
}

/// List all snapshots
fn execute_snapshot_list() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let snapshots = snapshot::list_snapshots(&cwd)?;

    if snapshots.is_empty() {
        println!("No snapshots found.");
        println!(
            "Create one with: {}",
            style("meta git snapshot create <name>").cyan()
        );
        return Ok(());
    }

    println!("Snapshots:\n");
    for info in snapshots {
        let dirty_note = if info.dirty_count > 0 {
            format!(" ({} dirty)", info.dirty_count)
        } else {
            String::new()
        };
        println!(
            "  {} - {} repos{} - {}",
            style(&info.name).cyan().bold(),
            info.repo_count,
            style(dirty_note).yellow(),
            style(info.created.format("%Y-%m-%d %H:%M:%S")).dim()
        );
    }
    println!();

    Ok(())
}

/// Show details of a snapshot
fn execute_snapshot_show(args: &[String]) -> anyhow::Result<()> {
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("Usage: meta git snapshot show <name>"))?;

    let cwd = std::env::current_dir()?;
    let snap = snapshot::load_snapshot(&cwd, name)?;

    println!("Snapshot: {}", style(&snap.name).cyan().bold());
    println!("Created:  {}", snap.created.format("%Y-%m-%d %H:%M:%S UTC"));
    println!("Repos:    {}", snap.repos.len());
    println!();

    // Sort repos by name for consistent output
    let mut repos: Vec<_> = snap.repos.iter().collect();
    repos.sort_by(|a, b| a.0.cmp(b.0));

    for (name, state) in repos {
        let branch_info = state
            .branch
            .as_ref()
            .map(|b| format!(" -> {}", b))
            .unwrap_or_else(|| " (detached)".to_string());

        let dirty_marker = if state.dirty {
            format!(" {}", style("(dirty)").yellow())
        } else {
            String::new()
        };

        println!(
            "  {} {}{}{}",
            style(&state.sha[..8]).dim(),
            name,
            style(branch_info).cyan(),
            dirty_marker
        );
    }

    Ok(())
}

/// Restore workspace to a snapshot state
fn execute_snapshot_restore(args: &[String], _projects: &[String]) -> anyhow::Result<()> {
    // Parse args
    let mut name: Option<&str> = None;
    let mut force = false;
    let mut dry_run = std::env::var("META_DRY_RUN").is_ok();

    for arg in args {
        match arg.as_str() {
            "--force" | "-f" => force = true,
            "--dry-run" => dry_run = true,
            s if !s.starts_with('-') => name = Some(s),
            _ => {}
        }
    }

    let name = name.ok_or_else(|| anyhow::anyhow!("Usage: meta git snapshot restore <name> [--force] [--dry-run]"))?;

    let cwd = std::env::current_dir()?;
    let snap = snapshot::load_snapshot(&cwd, name)?;

    // Analyze what would change
    let mut repos_to_restore: Vec<(&str, &RepoState, bool)> = Vec::new();
    let mut missing_repos = Vec::new();

    for (repo_name, state) in &snap.repos {
        let path = if repo_name == "." {
            cwd.clone()
        } else {
            cwd.join(repo_name)
        };

        if !path.exists() || !snapshot::is_git_repo(&path) {
            missing_repos.push(repo_name.as_str());
            continue;
        }

        // Check if current state is dirty
        let current_state = snapshot::capture_repo_state(&path)?;
        repos_to_restore.push((repo_name, state, current_state.dirty));
    }

    let dirty_count = repos_to_restore.iter().filter(|(_, _, d)| *d).count();

    // Show preview
    println!(
        "Restore {} repos to snapshot '{}':",
        repos_to_restore.len(),
        style(name).cyan()
    );
    println!(
        "  - {} repos will checkout to their recorded SHA",
        repos_to_restore.len() - dirty_count
    );
    if dirty_count > 0 {
        println!(
            "  - {} repos have uncommitted changes (will be stashed)",
            style(dirty_count).yellow()
        );
    }
    if !missing_repos.is_empty() {
        println!(
            "  - {} repos missing (will be skipped): {}",
            style(missing_repos.len()).red(),
            missing_repos.join(", ")
        );
    }
    println!();

    if dry_run {
        println!("{} Dry run - no changes made", style("[DRY RUN]").cyan());
        return Ok(());
    }

    // Confirm unless --force
    if !force {
        let proceed = Confirm::new()
            .with_prompt("Proceed?")
            .default(false)
            .interact()?;

        if !proceed {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Execute restore
    println!("Restoring...");
    let mut success_count = 0;
    let mut fail_count = 0;

    for (repo_name, state, _is_dirty) in &repos_to_restore {
        let path = if *repo_name == "." {
            cwd.clone()
        } else {
            cwd.join(repo_name)
        };

        let result = snapshot::restore_repo_state(&path, state, force)?;

        if result.success {
            let stash_note = if result.stashed {
                format!(" {}", style("(stashed changes)").yellow())
            } else {
                String::new()
            };
            println!(
                "  {} {} {}{}",
                style("✓").green(),
                repo_name,
                result.message,
                stash_note
            );
            success_count += 1;
        } else {
            println!(
                "  {} {} {}",
                style("✗").red(),
                repo_name,
                result.message
            );
            fail_count += 1;
        }
    }

    println!();
    if fail_count > 0 {
        println!(
            "Restored {} repo(s), {} failed",
            style(success_count).green(),
            style(fail_count).red()
        );
    } else {
        println!(
            "{} Restored {} repo(s)",
            style("✓").green(),
            success_count
        );
    }

    Ok(())
}

/// Delete a snapshot
fn execute_snapshot_delete(args: &[String]) -> anyhow::Result<()> {
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("Usage: meta git snapshot delete <name>"))?;

    let cwd = std::env::current_dir()?;

    snapshot::delete_snapshot(&cwd, name)?;

    println!(
        "{} Deleted snapshot '{}'",
        style("✓").green(),
        style(name).cyan()
    );

    Ok(())
}

/// Get all repository directories for snapshot operations (recursive by default)
fn get_all_repo_directories(projects: &[String]) -> anyhow::Result<Vec<String>> {
    if !projects.is_empty() {
        // Use projects from meta_cli (supports --recursive which is already the default behavior)
        return Ok(projects.to_vec());
    }

    // Fall back to reading local .meta file and discovering nested repos
    let cwd = std::env::current_dir()?;
    let mut dirs = vec![".".to_string()];

    // Recursively discover all repos
    discover_repos_recursive(&cwd, &cwd, &mut dirs)?;

    Ok(dirs)
}

/// Recursively discover repos by looking for .meta files
fn discover_repos_recursive(
    base: &Path,
    current: &Path,
    dirs: &mut Vec<String>,
) -> anyhow::Result<()> {
    let meta_path = current.join(".meta");
    if !meta_path.exists() {
        return Ok(());
    }

    let meta_content = std::fs::read_to_string(&meta_path)?;
    let meta_config: MetaConfig = serde_json::from_str(&meta_content)?;

    for name in meta_config.projects.keys() {
        let project_path = current.join(name);
        let relative_path = project_path
            .strip_prefix(base)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| name.clone());

        if project_path.exists() {
            dirs.push(relative_path);
            // Check for nested .meta
            discover_repos_recursive(base, &project_path, dirs)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

        // Should succeed (prints message and returns Ok)
        assert!(result.is_ok());
    }

    #[test]
    fn test_meta_config_parsing() {
        let json = r#"{"projects": {"foo": "git@github.com:org/foo.git", "bar": "git@github.com:org/bar.git"}}"#;
        let config: MetaConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.projects.len(), 2);
        assert!(config.projects.contains_key("foo"));
        assert!(config.projects.contains_key("bar"));
    }

    #[test]
    fn test_unknown_command() {
        let result = execute_command("git unknown", &[], &[]);
        assert!(result.is_err());
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
