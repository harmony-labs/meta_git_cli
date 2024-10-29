// #[no_mangle]
// pub extern "C" fn register_plugin() {
//     println!("meta-git plugin loaded");
//     // Here you can add more functionality, like registering commands
// }
use meta::{Plugin, PluginError};

pub struct GitPlugin;

impl Plugin for GitPlugin {
    fn name(&self) -> &'static str {
        "git"
    }

    fn commands(&self) -> Vec<&'static str> {
        vec!["git"]
    }

    fn execute(&self, command: &str, args: &[String]) -> anyhow::Result<()> {
        match command {
            "git clone" => {
                if args.is_empty() {
                    println!("Usage: meta git clone <repository>");
                    return Ok(());
                }
                // Implement git clone functionality
                println!("Cloning repository: {}", args[0]);
                Ok(())
            }
            _ => Err(PluginError::CommandNotFound(command.to_string()).into()),
        }
    }
}

#[no_mangle]
pub extern "C" fn _plugin_create() -> *mut dyn Plugin {
    Box::into_raw(Box::new(GitPlugin))
}