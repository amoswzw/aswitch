use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use aswitch_core::account;
use aswitch_core::paths;
use clap::{Args, Subcommand};
use serde::Serialize;

use super::{accounts, project, scope, session};
use aswitch_core::paths::AswitchPaths;
use aswitch_core::plugin::load_manifest;
use aswitch_core::registry::Registry;
use aswitch_core::store::{self, CredentialStore};

#[derive(Debug, Subcommand)]
pub enum ShellCommand {
    Use(ShellUseArgs),
    SyncProject,
}

#[derive(Debug, Args)]
pub struct ShellUseArgs {
    pub selector: Option<String>,
    #[arg(long, value_name = "ID")]
    pub plugin: Option<String>,
    #[arg(long)]
    pub off: bool,
    #[arg(long, hide = true)]
    pub json: bool,
    #[arg(long, hide = true)]
    pub scope: Option<String>,
}

pub fn run(paths: &AswitchPaths, command: ShellCommand) -> Result<()> {
    match command {
        ShellCommand::Use(args) => run_local(paths, args),
        ShellCommand::SyncProject => sync_project(paths),
    }
}

#[derive(Debug, Serialize)]
struct GlobalClearReport {
    plugin_id: String,
    previous_alias: Option<String>,
}

fn run_local(paths: &AswitchPaths, args: ShellUseArgs) -> Result<()> {
    if args.off {
        if args.selector.is_some() {
            bail!("Shell off does not take ACCOUNT; use --plugin when needed");
        }
        return clear_local(paths, args.plugin);
    }

    let resolved = match args.selector.as_deref() {
        Some(selector) => accounts::resolve_selector(paths, selector, args.plugin.as_deref())?,
        None => match accounts::prompt_for_account(paths)? {
            Some(resolved) => resolved,
            None => return Ok(()),
        },
    };

    let activation = session::build_activation_output(paths, &resolved.plugin_id, &resolved.alias)?;
    let mut local = scope::local_state();
    local.insert(resolved.plugin_id.clone(), resolved.alias.clone());

    let mut lines = vec![activation.shell];
    lines.push(export_var(
        scope::LOCAL_STATE_ENV,
        &scope::serialize_state(&local)?,
    ));

    println!("{}", lines.join("\n"));
    Ok(())
}

fn clear_local(paths: &AswitchPaths, plugin: Option<String>) -> Result<()> {
    let mut local = scope::local_state();
    let mut project_state = scope::project_state();
    let plugins = if let Some(plugin_id) = plugin {
        vec![plugin_id]
    } else {
        local.keys().cloned().collect::<Vec<_>>()
    };

    let mut lines = Vec::new();
    for plugin_id in plugins {
        if local.remove(&plugin_id).is_some() {
            if let Ok(output) = session::build_deactivation_output(paths, &plugin_id) {
                lines.push(output.shell);
            }
            project_state.remove(&plugin_id);
        }
    }

    if local.is_empty() {
        lines.push(unset_var(scope::LOCAL_STATE_ENV));
    } else {
        lines.push(export_var(
            scope::LOCAL_STATE_ENV,
            &scope::serialize_state(&local)?,
        ));
    }

    if project_state.is_empty() {
        lines.push(unset_var(scope::PROJECT_STATE_ENV));
    } else {
        lines.push(export_var(
            scope::PROJECT_STATE_ENV,
            &scope::serialize_state(&project_state)?,
        ));
    }

    println!("{}", lines.join("\n"));
    Ok(())
}

fn sync_project(paths: &AswitchPaths) -> Result<()> {
    let local = scope::local_state();
    let current_project = scope::project_state();
    let binding = project::current_project_binding()?;
    let desired_accounts = binding
        .as_ref()
        .map(|item| item.accounts.clone())
        .unwrap_or_default();

    let mut lines = Vec::new();

    for plugin_id in current_project.keys() {
        if !desired_accounts.contains_key(plugin_id) && !local.contains_key(plugin_id) {
            if let Ok(output) = session::build_deactivation_output(paths, plugin_id) {
                lines.push(output.shell);
            }
        }
    }

    for (plugin_id, alias) in &desired_accounts {
        if local.contains_key(plugin_id) {
            continue;
        }

        let already_active = current_project
            .get(plugin_id)
            .map(|current| current == alias)
            .unwrap_or(false);
        if already_active {
            continue;
        }

        let activation = session::build_activation_output(paths, plugin_id, alias)?;
        lines.push(activation.shell);
    }

    if desired_accounts.is_empty() {
        lines.push(unset_var(scope::PROJECT_STATE_ENV));
        lines.push(unset_var(scope::PROJECT_FILE_ENV));
    } else {
        lines.push(export_var(
            scope::PROJECT_STATE_ENV,
            &scope::serialize_state(&desired_accounts)?,
        ));
        if let Some(binding) = binding {
            lines.push(export_var(
                scope::PROJECT_FILE_ENV,
                binding.path.to_string_lossy().as_ref(),
            ));
        }
    }

    println!("{}", lines.join("\n"));
    Ok(())
}

pub(crate) fn direct_use_requires_shell() -> Result<()> {
    bail!(
        "Shell scope requires shell integration. Run `eval \"$(aswitch init zsh)\"`, or use `aswitch use --scope project ...` / `aswitch use --scope global ...`."
    )
}

pub(crate) fn direct_clear_requires_shell() -> Result<()> {
    bail!(
        "Shell scope off requires shell integration. Run `eval \"$(aswitch init zsh)\"` and retry `aswitch use --off`."
    )
}

pub(crate) fn clear_global(paths: &AswitchPaths, plugin: Option<String>, json: bool) -> Result<()> {
    let plugin_ids = match plugin {
        Some(plugin_id) => vec![plugin_id],
        None => account::current_accounts_with_config_dir(Some(accounts::config_dir(paths)), None)?
            .into_iter()
            .filter_map(|item| item.alias.map(|_| item.plugin_id))
            .collect(),
    };

    if plugin_ids.is_empty() {
        bail!("No global active account was found; pass --plugin to clear a specific plugin");
    }

    let _lock = paths.lock_file(Duration::from_secs(5))?;
    let mut registry = Registry::load(paths)?;
    let mut reports = Vec::with_capacity(plugin_ids.len());

    for plugin_id in plugin_ids {
        clear_global_plugin(paths, &plugin_id)?;
        let previous_alias = registry.active.insert(plugin_id.clone(), None).flatten();
        reports.push(GlobalClearReport {
            plugin_id,
            previous_alias,
        });
    }

    registry.save_atomic(paths)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        for report in reports {
            if let Some(alias) = report.previous_alias {
                println!(
                    "Cleared global live credentials for {} (previous alias: {})",
                    report.plugin_id, alias
                );
            } else {
                println!("Cleared global live credentials for {}", report.plugin_id);
            }
        }
    }

    Ok(())
}

fn clear_global_plugin(paths: &AswitchPaths, plugin_id: &str) -> Result<()> {
    let manifest_path = paths.plugins_dir.join(plugin_id).join("plugin.toml");
    let manifest = load_manifest(&manifest_path)
        .with_context(|| format!("failed to load manifest for plugin {plugin_id}"))?;
    let store = store::resolve_active_store(&manifest.manifest.credential_store)?;
    store.clear_active()?;

    for aux_file in &manifest.manifest.aux_files {
        let path = paths::expand_user_path(&aux_file.path)?;
        remove_path_if_exists(&path)
            .with_context(|| format!("failed to remove live path {}", path.display()))?;
    }

    Ok(())
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }

    Ok(())
}

fn export_var(name: &str, value: &str) -> String {
    format!("export {name}={}", session::shell_quote(value))
}

fn unset_var(name: &str) -> String {
    format!("unset {name}")
}
