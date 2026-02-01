use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::debug;
use meta_cli::config;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A clone task representing a single repository to clone
#[derive(Debug, Clone)]
pub(crate) struct CloneTask {
    /// Display name for progress output
    pub name: String,
    /// Git URL to clone from
    pub url: String,
    /// Target path to clone into
    pub target_path: PathBuf,
    /// Depth level (for display purposes)
    pub depth_level: usize,
}

/// Thread-safe queue for managing clone tasks with dynamic discovery
pub(crate) struct CloneQueue {
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
    pub fn new(git_depth: Option<String>, meta_depth: Option<usize>) -> Self {
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
    pub fn push(&self, task: CloneTask) -> bool {
        let path = task.target_path.clone();

        // Check if already completed
        {
            let completed = self.completed.lock().unwrap_or_else(|e| e.into_inner());
            if completed.contains(&path) {
                return false;
            }
        }

        // Add to pending
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
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
    pub fn push_from_meta(&self, base_dir: &Path, depth_level: usize) -> anyhow::Result<usize> {
        // Check meta depth limit
        if let Some(max_depth) = self.meta_depth {
            if depth_level > max_depth {
                return Ok(0);
            }
        }

        let Some((meta_path, _format)) = config::find_meta_config_in(base_dir) else {
            return Ok(0);
        };

        let (projects, _) = config::parse_meta_config(&meta_path)?;

        let mut added = 0;
        for project in projects {
            let target_path = base_dir.join(&project.path);

            // Skip if already exists
            if target_path.exists() {
                // But still check if it has a config file for nested discovery
                if config::find_meta_config_in(&target_path).is_some() {
                    // Queue it for discovery even though it's already cloned
                    added += self.push_from_meta(&target_path, depth_level + 1)?;
                }
                continue;
            }

            let task = CloneTask {
                name: project.name,
                url: project.repo,
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
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.pop()
    }

    /// Check if queue is finished (no pending and no active workers)
    fn is_finished(&self, active_workers: &AtomicUsize) -> bool {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.is_empty() && active_workers.load(Ordering::SeqCst) == 0
    }

    /// Drain all pending tasks (for dry-run display)
    pub fn drain_all(&self) -> Vec<CloneTask> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.drain(..).collect()
    }

    /// Get current counts for display
    pub fn get_counts(&self) -> (usize, usize) {
        (
            self.total_completed.load(Ordering::SeqCst),
            self.total_discovered.load(Ordering::SeqCst),
        )
    }

    /// Mark a task as completed and check for nested .meta files
    fn mark_completed(&self, task: &CloneTask) -> anyhow::Result<usize> {
        self.total_completed.fetch_add(1, Ordering::SeqCst);

        {
            let mut completed = self.completed.lock().unwrap_or_else(|e| e.into_inner());
            completed.insert(task.target_path.clone());
        }

        // Check for nested .meta file and add children to queue
        self.push_from_meta(&task.target_path, task.depth_level + 1)
    }

    /// Mark a task as failed
    fn mark_failed(&self, task: &CloneTask) {
        self.total_completed.fetch_add(1, Ordering::SeqCst);

        let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        failed.insert(task.target_path.clone());
    }
}

/// Clone repositories using a worker pool where each worker continuously pulls from the queue
pub(crate) fn clone_with_queue(
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
                            let mut done = lock.lock().unwrap_or_else(|e| e.into_inner());
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
                            let done = lock.lock().unwrap_or_else(|e| e.into_inner());
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
                        pb_clone.set_message(format!("{task_name}: {line}"));
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
                            debug!(
                                "Discovered {} more repos in {}, total now {}",
                                added, task.name, total
                            );
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
                                style(format!("Cloned {} (meta parse error: {})", task.name, e))
                                    .yellow()
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
