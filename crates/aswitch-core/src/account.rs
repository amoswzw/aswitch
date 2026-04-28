use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;

use crate::gemini;
use crate::identity::{self, Identity};
use crate::paths::{self, AswitchPaths, PathsError};
use crate::plugin::{self, load_all, PluginSource};
use crate::registry::{AccountMetadata, Registry, RegistryError};
use crate::store::{self, StoreResolveError};
use crate::switch::{self, SwitchFailure, LOCK_TIMEOUT};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AccountRecord {
    pub plugin_id: String,
    pub alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub added_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CurrentAccountRecord {
    pub plugin_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    pub managed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AddAccountReport {
    pub plugin_id: String,
    pub alias: String,
    pub overwritten: bool,
    pub saved_at: DateTime<Utc>,
    pub identity: Identity,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RenameAccountReport {
    pub plugin_id: String,
    pub old_alias: String,
    pub new_alias: String,
    pub active_updated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RemoveAccountReport {
    pub plugin_id: String,
    pub alias: String,
    pub removed_backup: bool,
    pub was_active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PluginStatusRow {
    pub plugin_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub loaded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_alias: Option<String>,
    pub account_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PluginStatusError {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    pub registry_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_switch_at: Option<DateTime<Utc>>,
    pub plugins: Vec<PluginStatusRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<PluginStatusError>,
}

#[derive(Debug, Error)]
pub enum AccountError {
    #[error("failed to resolve aswitch paths")]
    Paths(#[from] PathsError),
    #[error("failed to load or save registry")]
    Registry(#[from] RegistryError),
    #[error("failed to resolve credential store")]
    StoreResolve(#[from] StoreResolveError),
    #[error("failed to scan plugins")]
    PluginScan(#[from] plugin::PluginScanError),
    #[error(transparent)]
    Switch(#[from] SwitchFailure),
    #[error("account {plugin_id}/{alias} already exists; rerun with --force to overwrite")]
    AccountAlreadyExists { plugin_id: String, alias: String },
    #[error("plugin {plugin_id} has no active credentials to capture; run the native login first")]
    NoActiveCredentials { plugin_id: String },
    #[error("account {plugin_id}/{alias} does not exist")]
    AccountNotFound { plugin_id: String, alias: String },
    #[error("account {plugin_id}/{alias} cannot be renamed to the same alias")]
    AliasUnchanged { plugin_id: String, alias: String },
    #[error("account {plugin_id}/{alias} already exists")]
    TargetAliasExists { plugin_id: String, alias: String },
    #[error("cannot remove active account {plugin_id}/{alias} without --force")]
    RemoveActiveRequiresForce { plugin_id: String, alias: String },
    #[error("account backup path {path} is not a directory")]
    BackupPathNotDirectory { path: PathBuf },
    #[error("failed to remove path {path}")]
    RemovePath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to rename path from {from} to {to}")]
    RenamePath {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn add_account(
    plugin_id: &str,
    alias: &str,
    force: bool,
) -> Result<AddAccountReport, AccountError> {
    add_account_with_config_dir(plugin_id, alias, force, None)
}

pub fn add_account_with_config_dir(
    plugin_id: &str,
    alias: &str,
    force: bool,
    config_dir: Option<PathBuf>,
) -> Result<AddAccountReport, AccountError> {
    let home_dir = paths::home_dir()?;
    add_account_inner(plugin_id, alias, force, config_dir, &home_dir)
}

pub fn list_accounts(plugin_filter: Option<&str>) -> Result<Vec<AccountRecord>, AccountError> {
    list_accounts_with_config_dir(None, plugin_filter)
}

pub fn list_accounts_with_config_dir(
    config_dir: Option<PathBuf>,
    plugin_filter: Option<&str>,
) -> Result<Vec<AccountRecord>, AccountError> {
    let paths = AswitchPaths::resolve(config_dir)?;
    let registry = Registry::load(&paths)?;
    Ok(list_accounts_from_registry(&registry, plugin_filter))
}

pub fn current_accounts(
    plugin_filter: Option<&str>,
) -> Result<Vec<CurrentAccountRecord>, AccountError> {
    current_accounts_with_config_dir(None, plugin_filter)
}

pub fn current_accounts_with_config_dir(
    config_dir: Option<PathBuf>,
    plugin_filter: Option<&str>,
) -> Result<Vec<CurrentAccountRecord>, AccountError> {
    let paths = AswitchPaths::resolve(config_dir)?;
    let registry = Registry::load(&paths)?;
    let catalog = load_all(&paths.plugins_dir)?;
    let plugin_ids = plugin_ids(&registry, &catalog, plugin_filter);

    let mut current = Vec::with_capacity(plugin_ids.len());
    for plugin_id in plugin_ids {
        let active_alias = registry.active.get(&plugin_id).cloned().flatten();
        let metadata = active_alias.as_deref().and_then(|alias| {
            registry
                .accounts
                .get(&plugin_id)
                .and_then(|items| items.get(alias))
        });

        current.push(CurrentAccountRecord {
            plugin_id,
            alias: active_alias,
            email: metadata.and_then(|item| item.email.clone()),
            org_name: metadata.and_then(|item| item.org_name.clone()),
            plan: metadata.and_then(|item| item.plan.clone()),
            last_used_at: metadata.and_then(|item| item.last_used_at),
            managed: metadata.is_some(),
        });
    }

    Ok(current)
}

pub fn rename_account(
    plugin_id: &str,
    old_alias: &str,
    new_alias: &str,
) -> Result<RenameAccountReport, AccountError> {
    rename_account_with_config_dir(plugin_id, old_alias, new_alias, None)
}

pub fn rename_account_with_config_dir(
    plugin_id: &str,
    old_alias: &str,
    new_alias: &str,
    config_dir: Option<PathBuf>,
) -> Result<RenameAccountReport, AccountError> {
    let paths = AswitchPaths::resolve(config_dir)?;
    let _lock = paths.lock_file(LOCK_TIMEOUT)?;
    let mut registry = Registry::load(&paths)?;

    if old_alias == new_alias {
        return Err(AccountError::AliasUnchanged {
            plugin_id: plugin_id.to_string(),
            alias: old_alias.to_string(),
        });
    }

    let old_metadata = registry
        .accounts
        .get(plugin_id)
        .and_then(|items| items.get(old_alias))
        .cloned()
        .ok_or_else(|| AccountError::AccountNotFound {
            plugin_id: plugin_id.to_string(),
            alias: old_alias.to_string(),
        })?;

    if registry
        .accounts
        .get(plugin_id)
        .and_then(|items| items.get(new_alias))
        .is_some()
    {
        return Err(AccountError::TargetAliasExists {
            plugin_id: plugin_id.to_string(),
            alias: new_alias.to_string(),
        });
    }

    let plugin_dir = paths.accounts_dir.join(plugin_id);
    let old_path = plugin_dir.join(old_alias);
    let new_path = plugin_dir.join(new_alias);

    if old_path.exists() && !old_path.is_dir() {
        return Err(AccountError::BackupPathNotDirectory { path: old_path });
    }

    if new_path.exists() {
        return Err(AccountError::TargetAliasExists {
            plugin_id: plugin_id.to_string(),
            alias: new_alias.to_string(),
        });
    }

    let renamed_backup = if old_path.is_dir() {
        fs::rename(&old_path, &new_path).map_err(|source| AccountError::RenamePath {
            from: old_path.clone(),
            to: new_path.clone(),
            source,
        })?;
        true
    } else {
        false
    };

    let active_updated = registry
        .active
        .get(plugin_id)
        .and_then(|alias| alias.as_deref())
        == Some(old_alias);

    if let Some(accounts) = registry.accounts.get_mut(plugin_id) {
        accounts.remove(old_alias);
        let mut metadata = old_metadata;
        metadata.alias = new_alias.to_string();
        accounts.insert(new_alias.to_string(), metadata);
    }

    if active_updated {
        registry
            .active
            .insert(plugin_id.to_string(), Some(new_alias.to_string()));
    }

    if let Err(source) = registry.save_atomic(&paths) {
        if renamed_backup {
            let _ = fs::rename(&new_path, &old_path);
        }
        return Err(AccountError::Registry(source));
    }

    Ok(RenameAccountReport {
        plugin_id: plugin_id.to_string(),
        old_alias: old_alias.to_string(),
        new_alias: new_alias.to_string(),
        active_updated,
    })
}

pub fn remove_account(
    plugin_id: &str,
    alias: &str,
    force: bool,
) -> Result<RemoveAccountReport, AccountError> {
    remove_account_with_config_dir(plugin_id, alias, force, None)
}

pub fn remove_account_with_config_dir(
    plugin_id: &str,
    alias: &str,
    force: bool,
    config_dir: Option<PathBuf>,
) -> Result<RemoveAccountReport, AccountError> {
    let paths = AswitchPaths::resolve(config_dir)?;
    let _lock = paths.lock_file(LOCK_TIMEOUT)?;
    let mut registry = Registry::load(&paths)?;

    if registry
        .accounts
        .get(plugin_id)
        .and_then(|items| items.get(alias))
        .is_none()
    {
        return Err(AccountError::AccountNotFound {
            plugin_id: plugin_id.to_string(),
            alias: alias.to_string(),
        });
    }

    let was_active = registry
        .active
        .get(plugin_id)
        .and_then(|value| value.as_deref())
        == Some(alias);
    if was_active && !force {
        return Err(AccountError::RemoveActiveRequiresForce {
            plugin_id: plugin_id.to_string(),
            alias: alias.to_string(),
        });
    }

    let account_dir = paths.accounts_dir.join(plugin_id).join(alias);
    let removed_backup = if !account_dir.exists() {
        false
    } else if !account_dir.is_dir() {
        return Err(AccountError::BackupPathNotDirectory { path: account_dir });
    } else {
        fs::remove_dir_all(&account_dir).map_err(|source| AccountError::RemovePath {
            path: account_dir.clone(),
            source,
        })?;
        true
    };

    if let Some(accounts) = registry.accounts.get_mut(plugin_id) {
        accounts.remove(alias);
        if accounts.is_empty() {
            registry.accounts.remove(plugin_id);
        }
    }

    if was_active {
        registry.active.insert(plugin_id.to_string(), None);
    }

    registry.save_atomic(&paths)?;

    let plugin_dir = paths.accounts_dir.join(plugin_id);
    if plugin_dir.is_dir() {
        let is_empty = fs::read_dir(&plugin_dir)
            .map_err(|source| AccountError::RemovePath {
                path: plugin_dir.clone(),
                source,
            })?
            .next()
            .is_none();
        if is_empty {
            fs::remove_dir(&plugin_dir).map_err(|source| AccountError::RemovePath {
                path: plugin_dir.clone(),
                source,
            })?;
        }
    }

    Ok(RemoveAccountReport {
        plugin_id: plugin_id.to_string(),
        alias: alias.to_string(),
        removed_backup,
        was_active,
    })
}

pub fn status() -> Result<StatusReport, AccountError> {
    status_with_config_dir(None)
}

pub fn status_with_config_dir(config_dir: Option<PathBuf>) -> Result<StatusReport, AccountError> {
    let paths = AswitchPaths::resolve(config_dir)?;
    let registry = Registry::load(&paths)?;
    let catalog = load_all(&paths.plugins_dir)?;

    let plugin_ids = plugin_ids(&registry, &catalog, None);
    let mut plugins = Vec::with_capacity(plugin_ids.len());
    let mut last_switch_at = None;

    for plugin_id in plugin_ids {
        let manifest = catalog
            .plugins
            .iter()
            .find(|item| item.manifest.id == plugin_id);
        let last_used_at = registry
            .accounts
            .get(&plugin_id)
            .and_then(|items| items.values().filter_map(|item| item.last_used_at).max());

        if last_switch_at < last_used_at {
            last_switch_at = last_used_at;
        }

        plugins.push(PluginStatusRow {
            plugin_id: plugin_id.clone(),
            display_name: manifest.map(|item| item.manifest.display_name.clone()),
            version: manifest.map(|item| item.manifest.version.clone()),
            source: manifest.map(|item| plugin_source_label(item.source)),
            loaded: manifest.is_some(),
            active_alias: registry.active.get(&plugin_id).cloned().flatten(),
            account_count: registry
                .accounts
                .get(&plugin_id)
                .map_or(0, std::collections::BTreeMap::len),
            last_used_at,
            warnings: manifest
                .map(|item| item.warnings.clone())
                .unwrap_or_default(),
        });
    }

    Ok(StatusReport {
        registry_version: registry.version,
        last_switch_at,
        plugins,
        warnings: catalog.warnings,
        errors: catalog
            .errors
            .into_iter()
            .map(|item| PluginStatusError {
                path: item.path,
                error: item.error,
            })
            .collect(),
    })
}

fn add_account_inner(
    plugin_id: &str,
    alias: &str,
    force: bool,
    config_dir: Option<PathBuf>,
    home_dir: &Path,
) -> Result<AddAccountReport, AccountError> {
    let paths = AswitchPaths::resolve(config_dir)?;
    let _lock = paths.lock_file(LOCK_TIMEOUT)?;
    let manifest = switch::load_plugin_manifest(&paths, plugin_id)?;
    let store = store::resolve_active_store_with_home_dir(
        &manifest.credential_store,
        home_dir.to_path_buf(),
    )?;
    let mut registry = Registry::load(&paths)?;

    let existing_metadata = registry
        .accounts
        .get(plugin_id)
        .and_then(|items| items.get(alias))
        .cloned();

    if existing_metadata.is_some() && !force {
        return Err(AccountError::AccountAlreadyExists {
            plugin_id: plugin_id.to_string(),
            alias: alias.to_string(),
        });
    }

    let live_state = switch::capture_live_state(&manifest, &store, home_dir)?;
    let credential_bytes =
        live_state
            .primary_credentials()
            .ok_or_else(|| AccountError::NoActiveCredentials {
                plugin_id: plugin_id.to_string(),
            })?;
    let extraction = identity::extract_with_home_dir(&manifest, credential_bytes, home_dir);

    switch::save_account_state(&paths, plugin_id, alias, &live_state)?;

    let saved_at = Utc::now();
    let mut identity = extraction.identity;
    let mut warnings = extraction.warnings;
    if plugin_id == gemini::GEMINI_PLUGIN_ID {
        match gemini::fetch_code_assist_info(credential_bytes) {
            Ok(info) => {
                if identity.org_name.is_none() {
                    identity.org_name = info.project_id;
                }
                if identity.plan.is_none() {
                    identity.plan = info.tier_name;
                }
            }
            Err(error) => warnings.push(format!(
                "failed to fetch live Gemini plan metadata: {error}"
            )),
        }
    }
    let overwritten = existing_metadata.is_some();
    let added_at = existing_metadata
        .as_ref()
        .map(|item| item.added_at)
        .unwrap_or(saved_at);

    registry
        .accounts
        .entry(plugin_id.to_string())
        .or_default()
        .insert(
            alias.to_string(),
            AccountMetadata {
                alias: alias.to_string(),
                email: identity.email.clone(),
                org_name: identity.org_name.clone(),
                plan: identity.plan.clone(),
                added_at,
                last_used_at: Some(saved_at),
            },
        );
    registry
        .active
        .insert(plugin_id.to_string(), Some(alias.to_string()));
    registry.save_atomic(&paths)?;

    Ok(AddAccountReport {
        plugin_id: plugin_id.to_string(),
        alias: alias.to_string(),
        overwritten,
        saved_at,
        identity,
        warnings,
    })
}

fn list_accounts_from_registry(
    registry: &Registry,
    plugin_filter: Option<&str>,
) -> Vec<AccountRecord> {
    let mut accounts = Vec::new();

    for (plugin_id, items) in &registry.accounts {
        if plugin_filter.is_some_and(|filter| filter != plugin_id) {
            continue;
        }

        let active_alias = registry
            .active
            .get(plugin_id)
            .and_then(|value| value.as_deref());
        for (alias, metadata) in items {
            accounts.push(AccountRecord {
                plugin_id: plugin_id.clone(),
                alias: alias.clone(),
                email: metadata.email.clone(),
                org_name: metadata.org_name.clone(),
                plan: metadata.plan.clone(),
                added_at: metadata.added_at,
                last_used_at: metadata.last_used_at,
                active: active_alias == Some(alias.as_str()),
            });
        }
    }

    accounts
}

fn plugin_ids(
    registry: &Registry,
    catalog: &plugin::PluginCatalog,
    plugin_filter: Option<&str>,
) -> Vec<String> {
    let mut ids = BTreeSet::new();
    ids.extend(registry.accounts.keys().cloned());
    ids.extend(registry.active.keys().cloned());
    ids.extend(catalog.plugins.iter().map(|item| item.manifest.id.clone()));

    ids.into_iter()
        .filter(|plugin_id| plugin_filter.is_none_or(|filter| filter == plugin_id))
        .collect()
}

fn plugin_source_label(source: PluginSource) -> String {
    match source {
        PluginSource::User => "user".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{
        add_account_inner, current_accounts_with_config_dir, list_accounts_with_config_dir,
        remove_account_with_config_dir, rename_account_with_config_dir, status_with_config_dir,
        AccountError,
    };
    use crate::paths::AswitchPaths;
    use crate::registry::Registry;

    struct TestHarness {
        _temp_dir: tempfile::TempDir,
        home_dir: PathBuf,
        config_dir: PathBuf,
        paths: AswitchPaths,
        plugin_id: String,
    }

    impl TestHarness {
        fn new() -> Self {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let home_dir = temp_dir.path().join("home");
            let config_dir = temp_dir.path().join(".aswitch");
            let paths = AswitchPaths::resolve(Some(config_dir.clone())).expect("paths");
            paths.ensure().expect("ensure");
            fs::create_dir_all(&home_dir).expect("home dir");

            Self {
                _temp_dir: temp_dir,
                home_dir,
                config_dir,
                paths,
                plugin_id: "demo".to_string(),
            }
        }

        fn write_manifest(&self) {
            let plugin_dir = self.paths.plugins_dir.join(&self.plugin_id);
            fs::create_dir_all(&plugin_dir).expect("plugin dir");
            fs::write(
                plugin_dir.join("plugin.toml"),
                r#"
id = "demo"
display_name = "Demo"
version = "1.0.0"
author = "tests"
description = "account tests"
platforms = ["macos", "linux"]

[credential_store]
kind = "file"
path = "~/.demo/auth.json"
permissions = 384

[[aux_files]]
path = "~/.demo/settings.json"
required = false
kind = "file"

[[identity_extract]]
field = "email"
source = "json_value"
json_pointer = "user.email"

[[identity_extract]]
field = "plan"
source = "json_value"
json_pointer = "plan"

[login]
cmd = ["demo", "login"]
ready_marker_kind = "file"
ready_marker_timeout_s = 1
ready_marker_path = "~/.demo/auth.json"
"#,
            )
            .expect("write manifest");
        }

        fn active_auth_path(&self) -> PathBuf {
            self.home_dir.join(".demo/auth.json")
        }

        fn active_settings_path(&self) -> PathBuf {
            self.home_dir.join(".demo/settings.json")
        }

        fn set_live_state(&self, auth: &[u8], settings: Option<&[u8]>) {
            fs::create_dir_all(self.home_dir.join(".demo")).expect("live dir");
            fs::write(self.active_auth_path(), auth).expect("write auth");
            if let Some(settings) = settings {
                fs::write(self.active_settings_path(), settings).expect("write settings");
            } else if self.active_settings_path().exists() {
                fs::remove_file(self.active_settings_path()).expect("remove settings");
            }
        }
    }

    #[test]
    fn add_account_captures_live_state_and_marks_it_active() {
        let harness = TestHarness::new();
        harness.write_manifest();
        harness.set_live_state(
            br#"{"user":{"email":"amos@example.com"},"plan":"plus"}"#,
            Some(br#"{"theme":"dark"}"#),
        );

        let report = add_account_inner(
            &harness.plugin_id,
            "work",
            false,
            Some(harness.config_dir.clone()),
            &harness.home_dir,
        )
        .expect("add account");

        assert_eq!(report.alias, "work");
        assert_eq!(report.identity.email.as_deref(), Some("amos@example.com"));
        assert_eq!(report.identity.plan.as_deref(), Some("plus"));

        let registry = Registry::load(&harness.paths).expect("load registry");
        let metadata = registry
            .accounts
            .get(&harness.plugin_id)
            .and_then(|items| items.get("work"))
            .expect("metadata");
        assert_eq!(metadata.email.as_deref(), Some("amos@example.com"));
        assert_eq!(
            registry.active.get(&harness.plugin_id).cloned().flatten(),
            Some("work".to_string())
        );

        let account_dir = harness
            .paths
            .accounts_dir
            .join(&harness.plugin_id)
            .join("work");
        assert_eq!(
            fs::read(account_dir.join("primary.creds")).expect("primary creds"),
            br#"{"user":{"email":"amos@example.com"},"plan":"plus"}"#
        );
        assert_eq!(
            fs::read(account_dir.join("aux/.demo/settings.json")).expect("settings backup"),
            br#"{"theme":"dark"}"#
        );
    }

    #[test]
    fn list_and_current_reflect_saved_accounts() {
        let harness = TestHarness::new();
        harness.write_manifest();

        harness.set_live_state(
            br#"{"user":{"email":"personal@example.com"},"plan":"free"}"#,
            None,
        );
        add_account_inner(
            &harness.plugin_id,
            "personal",
            false,
            Some(harness.config_dir.clone()),
            &harness.home_dir,
        )
        .expect("add personal");

        harness.set_live_state(
            br#"{"user":{"email":"work@example.com"},"plan":"team"}"#,
            None,
        );
        add_account_inner(
            &harness.plugin_id,
            "work",
            false,
            Some(harness.config_dir.clone()),
            &harness.home_dir,
        )
        .expect("add work");

        let accounts =
            list_accounts_with_config_dir(Some(harness.config_dir.clone()), None).expect("list");
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].alias, "personal");
        assert!(!accounts[0].active);
        assert_eq!(accounts[1].alias, "work");
        assert!(accounts[1].active);

        let current = current_accounts_with_config_dir(Some(harness.config_dir.clone()), None)
            .expect("current");
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].alias.as_deref(), Some("work"));
        assert_eq!(current[0].email.as_deref(), Some("work@example.com"));
        assert!(current[0].managed);
    }

    #[test]
    fn rename_account_moves_backup_and_updates_active_alias() {
        let harness = TestHarness::new();
        harness.write_manifest();
        harness.set_live_state(
            br#"{"user":{"email":"amos@example.com"},"plan":"plus"}"#,
            None,
        );
        add_account_inner(
            &harness.plugin_id,
            "work",
            false,
            Some(harness.config_dir.clone()),
            &harness.home_dir,
        )
        .expect("add work");

        let report = rename_account_with_config_dir(
            &harness.plugin_id,
            "work",
            "office",
            Some(harness.config_dir.clone()),
        )
        .expect("rename");

        assert!(report.active_updated);
        assert!(!harness
            .paths
            .accounts_dir
            .join(&harness.plugin_id)
            .join("work")
            .exists());
        assert!(harness
            .paths
            .accounts_dir
            .join(&harness.plugin_id)
            .join("office")
            .is_dir());

        let registry = Registry::load(&harness.paths).expect("load registry");
        assert_eq!(
            registry.active.get(&harness.plugin_id).cloned().flatten(),
            Some("office".to_string())
        );
        assert!(registry
            .accounts
            .get(&harness.plugin_id)
            .and_then(|items| items.get("office"))
            .is_some());
    }

    #[test]
    fn remove_active_account_requires_force_and_clears_active_pointer() {
        let harness = TestHarness::new();
        harness.write_manifest();
        harness.set_live_state(
            br#"{"user":{"email":"amos@example.com"},"plan":"plus"}"#,
            None,
        );
        add_account_inner(
            &harness.plugin_id,
            "work",
            false,
            Some(harness.config_dir.clone()),
            &harness.home_dir,
        )
        .expect("add work");

        let error = remove_account_with_config_dir(
            &harness.plugin_id,
            "work",
            false,
            Some(harness.config_dir.clone()),
        )
        .expect_err("remove without force should fail");
        assert!(matches!(
            error,
            AccountError::RemoveActiveRequiresForce { .. }
        ));

        let report = remove_account_with_config_dir(
            &harness.plugin_id,
            "work",
            true,
            Some(harness.config_dir.clone()),
        )
        .expect("remove with force");
        assert!(report.was_active);
        assert!(report.removed_backup);

        let registry = Registry::load(&harness.paths).expect("load registry");
        assert_eq!(
            registry.active.get(&harness.plugin_id).cloned().flatten(),
            None
        );
        assert!(registry.accounts.get(&harness.plugin_id).is_none());
        assert!(!harness.paths.accounts_dir.join(&harness.plugin_id).exists());
    }

    #[test]
    fn status_includes_loaded_plugin_and_last_switch() {
        let harness = TestHarness::new();
        harness.write_manifest();
        harness.set_live_state(
            br#"{"user":{"email":"amos@example.com"},"plan":"plus"}"#,
            None,
        );
        add_account_inner(
            &harness.plugin_id,
            "work",
            false,
            Some(harness.config_dir.clone()),
            &harness.home_dir,
        )
        .expect("add work");

        let status = status_with_config_dir(Some(harness.config_dir.clone())).expect("status");
        assert_eq!(status.registry_version, 1);
        assert!(status.last_switch_at.is_some());
        assert_eq!(status.plugins.len(), 1);
        assert_eq!(status.plugins[0].plugin_id, "demo");
        assert!(status.plugins[0].loaded);
        assert_eq!(status.plugins[0].active_alias.as_deref(), Some("work"));
        assert_eq!(status.plugins[0].account_count, 1);
    }
}
