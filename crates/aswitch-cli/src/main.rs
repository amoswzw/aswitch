mod bundled;
mod cmd;
mod tui;

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;
use aswitch_core::paths::AswitchPaths;
use clap::{CommandFactory, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "aswitch",
    version,
    about = "Atomic account switching for AI agent CLIs"
)]
struct Cli {
    #[arg(long, global = true, value_name = "DIR")]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(name = "save")]
    Save(cmd::accounts::AddArgs),
    Use(cmd::accounts::UseArgs),
    #[command(name = "ls")]
    List(cmd::accounts::ListArgs),
    Login(cmd::login::LoginArgs),
    #[command(about = "Open the interactive terminal UI")]
    Tui,
    #[command(name = "rm")]
    Remove(cmd::accounts::RemoveArgs),
    #[command(hide = true)]
    Plugin {
        #[command(subcommand)]
        command: cmd::plugins::PluginCommand,
    },
    Init(cmd::init::InitArgs),
    #[command(name = "__shell", hide = true)]
    Shell {
        #[command(subcommand)]
        command: cmd::shell::ShellCommand,
    },
}

fn main() -> Result<()> {
    let raw_args: Vec<OsString> = env::args_os().collect();

    let bootstrap_paths = AswitchPaths::resolve(discover_config_dir(&raw_args))?;
    bootstrap_paths.ensure()?;

    let cli = Cli::parse_from(raw_args);
    let paths = AswitchPaths::resolve(cli.config_dir)?;
    paths.ensure()?;
    bundled::ensure_bundled_plugins(&paths)?;

    match cli.command {
        Some(Command::Save(args)) => cmd::accounts::add(&paths, args),
        Some(Command::Use(args)) => cmd::accounts::use_account(&paths, args),
        Some(Command::List(args)) => cmd::accounts::list(&paths, args),
        Some(Command::Login(args)) => cmd::login::run(&paths, args),
        Some(Command::Tui) => tui::run(&paths),
        Some(Command::Remove(args)) => cmd::accounts::remove(&paths, args),
        Some(Command::Plugin { command }) => cmd::plugins::run(&paths, command),
        Some(Command::Init(args)) => cmd::init::run(args),
        Some(Command::Shell { command }) => cmd::shell::run(&paths, command),
        None => {
            if tui::is_terminal_session() {
                tui::run(&paths)
            } else {
                Cli::command().print_help()?;
                println!();
                Ok(())
            }
        }
    }
}

fn discover_config_dir(args: &[OsString]) -> Option<PathBuf> {
    let mut iter = args.iter().skip(1);

    while let Some(arg) = iter.next() {
        if arg == "--config-dir" {
            return iter.next().map(PathBuf::from);
        }

        let text = arg.to_string_lossy();
        if let Some(value) = text.strip_prefix("--config-dir=") {
            return Some(PathBuf::from(value));
        }
    }

    None
}
