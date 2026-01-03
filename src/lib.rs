//! meta-git library
//!
//! Provides git operations optimized for meta repositories.

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::debug;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

/// Check if JSON output mode is enabled
fn is_json_output() -> bool {
    std::env::var("META_JSON_OUTPUT")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// JSON output schema for meta commands
#[derive(Debug, Serialize)]
struct JsonOutput {
    version: &'static str,
    command: String,
    timestamp: String,
    results: Vec<ProjectResult>,
    summary: OutputSummary,
}

#[derive(Debug, Serialize)]
struct ProjectResult {
    project: String,
    path: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct OutputSummary {
    total: usize,
    succeeded: usize,
    failed: usize,
}

#[derive(Debug, Deserialize)]
struct MetaConfig {
    projects: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct ProjectEntry {
    name: String,
    path: String,
    repo: String,
}

/// Execute a git command for meta repositories
pub fn execute_command(command: &str, args: &[String]) -> anyhow::Result<()> {
    debug!("[meta_git_cli] Plugin invoked with command: '{command}'");
    debug!("[meta_git_cli] Args: {args:?}");

    match command {
        "git status" => execute_git_status(),
        "git clone" => execute_git_clone(args),
        "git update" => execute_git_update(),
        "git setup-ssh" => execute_git_setup_ssh(),
        "git commit" => execute_git_commit(args),
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
"#
}

fn execute_git_status() -> anyhow::Result<()> {
    let json_mode = is_json_output();
    if std::env::var("META_DEBUG").is_ok() {
        eprintln!(
            "[meta_git_cli] json_mode = {}, META_JSON_OUTPUT = {:?}",
            json_mode,
            std::env::var("META_JSON_OUTPUT")
        );
    }

    // Load meta config
    let cwd = std::env::current_dir()?;
    let meta_path = cwd.join(".meta");
    if !meta_path.exists() {
        if json_mode {
            let output = JsonOutput {
                version: "1.0",
                command: "git status".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                results: vec![],
                summary: OutputSummary {
                    total: 0,
                    succeeded: 0,
                    failed: 0,
                },
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("No .meta file found in {}", cwd.display());
        }
        return Ok(());
    }
    let meta_content = std::fs::read_to_string(meta_path)?;
    let meta_config: MetaConfig = serde_json::from_str(&meta_content)?;

    // Start with the root directory
    let mut projects: Vec<ProjectEntry> = vec![ProjectEntry {
        name: ".".to_string(),
        path: ".".to_string(),
        repo: String::new(),
    }];

    // Add child projects
    let mut child_projects: Vec<ProjectEntry> = meta_config
        .projects
        .into_iter()
        .map(|(path, repo)| ProjectEntry {
            name: path.clone(),
            path,
            repo,
        })
        .collect();
    child_projects.sort_by(|a, b| a.name.cmp(&b.name));
    projects.extend(child_projects);

    let mut results: Vec<ProjectResult> = Vec::new();
    let mut failed = 0;
    let mut first = true;

    for project in &projects {
        let repo_path = std::path::Path::new(&project.path);

        if !repo_path.exists() {
            if json_mode {
                results.push(ProjectResult {
                    project: project.name.clone(),
                    path: project.path.clone(),
                    success: false,
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    error: Some(format!(
                        "Directory not found. Clone with: git clone {}",
                        project.repo
                    )),
                });
            } else {
                if !first {
                    println!();
                }
                first = false;
                meta_git_lib::print_missing_repo(&project.name, &project.repo, repo_path);
            }
            failed += 1;
            continue;
        }

        if json_mode {
            // Capture output for JSON mode
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&project.path)
                .arg("status")
                .output();

            match output {
                Ok(out) => {
                    let success = out.status.success();
                    if !success {
                        failed += 1;
                    }
                    results.push(ProjectResult {
                        project: project.name.clone(),
                        path: project.path.clone(),
                        success,
                        exit_code: out.status.code(),
                        stdout: Some(String::from_utf8_lossy(&out.stdout).to_string()),
                        stderr: if out.stderr.is_empty() {
                            None
                        } else {
                            Some(String::from_utf8_lossy(&out.stderr).to_string())
                        },
                        error: None,
                    });
                }
                Err(e) => {
                    failed += 1;
                    results.push(ProjectResult {
                        project: project.name.clone(),
                        path: project.path.clone(),
                        success: false,
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        error: Some(format!("Failed to run git status: {e}")),
                    });
                }
            }
        } else {
            // Human-readable output
            if !first {
                println!();
            }
            first = false;

            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&project.path)
                .arg("status")
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status();

            match status {
                Ok(exit) if exit.success() => {
                    println!();
                    println!(
                        "{} {}",
                        style("✓").green(),
                        style(&project.name).green().bold()
                    );
                }
                Ok(exit) => {
                    println!();
                    println!(
                        "{} {} (git status exited with code {:?})",
                        style("✗").red(),
                        style(&project.name).red().bold(),
                        exit.code()
                    );
                    failed += 1;
                }
                Err(e) => {
                    println!();
                    println!(
                        "{} {} (Failed to run git status: {})",
                        style("✗").red(),
                        style(&project.name).red().bold(),
                        e
                    );
                    failed += 1;
                }
            }
        }
    }

    if json_mode {
        let output = JsonOutput {
            version: "1.0",
            command: "git status".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            results,
            summary: OutputSummary {
                total: projects.len(),
                succeeded: projects.len() - failed,
                failed,
            },
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if failed > 0 {
        println!(
            "\nSummary: {} out of {} commands failed",
            style(format!("✗ {failed}")).red(),
            projects.len()
        );
        return Err(anyhow::anyhow!("At least one command failed"));
    }

    if failed > 0 && !json_mode {
        return Err(anyhow::anyhow!("At least one command failed"));
    }
    Ok(())
}

fn execute_git_clone(args: &[String]) -> anyhow::Result<()> {
    // Default options
    let mut _recursive = false;
    let parallel = 1_usize;
    let mut depth: Option<String> = None;

    let mut url = String::new();
    let mut dir_arg: Option<String> = None;
    let mut idx = 0;
    let mut git_clone_args: Vec<String> = Vec::new();

    while idx < args.len() {
        match args[idx].as_str() {
            "--recursive" => {
                _recursive = true;
                idx += 1;
            }
            "--parallel" => {
                if idx + 1 < args.len() {
                    let _parallel: usize = args[idx + 1].parse().unwrap_or(1);
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

    println!("Cloning meta repository: {url}");
    let mut clone_cmd = Command::new("git");
    clone_cmd.arg("clone").args(&git_clone_args).arg(&url);
    let clone_dir = if let Some(dir) = &dir_arg {
        clone_cmd.arg(dir);
        dir.clone()
    } else {
        // Derive directory name from URL
        url.trim_end_matches(".git")
            .rsplit('/')
            .next()
            .unwrap_or("meta")
            .to_string()
    };
    let status = clone_cmd.status()?;
    if !status.success() {
        println!("Failed to clone meta repository");
        return Ok(());
    }

    // Parse .meta file inside cloned repo
    let meta_path = Path::new(&clone_dir).join(".meta");
    if !meta_path.exists() {
        println!("No .meta file found in cloned repository");
        return Ok(());
    }

    let meta_content = fs::read_to_string(meta_path)?;
    let meta_config: MetaConfig = serde_json::from_str(&meta_content)?;

    let project_vec: Vec<ProjectEntry> = meta_config
        .projects
        .into_iter()
        .map(|(path, repo)| ProjectEntry {
            name: path.clone(),
            path,
            repo,
        })
        .collect();

    let projects = Arc::new(project_vec);

    println!(
        "Cloning {} child repositories with parallelism {}",
        projects.len(),
        parallel
    );

    let _pb = ProgressBar::new(projects.len() as u64);
    _pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
        )
        .unwrap()
        .progress_chars("#>-"),
    );

    println!(
        "{} 🔍  Resolving meta manifest...",
        style("[1/4]").bold().dim()
    );
    println!(
        "{} 🚚  Fetching meta repository...",
        style("[2/4]").bold().dim()
    );
    println!(
        "{} 🔗  Linking child repositories...",
        style("[3/4]").bold().dim()
    );
    println!(
        "{} 📃  Cloning child repositories...",
        style("[4/4]").bold().dim()
    );

    let mp = MultiProgress::new();

    let spinner_style = ProgressStyle::with_template("{prefix:.bold.dim} {spinner} {wide_msg}")
        .unwrap()
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

    let progress_per_repo = Arc::new(
        (0..projects.len())
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );

    let mut handles = vec![];
    let total = projects.len();

    for (i, proj) in projects.iter().cloned().enumerate() {
        let pb = mp.add(ProgressBar::new_spinner());
        pb.set_style(spinner_style.clone());
        pb.set_prefix(format!("[{}/{}]", i + 1, total));
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_message(format!("Cloning {}", proj.name));

        let proj = proj.clone();
        let clone_dir = clone_dir.clone();
        let depth = depth.clone();

        let progress_per_repo = Arc::clone(&progress_per_repo);
        let idx = i;

        let handle = std::thread::spawn(move || {
            let target_path = Path::new(&clone_dir).join(&proj.path);

            if target_path.exists()
                && target_path
                    .read_dir()
                    .map(|mut i| i.next().is_some())
                    .unwrap_or(false)
            {
                progress_per_repo[idx].store(100, std::sync::atomic::Ordering::Relaxed);
                pb.finish_with_message(format!(
                    "{}",
                    style(format!(
                        "Skipped {} (directory exists and is not empty)",
                        proj.name
                    ))
                    .yellow()
                ));
                return;
            }

            let mut cmd = Command::new("git");
            cmd.arg("clone").arg(&proj.repo).arg(&target_path);
            if let Some(ref d) = depth {
                cmd.arg("--depth").arg(d);
            }

            fn parse_git_progress(line: &str) -> Option<usize> {
                let patterns = [
                    "Receiving objects:",
                    "Counting objects:",
                    "Compressing objects:",
                ];
                for pat in &patterns {
                    if let Some(idx) = line.find(pat) {
                        let rest = &line[idx + pat.len()..];
                        if let Some(percent_idx) = rest.find('%') {
                            let before_percent = &rest[..percent_idx];
                            if let Some(num_start) = before_percent.rfind(' ') {
                                let num_str = before_percent[num_start..].trim();
                                if let Ok(pct) = num_str.parse::<usize>() {
                                    return Some(pct.min(100));
                                }
                            }
                        }
                    }
                }
                None
            }
            pb.set_message(format!("Cloning {}", proj.name));

            match cmd
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Ok(mut child) => {
                    let stdout = child.stdout.take().unwrap();
                    let stderr = child.stderr.take().unwrap();

                    let pb_clone = pb.clone();
                    let proj_name = proj.name.clone();
                    let progress_per_repo_clone = Arc::clone(&progress_per_repo);
                    std::thread::spawn(move || {
                        use std::io::{BufRead, BufReader};
                        let reader = BufReader::new(stdout);
                        for line in reader.lines().map_while(Result::ok) {
                            pb_clone.set_message(format!("{proj_name}: {line}"));
                            if let Some(percent) = parse_git_progress(&line) {
                                progress_per_repo_clone[idx]
                                    .store(percent, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    });

                    let pb_clone2 = pb.clone();
                    let proj_name2 = proj.name.clone();
                    let progress_per_repo_clone2 = Arc::clone(&progress_per_repo);
                    std::thread::spawn(move || {
                        use std::io::{BufRead, BufReader};
                        let reader = BufReader::new(stderr);
                        for line in reader.lines().map_while(Result::ok) {
                            pb_clone2.set_message(format!("{proj_name2}: {line}"));
                            if let Some(percent) = parse_git_progress(&line) {
                                progress_per_repo_clone2[idx]
                                    .store(percent, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    });

                    let status = child.wait();
                    match status {
                        Ok(s) if s.success() => {
                            progress_per_repo[idx].store(100, std::sync::atomic::Ordering::Relaxed);
                            pb.finish_with_message(format!(
                                "{}",
                                style(format!("Cloned {}", proj.name)).green()
                            ));
                        }
                        Ok(_) | Err(_) => {
                            progress_per_repo[idx].store(100, std::sync::atomic::Ordering::Relaxed);
                            pb.finish_with_message(format!(
                                "{}",
                                style(format!("Failed to clone {}", proj.name)).red()
                            ));
                        }
                    }
                }
                Err(_) => {
                    pb.finish_with_message(format!(
                        "{}",
                        style(format!("Failed to spawn git for {}", proj.name)).red()
                    ));
                }
            }
        });
        handles.push(handle);
    }

    for h in handles {
        let _ = h.join();
    }

    println!("Meta-repo clone completed");
    Ok(())
}

fn execute_git_update() -> anyhow::Result<()> {
    // Load meta config
    let cwd = std::env::current_dir()?;
    let meta_path = cwd.join(".meta");
    if !meta_path.exists() {
        println!("No .meta file found in {}", cwd.display());
        return Ok(());
    }
    let meta_content = std::fs::read_to_string(&meta_path)?;
    let meta_config: MetaConfig = serde_json::from_str(&meta_content)?;

    // Phase 1: Clone any missing repositories
    let mut cloned_count = 0;
    for (name, url) in &meta_config.projects {
        let repo_path = cwd.join(name);
        if !repo_path.exists() {
            println!("{} Cloning {}...", style("→").cyan(), style(name).bold());
            match meta_git_lib::clone_repo_with_progress(url, &repo_path, None) {
                Ok(_) => {
                    println!(
                        "{} {} cloned",
                        style("✓").green(),
                        style(name).green().bold()
                    );
                    cloned_count += 1;
                }
                Err(e) => {
                    println!(
                        "{} {} failed to clone: {}",
                        style("✗").red(),
                        style(name).red().bold(),
                        e
                    );
                }
            }
        }
    }

    if cloned_count > 0 {
        println!();
        println!("Cloned {} new repositories", style(cloned_count).green());
        println!();
    }

    // Phase 2: Use loop engine to run `git pull` in parallel across all repos
    let mut directories = vec![cwd.to_string_lossy().to_string()];
    directories.extend(
        meta_config
            .projects
            .keys()
            .map(|name| cwd.join(name).to_string_lossy().to_string()),
    );

    let config = loop_lib::LoopConfig {
        directories,
        ignore: vec![],
        verbose: false,
        silent: false,
        add_aliases_to_global_looprc: false,
        include_filters: None,
        exclude_filters: None,
        parallel: true, // Always run in parallel for git update
    };

    let result = loop_lib::run(&config, "git pull");

    // If there were failures and SSH multiplexing isn't configured, show hint
    if result.is_err() && !meta_git_lib::is_multiplexing_configured() {
        meta_git_lib::print_multiplexing_hint();
    }

    result
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
fn execute_git_commit(args: &[String]) -> anyhow::Result<()> {
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

    // Load meta config
    let cwd = std::env::current_dir()?;
    let meta_path = cwd.join(".meta");
    if !meta_path.exists() {
        println!("No .meta file found in {}", cwd.display());
        return Ok(());
    }
    let meta_content = std::fs::read_to_string(&meta_path)?;
    let meta_config: MetaConfig = serde_json::from_str(&meta_content)?;

    // Find repos with staged changes
    let mut repos_with_changes: Vec<(String, String, Vec<String>)> = Vec::new();

    // Check root repo
    if has_staged_changes(".") {
        let staged = get_staged_files(".");
        repos_with_changes.push((".".to_string(), ".".to_string(), staged));
    }

    // Check child repos
    for name in meta_config.projects.keys() {
        let repo_path = cwd.join(name);
        if repo_path.exists() && has_staged_changes(&repo_path.to_string_lossy()) {
            let staged = get_staged_files(&repo_path.to_string_lossy());
            repos_with_changes.push((
                name.clone(),
                repo_path.to_string_lossy().to_string(),
                staged,
            ));
        }
    }

    if repos_with_changes.is_empty() {
        println!("No staged changes found in any repository.");
        return Ok(());
    }

    if use_editor {
        // Open editor for per-repo messages
        execute_editor_commit(&repos_with_changes)?;
    } else if let Some(msg) = message {
        // Apply same message to all repos
        execute_bulk_commit(&repos_with_changes, &msg)?;
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

/// Execute bulk commit with same message for all repos
fn execute_bulk_commit(
    repos: &[(String, String, Vec<String>)],
    message: &str,
) -> anyhow::Result<()> {
    let mut succeeded = 0;
    let mut failed = 0;

    for (name, path, _files) in repos {
        println!("{} Committing {}...", style("→").cyan(), style(name).bold());

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

        let result = execute_command("git status", &[]);

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
    fn test_project_entry_clone() {
        let entry = ProjectEntry {
            name: "test".to_string(),
            path: "/path/to/test".to_string(),
            repo: "git@github.com:org/test.git".to_string(),
        };
        let cloned = entry.clone();
        assert_eq!(cloned.name, entry.name);
        assert_eq!(cloned.path, entry.path);
        assert_eq!(cloned.repo, entry.repo);
    }

    #[test]
    fn test_unknown_command() {
        let result = execute_command("git unknown", &[]);
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
}
