use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::debug;
use meta_git_lib::clone_queue::{CloneQueue, CloneTask};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
                    // Mark worker as active BEFORE taking a task to prevent
                    // a race where is_finished() sees pending=empty, active=0
                    // while a worker is between take_one() and starting work.
                    active.fetch_add(1, Ordering::SeqCst);

                    // Try to get a task
                    let task = queue.take_one();

                    match task {
                        Some(task) => {
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
                            // No task available - mark worker as inactive
                            active.fetch_sub(1, Ordering::SeqCst);

                            // Check if we should terminate
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
    if let Some(d) = queue.git_depth() {
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
