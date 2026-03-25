//! Self-contained SSH multiplexing for parallel git operations.
//!
//! Establishes SSH ControlMaster connections using explicit `-o` flags,
//! with no dependency on `~/.ssh/config`. Returns a `GIT_SSH_COMMAND`
//! value that git subprocesses use to share the established connections.

use log::{debug, warn};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Check if an existing ControlMaster connection is active for a host.
///
/// Uses `ssh -O check` which respects the user's own SSH config.
/// Returns true if the user already has a working master connection,
/// regardless of how it was configured.
fn has_existing_master(host: &str) -> bool {
    Command::new("ssh")
        .args(["-O", "check", &format!("git@{host}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check if a ControlMaster socket exists in the given directory for a host.
fn socket_exists_in(sockets_dir: &Path, host: &str) -> bool {
    let socket_path = sockets_dir.join(format!("git@{host}-22"));
    socket_path.exists()
}

/// Establish SSH ControlMaster connections for the given hosts.
///
/// Uses explicit `-o` flags so this works without `~/.ssh/config` having
/// multiplexing configured. If the user already has working ControlMaster
/// connections (from their own config), those are respected and we skip
/// setting up our own.
///
/// Returns `Some(sockets_dir)` if at least one master was established
/// (or already existed via our sockets dir). Returns `None` if:
/// - All hosts already have masters from user config (no override needed)
/// - Sockets dir couldn't be created
/// - All host connections failed
pub fn establish_ssh_masters(hosts: &[&str]) -> Option<PathBuf> {
    let sockets_dir = match meta_git_lib::ensure_ssh_sockets_dir() {
        Ok(Some(dir)) => dir,
        Ok(None) => {
            warn!("Could not determine HOME directory for SSH sockets");
            return None;
        }
        Err(e) => {
            warn!("Failed to create SSH sockets directory: {e}");
            return None;
        }
    };

    let mut any_needed_our_master = false;
    let mut any_succeeded = false;

    for host in hosts {
        // If user already has a working master from their own config, skip
        if has_existing_master(host) {
            debug!("Host {host} already has an active ControlMaster");
            continue;
        }

        // Check if we already have a socket from a previous run
        if socket_exists_in(&sockets_dir, host) {
            debug!("Socket already exists for {host}");
            any_needed_our_master = true;
            any_succeeded = true;
            continue;
        }

        any_needed_our_master = true;

        let control_path = format!("{}/{}", sockets_dir.display(), "%r@%h-%p");
        let status = Command::new("ssh")
            .args([
                "-fNM",
                "-o",
                "ControlMaster=auto",
                "-o",
                &format!("ControlPath={control_path}"),
                "-o",
                "ControlPersist=600",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &format!("git@{host}"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        match status {
            Ok(s) if s.success() => {
                debug!("Established SSH ControlMaster for {host}");
                any_succeeded = true;
            }
            Ok(s) => warn!("SSH master for {host} exited with status {s}"),
            Err(e) => warn!("Failed to spawn SSH master for {host}: {e}"),
        }
    }

    // If no host needed our master (all had existing connections), return None
    // so callers don't override GIT_SSH_COMMAND unnecessarily
    if !any_needed_our_master {
        debug!("All hosts have existing ControlMaster connections, no override needed");
        return None;
    }

    if any_succeeded {
        Some(sockets_dir)
    } else {
        None
    }
}

/// Build a `GIT_SSH_COMMAND` value that reuses established ControlMaster connections.
///
/// Set this on git subprocesses so they share the pre-established connections
/// without needing `~/.ssh/config` to be configured.
pub fn git_ssh_command(sockets_dir: &Path) -> String {
    format!(
        "ssh -o ControlMaster=auto -o ControlPath={sockets}/%r@%h-%p -o ControlPersist=600",
        sockets = sockets_dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_ssh_command_format() {
        let cmd = git_ssh_command(Path::new("/home/user/.ssh/sockets"));
        assert!(cmd.contains("ControlMaster=auto"));
        assert!(cmd.contains("ControlPath=/home/user/.ssh/sockets/%r@%h-%p"));
        assert!(cmd.contains("ControlPersist=600"));
        // Must NOT contain %% — Rust format! doesn't use % for formatting
        assert!(!cmd.contains("%%"));
    }

    #[test]
    fn test_socket_exists_in_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!socket_exists_in(dir.path(), "github.com"));
    }

    #[test]
    fn test_socket_exists_in_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("git@github.com-22"), "").unwrap();
        assert!(socket_exists_in(dir.path(), "github.com"));
    }

    #[test]
    fn test_has_existing_master_nonexistent_host() {
        // A host that definitely doesn't have a master
        assert!(!has_existing_master("nonexistent-host-12345.example.com"));
    }
}
