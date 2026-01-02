//! meta-git subprocess plugin
//!
//! This is the main entry point for the meta-git plugin, which provides
//! git operations optimized for meta repositories.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{self, Read};

/// Plugin info returned by --meta-plugin-info
#[derive(Debug, Serialize)]
struct PluginInfo {
    name: String,
    version: String,
    commands: Vec<String>,
    description: Option<String>,
}

/// Request received from meta CLI via --meta-plugin-exec
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PluginRequest {
    command: String,
    args: Vec<String>,
    #[serde(default)]
    projects: Vec<String>,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    options: PluginRequestOptions,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct PluginRequestOptions {
    #[serde(default)]
    json_output: bool,
    #[serde(default)]
    verbose: bool,
    #[serde(default)]
    parallel: bool,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: meta-git --meta-plugin-info | --meta-plugin-exec");
        std::process::exit(1);
    }

    match args[1].as_str() {
        "--meta-plugin-info" => {
            let info = PluginInfo {
                name: "git".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                commands: vec![
                    "git clone".to_string(),
                    "git status".to_string(),
                    "git update".to_string(),
                    "git setup-ssh".to_string(),
                ],
                description: Some("Git operations for meta repositories".to_string()),
            };
            println!("{}", serde_json::to_string(&info)?);
        }
        "--meta-plugin-exec" => {
            // Read JSON request from stdin
            let mut input = String::new();
            io::stdin().read_to_string(&mut input)?;

            let request: PluginRequest = serde_json::from_str(&input)?;

            // Set environment variables based on options
            if request.options.json_output {
                std::env::set_var("META_JSON_OUTPUT", "1");
            }

            // Change to the specified working directory if provided
            if !request.cwd.is_empty() {
                std::env::set_current_dir(&request.cwd)?;
            }

            // Execute the command
            let result = meta_git_cli::execute_command(&request.command, &request.args);

            if let Err(e) = result {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        "--help" | "-h" => {
            println!("{}", meta_git_cli::get_help_text());
        }
        _ => {
            eprintln!("Unknown argument: {}", args[1]);
            eprintln!("Usage: meta-git --meta-plugin-info | --meta-plugin-exec");
            std::process::exit(1);
        }
    }

    Ok(())
}
