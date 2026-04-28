use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use super::{live, project};
use aswitch_core::account::{self, AccountRecord};
use aswitch_core::paths::AswitchPaths;

pub(crate) const LOCAL_STATE_ENV: &str = "ASWITCH_LOCAL_STATE";
pub(crate) const PROJECT_STATE_ENV: &str = "ASWITCH_PROJECT_STATE";
pub(crate) const PROJECT_FILE_ENV: &str = "ASWITCH_PROJECT_FILE";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ScopeSource {
    Local,
    Project,
    Global,
    None,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct EffectiveAccountRecord {
    pub plugin_id: String,
    pub alias: Option<String>,
    pub email: Option<String>,
    pub org_name: Option<String>,
    pub plan: Option<String>,
    pub scope: ScopeSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub(crate) fn local_state() -> BTreeMap<String, String> {
    read_state_env(LOCAL_STATE_ENV)
}

pub(crate) fn project_state() -> BTreeMap<String, String> {
    read_state_env(PROJECT_STATE_ENV)
}

pub(crate) fn project_file_from_env() -> Option<PathBuf> {
    env::var_os(PROJECT_FILE_ENV).map(PathBuf::from)
}

pub(crate) fn serialize_state(state: &BTreeMap<String, String>) -> Result<String> {
    Ok(serde_json::to_string(state)?)
}

pub(crate) fn effective_accounts(
    paths: &AswitchPaths,
    plugin_filter: Option<&str>,
) -> Result<Vec<EffectiveAccountRecord>> {
    effective_accounts_with_options(paths, plugin_filter, true)
}

pub(crate) fn effective_accounts_offline(
    paths: &AswitchPaths,
    plugin_filter: Option<&str>,
) -> Result<Vec<EffectiveAccountRecord>> {
    effective_accounts_with_options(paths, plugin_filter, false)
}

fn effective_accounts_with_options(
    paths: &AswitchPaths,
    plugin_filter: Option<&str>,
    live_enrich: bool,
) -> Result<Vec<EffectiveAccountRecord>> {
    let all_accounts = account::list_accounts_with_config_dir(Some(paths.root.clone()), None)?;
    let local = local_state();
    let project_binding = project::current_project_binding()?;
    let project = project_binding
        .as_ref()
        .map(|binding| binding.accounts.clone())
        .unwrap_or_default();
    let project_file = project_binding
        .as_ref()
        .map(|binding| binding.path.clone())
        .or_else(project_file_from_env);

    let mut plugins = BTreeMap::<String, Option<String>>::new();
    for item in account::current_accounts_with_config_dir(Some(paths.root.clone()), plugin_filter)?
    {
        plugins.insert(item.plugin_id, item.alias);
    }
    for plugin_id in local.keys() {
        if plugin_filter
            .map(|filter| filter == plugin_id)
            .unwrap_or(true)
        {
            plugins.entry(plugin_id.clone()).or_insert(None);
        }
    }
    for plugin_id in project.keys() {
        if plugin_filter
            .map(|filter| filter == plugin_id)
            .unwrap_or(true)
        {
            plugins.entry(plugin_id.clone()).or_insert(None);
        }
    }

    let mut rows = Vec::with_capacity(plugins.len());
    for plugin_id in plugins.keys() {
        let (alias, scope, detail) = if let Some(alias) = local.get(plugin_id) {
            (
                Some(alias.clone()),
                ScopeSource::Local,
                Some("current shell override".to_string()),
            )
        } else if let Some(alias) = project.get(plugin_id) {
            let detail = project_file
                .as_ref()
                .map(|path| format!("project file: {}", path.display()));
            (Some(alias.clone()), ScopeSource::Project, detail)
        } else {
            let alias = plugins.get(plugin_id).cloned().flatten();
            let scope = if alias.is_some() {
                ScopeSource::Global
            } else {
                ScopeSource::None
            };
            (alias, scope, None)
        };

        let metadata = alias
            .as_deref()
            .and_then(|current| find_account(&all_accounts, plugin_id, current));

        rows.push(EffectiveAccountRecord {
            plugin_id: plugin_id.clone(),
            alias,
            email: metadata.and_then(|item| item.email.clone()),
            org_name: metadata.and_then(|item| item.org_name.clone()),
            plan: metadata.and_then(|item| item.plan.clone()),
            scope,
            detail,
        });
    }

    if live_enrich {
        live::enrich_effective_accounts(&mut rows);
    }

    Ok(rows)
}

pub(crate) fn effective_targets_offline(
    paths: &AswitchPaths,
    plugin_filter: Option<&str>,
) -> Result<Vec<(String, String)>> {
    Ok(effective_accounts_offline(paths, plugin_filter)?
        .into_iter()
        .filter_map(|item| item.alias.map(|alias| (item.plugin_id, alias)))
        .collect())
}

pub(crate) fn effective_targets(
    paths: &AswitchPaths,
    plugin_filter: Option<&str>,
) -> Result<Vec<(String, String)>> {
    Ok(effective_accounts(paths, plugin_filter)?
        .into_iter()
        .filter_map(|item| item.alias.map(|alias| (item.plugin_id, alias)))
        .collect())
}

fn read_state_env(name: &str) -> BTreeMap<String, String> {
    env::var(name)
        .ok()
        .and_then(|value| serde_json::from_str::<BTreeMap<String, String>>(&value).ok())
        .unwrap_or_default()
}

fn find_account<'a>(
    accounts: &'a [AccountRecord],
    plugin_id: &str,
    alias: &str,
) -> Option<&'a AccountRecord> {
    accounts
        .iter()
        .find(|item| item.plugin_id == plugin_id && item.alias == alias)
}
