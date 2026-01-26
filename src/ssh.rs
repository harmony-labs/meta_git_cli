use console::style;
use meta_plugin_protocol::CommandResult;
use std::collections::BTreeSet;

pub(crate) fn execute_git_setup_ssh() -> anyhow::Result<CommandResult> {
    let hosts = discover_ssh_hosts();
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

/// Discover unique SSH hosts from the .meta config in the current directory.
/// Falls back to ["github.com"] if no .meta config is found or no SSH URLs exist.
fn discover_ssh_hosts() -> Vec<String> {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return vec!["github.com".to_string()],
    };

    let Some((config_path, _format)) = meta_cli::config::find_meta_config(&cwd, None) else {
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
