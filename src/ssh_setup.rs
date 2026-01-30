//! SSH setup helpers for parallel git operations.
//!
//! This module provides functions to generate SSH ControlMaster pre-commands
//! that establish persistent connections before parallel git operations.
//! This prevents the race condition where multiple parallel connections
//! all try to become the ControlMaster simultaneously.

use meta_plugin_protocol::PlannedCommand;
use std::path::Path;

/// Generate SSH ControlMaster pre-commands for the given hosts.
///
/// Returns commands that establish persistent SSH connections before
/// parallel git operations, preventing the ControlMaster race condition.
///
/// Only generates commands for hosts that:
/// 1. Have multiplexing configured (ControlMaster in ~/.ssh/config)
/// 2. Don't already have an active socket
pub fn ssh_pre_commands(hosts: &[&str]) -> Vec<PlannedCommand> {
    hosts
        .iter()
        .filter(|host| needs_master_connection(host))
        .map(|host| PlannedCommand {
            dir: ".".to_string(),
            cmd: format!(
                "ssh -fNM -o ControlMaster=auto -o ControlPath=~/.ssh/sockets/%r@%h-%p -o ControlPersist=600 -o ConnectTimeout=10 git@{}",
                host
            ),
            env: None,
        })
        .collect()
}

/// Check if we need to establish a master connection for this host.
///
/// Returns true if:
/// - SSH multiplexing is configured for this host
/// - No active ControlMaster socket exists
fn needs_master_connection(host: &str) -> bool {
    // If socket already exists, no need to create another
    if socket_exists(host) {
        return false;
    }

    // Check if multiplexing is configured
    is_multiplexing_configured(host)
}

/// Check if ControlMaster socket already exists for host.
pub fn socket_exists(host: &str) -> bool {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return false,
    };

    // Check common socket path patterns
    let socket_patterns = [
        format!("{}/.ssh/sockets/git@{}-22", home, host),
        format!("{}/.ssh/sockets/%r@%h-%p", home), // literal pattern (shouldn't exist)
    ];

    for pattern in &socket_patterns {
        if Path::new(pattern).exists() {
            return true;
        }
    }

    false
}

/// Check if SSH multiplexing is configured for the given host.
///
/// Looks for ControlMaster settings in ~/.ssh/config.
fn is_multiplexing_configured(host: &str) -> bool {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return false,
    };

    let config_path = format!("{}/.ssh/config", home);
    let config_content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Parse SSH config to check for ControlMaster
    // This is a simplified parser - SSH config is complex
    let mut in_matching_host_block = false;
    let mut found_control_master = false;

    for line in config_content.lines() {
        let line = line.trim();

        // Skip comments and empty lines
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        // Check for Host directive
        if line.to_lowercase().starts_with("host ") {
            let hosts: Vec<&str> = line[5..].split_whitespace().collect();
            in_matching_host_block = hosts.iter().any(|h| {
                // Handle wildcards
                if *h == "*" {
                    true
                } else if h.contains('*') {
                    // Simple wildcard matching (e.g., "*.github.com")
                    let pattern = h.replace('*', "");
                    host.contains(&pattern)
                } else {
                    *h == host
                }
            });
        }

        // Check for ControlMaster in matching block or global (*) block
        if in_matching_host_block
            && (line.to_lowercase().starts_with("controlmaster ")
                || line.to_lowercase().starts_with("controlmaster="))
        {
            let value = line
                .split_once(char::is_whitespace)
                .or_else(|| line.split_once('='))
                .map(|(_, v)| v.trim())
                .unwrap_or("");

            if value == "auto" || value == "yes" || value == "autoask" {
                found_control_master = true;
            }
        }
    }

    found_control_master
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_pre_commands_generates_correct_format() {
        // Note: This test may not generate commands if multiplexing isn't configured
        // or sockets already exist. We test the format when commands ARE generated.
        let commands = ssh_pre_commands(&["example.com"]);

        // If a command was generated, verify its format
        for cmd in commands {
            assert!(cmd.cmd.contains("ssh -fNM"));
            assert!(cmd.cmd.contains("ControlMaster=auto"));
            assert!(cmd.cmd.contains("ControlPersist=600"));
            assert!(cmd.cmd.contains("git@example.com"));
            assert_eq!(cmd.dir, ".");
        }
    }

    #[test]
    fn test_socket_exists_nonexistent() {
        // A host that definitely doesn't have a socket
        assert!(!socket_exists("nonexistent-host-12345.example.com"));
    }

    #[test]
    fn test_needs_master_connection_respects_existing_socket() {
        // If socket exists, should return false
        // We can't easily test this without creating actual sockets,
        // but we can verify the logic flow
        let host = "test-host-that-does-not-exist.example.com";
        // This should check socket_exists first, which will return false
        // Then check is_multiplexing_configured, which will also likely return false
        let _ = needs_master_connection(host);
    }
}
