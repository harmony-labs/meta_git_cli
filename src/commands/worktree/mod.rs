//! Worktree command dispatch and help.

mod add;
pub(crate) mod cli_types;
mod create;
mod destroy;
mod diff;
mod exec;
mod list;
mod prune;
mod status;

use anyhow::Result;
use clap::Parser;
use colored::*;
use meta_plugin_protocol::CommandResult;

use cli_types::WorktreeCommands;

/// Clap parser wrapper for worktree subcommands.
#[derive(Parser)]
#[command(no_binary_name = true)]
struct WorktreeParser {
    #[command(subcommand)]
    command: WorktreeCommands,
}

/// Execute a worktree command dispatched from the plugin.
///
/// `command` is the matched command string (e.g., "worktree create", "worktree", "git worktree").
/// `args` contains the remaining arguments after the command prefix.
pub fn execute_worktree_command(
    command: &str,
    args: &[String],
    verbose: bool,
    json: bool,
) -> CommandResult {
    // Extract the subcommand name from the matched command prefix (if present).
    // When the plugin registers bare "worktree", the subcommand is in args[0] instead.
    let subcommand = command
        .strip_prefix("worktree ")
        .or_else(|| command.strip_prefix("git worktree "))
        .unwrap_or("");

    // Build args for clap
    let clap_args: Vec<String> = if subcommand.is_empty() {
        // Bare "worktree" or "git worktree" — args already contains the subcommand (if any)
        args.to_vec()
    } else {
        // Subcommand was part of the command prefix — prepend it
        std::iter::once(subcommand.to_string())
            .chain(args.iter().cloned())
            .collect()
    };

    // No subcommand at all — show help
    if clap_args.is_empty() {
        print_worktree_help();
        return CommandResult::Message(String::new());
    }

    match WorktreeParser::try_parse_from(&clap_args) {
        Ok(parsed) => match handle_worktree_command(parsed.command, verbose, json) {
            Ok(()) => CommandResult::Message(String::new()),
            Err(e) => CommandResult::Error(format!("{e}")),
        },
        Err(e) => {
            // Clap errors (help, version, parse errors)
            CommandResult::ShowHelp(Some(e.to_string()))
        }
    }
}

fn handle_worktree_command(
    command: WorktreeCommands,
    verbose: bool,
    json: bool,
) -> Result<()> {
    match command {
        WorktreeCommands::Create(args) => create::handle_create(args, verbose, json),
        WorktreeCommands::Add(args) => add::handle_add(args, verbose, json),
        WorktreeCommands::Destroy(args) => destroy::handle_destroy(args, verbose, json),
        WorktreeCommands::List(args) => list::handle_list(args, verbose, json),
        WorktreeCommands::Status(args) => status::handle_status(args, verbose, json),
        WorktreeCommands::Diff(args) => diff::handle_diff(args, verbose, json),
        WorktreeCommands::Exec(args) => exec::handle_exec(args, verbose, json),
        WorktreeCommands::Prune(args) => prune::handle_prune(args, verbose, json),
        WorktreeCommands::Unknown(args) => {
            let cmd = args.first().map(|s| s.as_str()).unwrap_or("");
            eprintln!(
                "{}: unrecognized worktree subcommand '{cmd}'",
                "error".red().bold(),
            );
            eprintln!();
            eprint_worktree_help();
            anyhow::bail!("unrecognized worktree subcommand '{cmd}'");
        }
    }
}

/// Log a store operation error as a warning without failing the command.
pub(crate) fn warn_store_error(result: anyhow::Result<()>) {
    if let Err(e) = result {
        eprintln!(
            "{} Failed to update store: {}",
            "warning:".yellow().bold(),
            e
        );
    }
}

/// Print worktree help text to stdout.
pub fn print_worktree_help() {
    write_worktree_help(&mut std::io::stdout());
}

/// Print worktree help text to stderr (for error cases in plugin context).
pub fn eprint_worktree_help() {
    write_worktree_help(&mut std::io::stderr());
}

fn write_worktree_help(w: &mut dyn std::io::Write) {
    let _ = writeln!(w, "Manage git worktrees across repos");
    let _ = writeln!(w);
    let _ = writeln!(w, "USAGE: meta worktree <COMMAND> [OPTIONS]");
    let _ = writeln!(w);
    let _ = writeln!(w, "COMMANDS:");
    let _ = writeln!(w, "  create   Create a new worktree set");
    let _ = writeln!(w, "  add      Add a repo to an existing worktree set");
    let _ = writeln!(w, "  destroy  Remove a worktree set");
    let _ = writeln!(w, "  list     List all worktree sets");
    let _ = writeln!(w, "  status   Show detailed status of a worktree set");
    let _ = writeln!(w, "  diff     Show cross-repo diff vs base branch");
    let _ = writeln!(w, "  exec     Run a command across worktree repos");
    let _ = writeln!(w, "  prune    Remove expired/orphaned worktrees");
    let _ = writeln!(w);
    let _ = writeln!(w, "CREATE OPTIONS:");
    let _ = writeln!(w, "  --repo <ALIAS[:BRANCH]>  Add specific repo(s)");
    let _ = writeln!(w, "  --all                    Add all repos from .meta config");
    let _ = writeln!(w, "  --branch <NAME>          Override default branch name");
    let _ = writeln!(w, "  --from-ref <REF>         Start from a specific tag/SHA");
    let _ = writeln!(w, "  --from-pr <OWNER/REPO#N> Start from a PR's head branch");
    let _ = writeln!(w, "  --ephemeral              Mark for automatic cleanup");
    let _ = writeln!(w, "  --ttl <DURATION>         Time-to-live (30s, 5m, 1h, 2d, 1w)");
    let _ = writeln!(w, "  --meta <KEY=VALUE>       Store custom metadata");
    let _ = writeln!(w);
    let _ = writeln!(w, "DESTROY OPTIONS:");
    let _ = writeln!(w, "  --force                  Remove even with uncommitted changes");
    let _ = writeln!(w);
    let _ = writeln!(w, "EXEC OPTIONS:");
    let _ = writeln!(w, "  --include <REPOS>        Only run in specified repos");
    let _ = writeln!(w, "  --exclude <REPOS>        Skip specified repos");
    let _ = writeln!(w, "  --parallel               Run commands concurrently");
    let _ = writeln!(w, "  --ephemeral              Atomic create+exec+destroy");
    let _ = writeln!(w);
    let _ = writeln!(w, "DIFF OPTIONS:");
    let _ = writeln!(w, "  --base <BRANCH>          Base branch for comparison (default: main)");
    let _ = writeln!(w, "  --stat                   Show diffstat summary only");
    let _ = writeln!(w);
    let _ = writeln!(w, "Use 'meta worktree <command> --help' for more details.");
}
