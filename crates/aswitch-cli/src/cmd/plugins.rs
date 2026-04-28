use std::path::PathBuf;

use anyhow::Result;
use aswitch_core::paths::AswitchPaths;
use aswitch_core::plugin::{load_all, load_manifest, PluginCatalog};
use clap::{Args, Subcommand};

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    #[command(name = "ls")]
    List(ListArgs),
    #[command(about = "Show the plugin directory path")]
    Path,
    Validate(ValidateArgs),
    Install(InstallArgs),
}

#[derive(Debug, Args)]
#[command(about = "List loaded plugins")]
pub struct ListArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
#[command(about = "Validate a plugin.toml file")]
pub struct ValidateArgs {
    pub path: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
#[command(about = "Install a plugin.toml file into the config directory")]
pub struct InstallArgs {
    pub path: PathBuf,
    #[arg(long)]
    force: bool,
}

pub fn run(paths: &AswitchPaths, command: PluginCommand) -> Result<()> {
    match command {
        PluginCommand::List(args) => list(paths, args),
        PluginCommand::Path => {
            println!("{}", paths.plugins_dir.display());
            Ok(())
        }
        PluginCommand::Validate(args) => validate(args),
        PluginCommand::Install(args) => install(paths, args),
    }
}

fn list(paths: &AswitchPaths, args: ListArgs) -> Result<()> {
    let catalog = load_all(&paths.plugins_dir)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&catalog)?);
        return Ok(());
    }

    print_catalog(&catalog);
    Ok(())
}

fn validate(args: ValidateArgs) -> Result<()> {
    let manifest = load_manifest(&args.path)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&manifest)?);
    } else {
        println!(
            "valid\t{}\t{}\t{}",
            manifest.manifest.id,
            manifest.manifest.version,
            manifest.path.display()
        );
    }

    Ok(())
}

fn install(paths: &AswitchPaths, args: InstallArgs) -> Result<()> {
    let manifest = load_manifest(&args.path)?;
    let plugin_dir = paths.plugins_dir.join(&manifest.manifest.id);
    let destination = plugin_dir.join("plugin.toml");

    if destination.exists() && !args.force {
        anyhow::bail!(
            "plugin {} already exists at {}; rerun with --force to overwrite",
            manifest.manifest.id,
            destination.display()
        );
    }

    std::fs::create_dir_all(&plugin_dir)?;
    std::fs::copy(&args.path, &destination)?;
    println!(
        "installed\t{}\t{}\t{}",
        manifest.manifest.id,
        manifest.manifest.version,
        destination.display()
    );
    Ok(())
}

fn print_catalog(catalog: &PluginCatalog) {
    println!("{:<16} {:<10} {:<8} STATUS", "ID", "VERSION", "SOURCE");

    for plugin in &catalog.plugins {
        println!(
            "{:<16} {:<10} {:<8} ok",
            plugin.manifest.id, plugin.manifest.version, "user"
        );
    }

    for error in &catalog.errors {
        println!(
            "{:<16} {:<10} {:<8} error: {}",
            "-", "-", "user", error.error
        );
    }

    for warning in &catalog.warnings {
        eprintln!("warning: {warning}");
    }
}
