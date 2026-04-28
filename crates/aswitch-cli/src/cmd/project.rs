use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use super::accounts;
use crate::cmd::session;
use aswitch_core::paths::AswitchPaths;

pub(crate) const PROJECT_FILE_NAME: &str = ".aswitch.toml";

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    Use(ProjectUseArgs),
    Clear(ProjectClearArgs),
    Show(ProjectShowArgs),
}

#[derive(Debug, Args)]
#[command(about = "Bind an account to the current project")]
pub struct ProjectUseArgs {
    #[arg(
        value_name = "ACCOUNT",
        help = "<plugin>/<alias>, alias, or row number"
    )]
    pub selector: Option<String>,
    #[arg(long, value_name = "ID")]
    pub plugin: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
#[command(about = "Clear the current project binding")]
pub struct ProjectClearArgs {
    #[arg(long, value_name = "ID")]
    pub plugin: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
#[command(about = "Show the current project binding")]
pub struct ProjectShowArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ProjectConfig {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    accounts: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProjectBinding {
    pub path: PathBuf,
    pub accounts: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
struct ProjectUseReport {
    path: PathBuf,
    plugin_id: String,
    alias: String,
}

#[derive(Clone, Debug, Serialize)]
struct ProjectClearReport {
    path: PathBuf,
    removed_plugin: Option<String>,
    removed_file: bool,
}

pub fn run(paths: &AswitchPaths, command: ProjectCommand) -> Result<()> {
    match command {
        ProjectCommand::Use(args) => use_project(paths, args),
        ProjectCommand::Clear(args) => clear_project(args),
        ProjectCommand::Show(args) => show_project(args),
    }
}

pub(crate) fn current_project_binding() -> Result<Option<ProjectBinding>> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    load_nearest_binding(&cwd)
}

pub(crate) fn load_nearest_binding(start_dir: &Path) -> Result<Option<ProjectBinding>> {
    let Some(path) = find_project_file(start_dir) else {
        return Ok(None);
    };

    let config = read_project_config(&path)?;
    Ok(Some(ProjectBinding {
        path,
        accounts: config.accounts,
    }))
}

fn use_project(paths: &AswitchPaths, args: ProjectUseArgs) -> Result<()> {
    let resolved = match args.selector.as_deref() {
        Some(selector) => accounts::resolve_selector(paths, selector, args.plugin.as_deref())?,
        None => {
            if args.json {
                bail!("Interactive project binding does not support --json; pass <plugin>/<alias>");
            }
            let Some(resolved) = accounts::prompt_for_account(paths)? else {
                println!("Project binding cancelled.");
                return Ok(());
            };
            resolved
        }
    };
    session::build_activation_output(paths, &resolved.plugin_id, &resolved.alias)?;

    let path = project_file_for_write()?;
    let mut config = if path.exists() {
        read_project_config(&path)?
    } else {
        ProjectConfig::default()
    };
    config.version = default_version();
    config
        .accounts
        .insert(resolved.plugin_id.clone(), resolved.alias.clone());
    write_project_config(&path, &config)?;

    let report = ProjectUseReport {
        path,
        plugin_id: resolved.plugin_id,
        alias: resolved.alias,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Project binding saved: {} -> {}/{}",
            report.path.display(),
            report.plugin_id,
            report.alias
        );
    }

    Ok(())
}

fn clear_project(args: ProjectClearArgs) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let Some(path) = find_project_file(&cwd) else {
        bail!("No project binding was found in the current directory tree");
    };

    let mut config = read_project_config(&path)?;
    let removed_file;
    let removed_plugin = args.plugin.clone();

    if let Some(plugin_id) = args.plugin {
        if config.accounts.remove(&plugin_id).is_none() {
            bail!("Plugin {plugin_id} is not bound in {}", path.display());
        }

        if config.accounts.is_empty() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            removed_file = true;
        } else {
            write_project_config(&path, &config)?;
            removed_file = false;
        }
    } else {
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
        removed_file = true;
    }

    let report = ProjectClearReport {
        path,
        removed_plugin,
        removed_file,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if let Some(plugin_id) = report.removed_plugin.as_deref() {
        println!(
            "Removed project binding for {plugin_id} from {}",
            report.path.display()
        );
    } else {
        println!("Removed project binding file {}", report.path.display());
    }

    Ok(())
}

fn show_project(args: ProjectShowArgs) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let binding = load_nearest_binding(&cwd)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&binding)?);
        return Ok(());
    }

    let Some(binding) = binding else {
        println!("No project binding found.");
        return Ok(());
    };

    println!("path: {}", binding.path.display());
    if binding.accounts.is_empty() {
        println!("accounts: -");
        return Ok(());
    }

    println!("accounts:");
    for (plugin_id, alias) in binding.accounts {
        println!("- {plugin_id}/{alias}");
    }

    Ok(())
}

fn project_file_for_write() -> Result<PathBuf> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    Ok(find_project_file(&cwd).unwrap_or_else(|| cwd.join(PROJECT_FILE_NAME)))
}

pub(crate) fn find_project_file(start_dir: &Path) -> Option<PathBuf> {
    let mut current = Some(start_dir);

    while let Some(dir) = current {
        let candidate = dir.join(PROJECT_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        current = dir.parent();
    }

    None
}

fn read_project_config(path: &Path) -> Result<ProjectConfig> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut config: ProjectConfig =
        toml::from_str(&source).with_context(|| format!("failed to parse {}", path.display()))?;
    if config.version == 0 {
        config.version = default_version();
    }
    Ok(config)
}

fn write_project_config(path: &Path, config: &ProjectConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let payload = toml::to_string_pretty(config).context("failed to serialize project config")?;
    fs::write(path, payload).with_context(|| format!("failed to write {}", path.display()))
}

fn default_version() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{find_project_file, load_nearest_binding, write_project_config, ProjectConfig};

    #[test]
    fn finds_nearest_project_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join("repo");
        let nested = root.join("a/b/c");
        fs::create_dir_all(&nested).expect("nested dirs");
        fs::write(root.join(".aswitch.toml"), "version = 1\n").expect("config");

        let found = find_project_file(&nested).expect("project file");
        assert_eq!(found, root.join(".aswitch.toml"));
    }

    #[test]
    fn loads_project_accounts() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join(".aswitch.toml");
        write_project_config(
            &path,
            &ProjectConfig {
                version: 1,
                accounts: [("codex".to_string(), "work".to_string())]
                    .into_iter()
                    .collect(),
            },
        )
        .expect("write");

        let binding = load_nearest_binding(temp_dir.path())
            .expect("load")
            .expect("binding");
        assert_eq!(
            binding.accounts.get("codex").map(String::as_str),
            Some("work")
        );
    }
}
