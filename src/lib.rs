use console::style;
use indicatif::{ProgressBar, ProgressStyle, MultiProgress};
use log::debug;
use meta_plugin_api::{Plugin, HelpMode, PluginError};
use serde::Deserialize;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use meta_git_lib;
use std::time::Duration;
use std::path::Path;
use std::process::Command;

use std::collections::HashMap;

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

pub struct GitPlugin;


impl Plugin for GitPlugin {
    fn name(&self) -> &'static str {
        "git-clone"
    }

    fn commands(&self) -> Vec<&'static str> {
        vec!["git clone"]
    }

    fn execute(&self, command: &str, args: &[String]) -> anyhow::Result<()> {
        debug!("[meta_git_cli] Plugin invoked with command: '{}'", command);
        debug!("[meta_git_cli] Args: {:?}", args);
        match command {
            "git clone" => {
                // Default options
                let mut recursive = false;
                let mut parallel = 1_usize;
                let mut depth: Option<String> = None;

                let mut url = String::new();
                let mut dir_arg: Option<String> = None;
                let mut idx = 0;
                let mut git_clone_args: Vec<String> = Vec::new();

                while idx < args.len() {
                    match args[idx].as_str() {
                        "--recursive" => {
                            recursive = true;
                            idx += 1;
                        }
                        "--parallel" => {
                            if idx + 1 < args.len() {
                                parallel = args[idx + 1].parse().unwrap_or(1);
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

                println!("Cloning meta repository: {}", url);
                let mut clone_cmd = Command::new("git");
                clone_cmd.arg("clone").args(&git_clone_args).arg(&url);
                let clone_dir = if let Some(dir) = &dir_arg {
                    clone_cmd.arg(dir);
                    dir.clone()
                } else {
                    // Derive directory name from URL
                    let repo_name = url
                        .trim_end_matches(".git")
                        .rsplit('/')
                        .next()
                        .unwrap_or("meta")
                        .to_string();
                    repo_name
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

                let project_vec: Vec<ProjectEntry> = meta_config.projects.into_iter().map(|(path, repo)| {
                    ProjectEntry {
                        name: path.clone(),
                        path,
                        repo,
                    }
                }).collect();

                let projects = Arc::new(project_vec);

                println!("Cloning {} child repositories with parallelism {}", projects.len(), parallel);


                let pb = ProgressBar::new(projects.len() as u64);
                pb.set_style(ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
                    .unwrap()
                    .progress_chars("#>-"));

                println!(
                    "{} {}Resolving meta manifest...",
                    style("[1/4]").bold().dim(),
                    "🔍  "
                );
                println!(
                    "{} {}Fetching meta repository...",
                    style("[2/4]").bold().dim(),
                    "🚚  "
                );
                println!(
                    "{} {}Linking child repositories...",
                    style("[3/4]").bold().dim(),
                    "🔗  "
                );
                println!(
                    "{} {}Cloning child repositories...",
                    style("[4/4]").bold().dim(),
                    "📃  "
                );

                let mp = MultiProgress::new();


                let spinner_style = ProgressStyle::with_template("{prefix:.bold.dim} {spinner} {wide_msg}")
                    .unwrap()
                    .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

                let mut repo_pbs = Vec::new();

                let total = projects.len();
                for (i, proj) in projects.iter().cloned().enumerate() {
                    let pb = mp.add(ProgressBar::new_spinner());
                    pb.set_style(spinner_style.clone());
                    pb.set_prefix(format!("[{}/{}]", i + 1, total));
                    pb.enable_steady_tick(Duration::from_millis(100));
                    pb.set_message(format!("Cloning {}", proj.name));
                    // Use the shared clone logic for each repo
                    let url = &proj.repo;
                    let target_dir = std::path::Path::new(&proj.path);
                    let _ = meta_git_lib::clone_repo_with_progress(url, target_dir, Some(&pb));
                    repo_pbs.push(pb);
                }

                let spinner_style = ProgressStyle::with_template("{prefix:.bold.dim} {spinner} {wide_msg}")
                    .unwrap()
                    .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

                let progress_per_repo = Arc::new(
                    (0..projects.len()).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>()
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

                        if target_path.exists() && target_path.read_dir().map(|mut i| i.next().is_some()).unwrap_or(false) {
                            progress_per_repo[idx].store(100, std::sync::atomic::Ordering::Relaxed);
                            pb.finish_with_message(format!("{}", style(format!("Skipped {} (directory exists and is not empty)", proj.name)).yellow()));
                            return;
                        }

                        let mut cmd = Command::new("git");
                        cmd.arg("clone").arg(&proj.repo).arg(&target_path);
                        if let Some(ref d) = depth {
                            cmd.arg("--depth").arg(d);
                        }
                        
                        fn parse_git_progress(line: &str) -> Option<usize> {
                            let patterns = ["Receiving objects:", "Counting objects:", "Compressing objects:"];
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

                        match cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn() {
                            Ok(mut child) => {
                                let stdout = child.stdout.take().unwrap();
                                let stderr = child.stderr.take().unwrap();

                                let pb_clone = pb.clone();
                                let proj_name = proj.name.clone();
                                let progress_per_repo_clone = Arc::clone(&progress_per_repo);
                                std::thread::spawn(move || {
                                    use std::io::{BufRead, BufReader};
                                    let reader = BufReader::new(stdout);
                                    for line in reader.lines().flatten() {
                                        pb_clone.set_message(format!("{}: {}", proj_name, line));
                                        if let Some(percent) = parse_git_progress(&line) {
                                            progress_per_repo_clone[idx].store(percent, std::sync::atomic::Ordering::Relaxed);
                                        }
                                    }
                                });

                                let pb_clone2 = pb.clone();
                                let proj_name2 = proj.name.clone();
                                let progress_per_repo_clone2 = Arc::clone(&progress_per_repo);
                                std::thread::spawn(move || {
                                    use std::io::{BufRead, BufReader};
                                    let reader = BufReader::new(stderr);
                                    for line in reader.lines().flatten() {
                                        pb_clone2.set_message(format!("{}: {}", proj_name2, line));
                                        if let Some(percent) = parse_git_progress(&line) {
                                            progress_per_repo_clone2[idx].store(percent, std::sync::atomic::Ordering::Relaxed);
                                        }
                                    }
                                });

                                let status = child.wait();
                                match status {
                                    Ok(s) if s.success() => {
                                        progress_per_repo[idx].store(100, std::sync::atomic::Ordering::Relaxed);
                                        pb.finish_with_message(format!("{}", style(format!("Cloned {}", proj.name)).green()));
                                    }
                                    Ok(_) | Err(_) => {
                                        progress_per_repo[idx].store(100, std::sync::atomic::Ordering::Relaxed);
                                        pb.finish_with_message(format!("{}", style(format!("Failed to clone {}", proj.name)).red()));
                                    }
                                }
                            }
                            Err(_) => {
                                pb.finish_with_message(format!("{}", style(format!("Failed to spawn git for {}", proj.name)).red()));
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
            "git update" => {
                let status = std::process::Command::new("git")
                    .arg("pull")
                    .status()?;
                if !status.success() {
                    anyhow::bail!("git pull failed");
                }
                Ok(())
            }
            _ => Err(PluginError::CommandNotFound(command.to_string()).into()),
        }
    }
    fn get_help_output(&self, _args: &[String]) -> Option<(HelpMode, String)> {
        let help_text = r#"meta git - Meta CLI Git Plugin
(This is NOT plain git)

Meta-repo Commands:
  meta git clone <meta-repo-url> [options]
    Clones the meta repository and all child repositories defined in its manifest.

    Options:
      --recursive       Clone nested meta repositories recursively
      --parallel N      Clone up to N repositories in parallel
      --depth N         Create a shallow clone with truncated history

    Examples:
      meta git clone https://github.com/example/meta-repo.git
      meta git clone --parallel 4 --depth 1 https://github.com/example/meta-repo.git

For standard git commands, see below.
"#.to_string();

        Some((HelpMode::Prepend, help_text))
    }
}

#[no_mangle]
pub extern "C" fn _plugin_create() -> *mut dyn Plugin {
    Box::into_raw(Box::new(GitPlugin))
}
