use console::style;
use meta_plugin_protocol::CommandResult;

pub(crate) fn execute_git_setup_ssh() -> anyhow::Result<CommandResult> {
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
                return Ok(CommandResult::Error(format!("Failed to set up SSH multiplexing: {e}")));
            }
        }
    }
    Ok(CommandResult::Message(String::new()))
}
