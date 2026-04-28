use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;

use crate::paths::{self, AswitchPaths, PathsError};
use crate::plugin::{self, AuxFileKind, Manifest};
use crate::registry::{Registry, RegistryError};
use crate::store::{self, CredentialStore, ResolvedCredentialStore, StoreError, StoreResolveError};

pub(crate) const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const PRIMARY_CREDENTIALS_FILE: &str = "primary.creds";
const AUX_DIR: &str = "aux";
const FILE_MODE: u32 = 0o600;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SwitchReport {
    pub plugin_id: String,
    pub alias: String,
    pub previous_active: Option<String>,
    pub switched_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum SwitchError {
    #[error(transparent)]
    Failed(#[from] SwitchFailure),
    #[error("switch failed but rollback completed: {cause}")]
    RollbackSucceeded { cause: Box<SwitchFailure> },
    #[error("switch failed and rollback failed: {cause}; rollback error: {rollback}")]
    RollbackFailed {
        cause: Box<SwitchFailure>,
        rollback: String,
    },
}

impl SwitchError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::RollbackSucceeded { .. } => 10,
            Self::RollbackFailed { .. } => 11,
            Self::Failed(_) => 1,
        }
    }
}

#[derive(Debug, Error)]
pub enum SwitchFailure {
    #[error("failed to resolve aswitch paths")]
    Paths(#[from] PathsError),
    #[error("failed to load plugin manifest")]
    Manifest(#[from] plugin::ManifestLoadError),
    #[error("failed to load or save registry")]
    Registry(#[from] RegistryError),
    #[error("failed to resolve credential store")]
    StoreResolve(#[from] StoreResolveError),
    #[error("credential store operation failed")]
    Store(#[from] StoreError),
    #[error("plugin {plugin_id} manifest not found at {path}")]
    PluginNotFound { plugin_id: String, path: PathBuf },
    #[error("target account {plugin_id}/{alias} does not exist in registry")]
    TargetAccountNotFound { plugin_id: String, alias: String },
    #[error("account backup for {plugin_id}/{alias} is missing at {path}")]
    TargetBackupMissing {
        plugin_id: String,
        alias: String,
        path: PathBuf,
    },
    #[error("required auxiliary path {path} is missing")]
    MissingRequiredPath { path: PathBuf },
    #[error("expected a file at {path}")]
    ExpectedFile { path: PathBuf },
    #[error("expected a directory at {path}")]
    ExpectedDirectory { path: PathBuf },
    #[error("failed to read file {path}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create directory {path}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write file {path}")]
    WriteFile {
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
    #[error("failed to remove path {path}")]
    RemovePath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("simulated failure after writing active credentials")]
    SimulatedFailureAfterPrimaryWrite,
}

#[derive(Clone, Copy, Debug, Default)]
struct SwitchHooks {
    fail_after_primary_write: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StoredAccountState {
    credentials: Option<Vec<u8>>,
    aux: Vec<StoredAuxState>,
}

impl StoredAccountState {
    pub(crate) fn primary_credentials(&self) -> Option<&[u8]> {
        self.credentials.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredAuxState {
    active_path: PathBuf,
    backup_relative_path: PathBuf,
    content: SnapshotContent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SnapshotContent {
    Missing,
    File(Vec<u8>),
    Dir(DirectorySnapshot),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DirectorySnapshot {
    files: Vec<DirectoryFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryFile {
    relative_path: PathBuf,
    bytes: Vec<u8>,
}

pub fn use_account(plugin_id: &str, alias: &str) -> Result<SwitchReport, SwitchError> {
    use_account_with_config_dir(plugin_id, alias, None)
}

pub fn use_account_with_config_dir(
    plugin_id: &str,
    alias: &str,
    config_dir: Option<PathBuf>,
) -> Result<SwitchReport, SwitchError> {
    let home_dir = paths::home_dir().map_err(SwitchFailure::from)?;
    use_account_inner(
        plugin_id,
        alias,
        config_dir,
        &home_dir,
        SwitchHooks::default(),
    )
}

fn use_account_inner(
    plugin_id: &str,
    alias: &str,
    config_dir: Option<PathBuf>,
    home_dir: &Path,
    hooks: SwitchHooks,
) -> Result<SwitchReport, SwitchError> {
    let paths = AswitchPaths::resolve(config_dir).map_err(SwitchFailure::from)?;
    let _lock = paths.lock_file(LOCK_TIMEOUT).map_err(SwitchFailure::from)?;
    let manifest = load_plugin_manifest(&paths, plugin_id)?;
    let store = store::resolve_active_store_with_home_dir(
        &manifest.credential_store,
        home_dir.to_path_buf(),
    )
    .map_err(SwitchFailure::from)?;
    let mut registry = Registry::load(&paths).map_err(SwitchFailure::from)?;

    if !registry
        .accounts
        .get(plugin_id)
        .and_then(|accounts| accounts.get(alias))
        .is_some()
    {
        return Err(SwitchFailure::TargetAccountNotFound {
            plugin_id: plugin_id.to_string(),
            alias: alias.to_string(),
        }
        .into());
    }

    let previous_active = registry.active.get(plugin_id).cloned().flatten();
    let previous_live_state =
        capture_live_state(&manifest, &store, home_dir).map_err(SwitchError::from)?;

    if let Some(current_alias) = previous_active.as_deref() {
        save_account_state(&paths, plugin_id, current_alias, &previous_live_state)
            .map_err(SwitchError::from)?;
    }

    let target_state = read_account_state(&paths, plugin_id, alias, &manifest, &store, home_dir)
        .map_err(SwitchError::from)?;

    if let Err(cause) = apply_account_state_to_live(&store, &target_state, hooks) {
        return Err(rollback_after_failure(cause, &store, &previous_live_state));
    }

    let switched_at = Utc::now();
    registry
        .active
        .insert(plugin_id.to_string(), Some(alias.to_string()));
    if let Some(metadata) = registry
        .accounts
        .get_mut(plugin_id)
        .and_then(|accounts| accounts.get_mut(alias))
    {
        metadata.last_used_at = Some(switched_at);
    }

    if let Err(cause) = registry.save_atomic(&paths).map_err(SwitchFailure::from) {
        return Err(rollback_after_failure(cause, &store, &previous_live_state));
    }

    Ok(SwitchReport {
        plugin_id: plugin_id.to_string(),
        alias: alias.to_string(),
        previous_active,
        switched_at,
    })
}

fn rollback_after_failure(
    cause: SwitchFailure,
    store: &ResolvedCredentialStore,
    previous_live_state: &StoredAccountState,
) -> SwitchError {
    match apply_account_state_to_live(store, previous_live_state, SwitchHooks::default()) {
        Ok(()) => SwitchError::RollbackSucceeded {
            cause: Box::new(cause),
        },
        Err(rollback) => SwitchError::RollbackFailed {
            cause: Box::new(cause),
            rollback: rollback.to_string(),
        },
    }
}

pub(crate) fn load_plugin_manifest(
    paths: &AswitchPaths,
    plugin_id: &str,
) -> Result<Manifest, SwitchFailure> {
    let manifest_path = paths.plugins_dir.join(plugin_id).join("plugin.toml");
    if !manifest_path.is_file() {
        return Err(SwitchFailure::PluginNotFound {
            plugin_id: plugin_id.to_string(),
            path: manifest_path,
        });
    }

    Ok(plugin::load_manifest(&manifest_path)?.manifest)
}

pub(crate) fn capture_live_state(
    manifest: &Manifest,
    store: &ResolvedCredentialStore,
    home_dir: &Path,
) -> Result<StoredAccountState, SwitchFailure> {
    let credentials = capture_live_credentials(store)?;
    let mut aux = Vec::with_capacity(manifest.aux_files.len());

    for aux_file in &manifest.aux_files {
        let active_path = paths::expand_user_path_from(&aux_file.path, home_dir);
        let backup_relative_path = backup_relative_path(&active_path, home_dir);
        let content = read_snapshot_content(&active_path, aux_file.kind, aux_file.required)?;
        aux.push(StoredAuxState {
            active_path,
            backup_relative_path,
            content,
        });
    }

    Ok(StoredAccountState { credentials, aux })
}

fn capture_live_credentials(
    store: &ResolvedCredentialStore,
) -> Result<Option<Vec<u8>>, SwitchFailure> {
    if store.exists()? {
        return Ok(Some(store.read_active()?));
    }

    if store.allows_missing_active() {
        Ok(None)
    } else {
        Ok(Some(store.read_active()?))
    }
}

fn read_account_state(
    paths: &AswitchPaths,
    plugin_id: &str,
    alias: &str,
    manifest: &Manifest,
    store: &ResolvedCredentialStore,
    home_dir: &Path,
) -> Result<StoredAccountState, SwitchFailure> {
    let account_dir = paths.accounts_dir.join(plugin_id).join(alias);
    if !account_dir.is_dir() {
        return Err(SwitchFailure::TargetBackupMissing {
            plugin_id: plugin_id.to_string(),
            alias: alias.to_string(),
            path: account_dir,
        });
    }

    let primary_path = account_dir.join(PRIMARY_CREDENTIALS_FILE);
    let credentials = if primary_path.is_file() {
        Some(read_file_bytes(&primary_path)?)
    } else if !primary_path.exists() && store.allows_missing_active() {
        None
    } else if primary_path.exists() {
        return Err(SwitchFailure::ExpectedFile { path: primary_path });
    } else {
        return Err(SwitchFailure::TargetBackupMissing {
            plugin_id: plugin_id.to_string(),
            alias: alias.to_string(),
            path: primary_path,
        });
    };

    let mut aux = Vec::with_capacity(manifest.aux_files.len());
    for aux_file in &manifest.aux_files {
        let active_path = paths::expand_user_path_from(&aux_file.path, home_dir);
        let backup_relative_path = backup_relative_path(&active_path, home_dir);
        let backup_path = account_dir.join(AUX_DIR).join(&backup_relative_path);
        let content = read_snapshot_content(&backup_path, aux_file.kind, aux_file.required)?;
        aux.push(StoredAuxState {
            active_path,
            backup_relative_path,
            content,
        });
    }

    Ok(StoredAccountState { credentials, aux })
}

pub(crate) fn save_account_state(
    paths: &AswitchPaths,
    plugin_id: &str,
    alias: &str,
    state: &StoredAccountState,
) -> Result<(), SwitchFailure> {
    let plugin_dir = paths.accounts_dir.join(plugin_id);
    create_directory_all(&plugin_dir)?;

    let destination = plugin_dir.join(alias);
    let staging = temp_sibling_path(&destination, "tmp");
    remove_path_if_exists(&staging)?;
    create_directory_all(&staging)?;

    if let Some(bytes) = &state.credentials {
        write_file_atomic(&staging.join(PRIMARY_CREDENTIALS_FILE), bytes, FILE_MODE)?;
    }

    for aux in &state.aux {
        let backup_path = staging.join(AUX_DIR).join(&aux.backup_relative_path);
        materialize_content(&backup_path, &aux.content)?;
    }

    replace_path_atomically(&staging, &destination)
}

fn apply_account_state_to_live(
    store: &ResolvedCredentialStore,
    state: &StoredAccountState,
    hooks: SwitchHooks,
) -> Result<(), SwitchFailure> {
    match &state.credentials {
        Some(bytes) => store.write_active(bytes)?,
        None => store.clear_active()?,
    }

    if hooks.fail_after_primary_write {
        return Err(SwitchFailure::SimulatedFailureAfterPrimaryWrite);
    }

    for aux in &state.aux {
        apply_content_to_live(&aux.active_path, &aux.content)?;
    }

    Ok(())
}

fn read_snapshot_content(
    path: &Path,
    kind: AuxFileKind,
    required: bool,
) -> Result<SnapshotContent, SwitchFailure> {
    match kind {
        AuxFileKind::File => {
            if path.is_file() {
                return Ok(SnapshotContent::File(read_file_bytes(path)?));
            }
            if !path.exists() && !required {
                return Ok(SnapshotContent::Missing);
            }
            if !path.exists() {
                return Err(SwitchFailure::MissingRequiredPath {
                    path: path.to_path_buf(),
                });
            }
            Err(SwitchFailure::ExpectedFile {
                path: path.to_path_buf(),
            })
        }
        AuxFileKind::Dir => {
            if path.is_dir() {
                return Ok(SnapshotContent::Dir(read_directory_snapshot(path)?));
            }
            if !path.exists() && !required {
                return Ok(SnapshotContent::Missing);
            }
            if !path.exists() {
                return Err(SwitchFailure::MissingRequiredPath {
                    path: path.to_path_buf(),
                });
            }
            Err(SwitchFailure::ExpectedDirectory {
                path: path.to_path_buf(),
            })
        }
    }
}

fn read_directory_snapshot(path: &Path) -> Result<DirectorySnapshot, SwitchFailure> {
    let mut files = Vec::new();
    collect_directory_files(path, path, &mut files)?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(DirectorySnapshot { files })
}

fn collect_directory_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<DirectoryFile>,
) -> Result<(), SwitchFailure> {
    let mut entries = fs::read_dir(current)
        .map_err(|source| SwitchFailure::ReadFile {
            path: current.to_path_buf(),
            source,
        })?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_directory_files(root, &path, files)?;
        } else if path.is_file() {
            let relative_path = path
                .strip_prefix(root)
                .expect("directory traversal should stay under root")
                .to_path_buf();
            files.push(DirectoryFile {
                relative_path,
                bytes: read_file_bytes(&path)?,
            });
        } else {
            return Err(SwitchFailure::ExpectedFile { path });
        }
    }

    Ok(())
}

fn materialize_content(path: &Path, content: &SnapshotContent) -> Result<(), SwitchFailure> {
    match content {
        SnapshotContent::Missing => Ok(()),
        SnapshotContent::File(bytes) => write_file_atomic(path, bytes, FILE_MODE),
        SnapshotContent::Dir(snapshot) => write_directory_contents(path, snapshot),
    }
}

fn apply_content_to_live(path: &Path, content: &SnapshotContent) -> Result<(), SwitchFailure> {
    match content {
        SnapshotContent::Missing => remove_path_if_exists(path),
        SnapshotContent::File(bytes) => {
            if path.is_dir() {
                return Err(SwitchFailure::ExpectedFile {
                    path: path.to_path_buf(),
                });
            }
            write_file_atomic(path, bytes, FILE_MODE)
        }
        SnapshotContent::Dir(snapshot) => replace_directory_with_snapshot(path, snapshot),
    }
}

fn write_directory_contents(
    path: &Path,
    snapshot: &DirectorySnapshot,
) -> Result<(), SwitchFailure> {
    create_directory_all(path)?;
    for file in &snapshot.files {
        write_file_atomic(&path.join(&file.relative_path), &file.bytes, FILE_MODE)?;
    }
    Ok(())
}

fn replace_directory_with_snapshot(
    path: &Path,
    snapshot: &DirectorySnapshot,
) -> Result<(), SwitchFailure> {
    let staging = temp_sibling_path(path, "tmp");
    remove_path_if_exists(&staging)?;
    create_directory_all(&staging)?;
    write_directory_contents(&staging, snapshot)?;
    replace_path_atomically(&staging, path)
}

fn replace_path_atomically(staging: &Path, destination: &Path) -> Result<(), SwitchFailure> {
    let parent = destination
        .parent()
        .ok_or_else(|| SwitchFailure::CreateDirectory {
            path: destination.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no parent directory",
            ),
        })?;
    create_directory_all(parent)?;

    let previous = temp_sibling_path(destination, "old");
    remove_path_if_exists(&previous)?;
    let had_previous = destination.exists();

    if had_previous {
        fs::rename(destination, &previous).map_err(|source| SwitchFailure::RenamePath {
            from: destination.to_path_buf(),
            to: previous.clone(),
            source,
        })?;
    }

    if let Err(source) = fs::rename(staging, destination) {
        if had_previous {
            let _ = fs::rename(&previous, destination);
        }
        return Err(SwitchFailure::RenamePath {
            from: staging.to_path_buf(),
            to: destination.to_path_buf(),
            source,
        });
    }

    if had_previous {
        remove_path_if_exists(&previous)?;
    }

    Ok(())
}

fn write_file_atomic(path: &Path, bytes: &[u8], permissions: u32) -> Result<(), SwitchFailure> {
    let parent = path
        .parent()
        .ok_or_else(|| SwitchFailure::CreateDirectory {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no parent directory",
            ),
        })?;
    create_directory_all(parent)?;

    let temp_path = temp_sibling_path(path, "tmp");
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(permissions);
    }

    let mut file = options
        .open(&temp_path)
        .map_err(|source| SwitchFailure::WriteFile {
            path: temp_path.clone(),
            source,
        })?;
    file.write_all(bytes)
        .map_err(|source| SwitchFailure::WriteFile {
            path: temp_path.clone(),
            source,
        })?;
    file.sync_all().map_err(|source| SwitchFailure::WriteFile {
        path: temp_path.clone(),
        source,
    })?;

    fs::rename(&temp_path, path).map_err(|source| SwitchFailure::RenamePath {
        from: temp_path,
        to: path.to_path_buf(),
        source,
    })
}

fn read_file_bytes(path: &Path) -> Result<Vec<u8>, SwitchFailure> {
    fs::read(path).map_err(|source| SwitchFailure::ReadFile {
        path: path.to_path_buf(),
        source,
    })
}

fn create_directory_all(path: &Path) -> Result<(), SwitchFailure> {
    fs::create_dir_all(path).map_err(|source| SwitchFailure::CreateDirectory {
        path: path.to_path_buf(),
        source,
    })
}

fn remove_path_if_exists(path: &Path) -> Result<(), SwitchFailure> {
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|source| SwitchFailure::RemovePath {
            path: path.to_path_buf(),
            source,
        })?;
    } else if path.exists() {
        fs::remove_file(path).map_err(|source| SwitchFailure::RemovePath {
            path: path.to_path_buf(),
            source,
        })?;
    }

    Ok(())
}

fn backup_relative_path(path: &Path, home_dir: &Path) -> PathBuf {
    if let Ok(relative) = path.strip_prefix(home_dir) {
        return relative.to_path_buf();
    }

    if path.is_absolute() {
        let mut relative = PathBuf::new();
        for component in path.components() {
            use std::path::Component;

            if let Component::Normal(value) = component {
                relative.push(value);
            }
        }
        return relative;
    }

    path.to_path_buf()
}

fn temp_sibling_path(path: &Path, label: &str) -> PathBuf {
    let stem = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("path");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic enough for temp names")
        .as_nanos();
    path.with_file_name(format!("{stem}.{label}.{}.{}", process::id(), nanos))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use chrono::{TimeZone, Utc};

    use crate::paths::AswitchPaths;
    use crate::registry::{AccountMetadata, Registry};
    use crate::store;

    use super::{
        backup_relative_path, load_plugin_manifest, read_account_state, save_account_state,
        use_account_inner, SnapshotContent, StoredAccountState, StoredAuxState, SwitchError,
        SwitchHooks,
    };

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
description = "switch tests"
platforms = ["macos", "linux"]

[credential_store]
kind = "file"
path = "~/.demo/auth.json"
permissions = 384

[[aux_files]]
path = "~/.demo/settings.json"
required = true
kind = "file"

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

        fn backup_relative_path(&self, path: &Path) -> PathBuf {
            backup_relative_path(path, &self.home_dir)
        }

        fn set_live_state(&self, auth: &[u8], settings: &[u8]) {
            fs::create_dir_all(self.home_dir.join(".demo")).expect("live dir");
            fs::write(self.active_auth_path(), auth).expect("write auth");
            fs::write(self.active_settings_path(), settings).expect("write settings");
        }

        fn seed_registry(&self) {
            let mut registry = Registry::default();
            registry
                .active
                .insert(self.plugin_id.clone(), Some("x".to_string()));
            registry
                .accounts
                .entry(self.plugin_id.clone())
                .or_default()
                .insert(
                    "x".to_string(),
                    AccountMetadata {
                        alias: "x".to_string(),
                        email: Some("x@example.com".to_string()),
                        org_name: None,
                        plan: None,
                        added_at: Utc
                            .with_ymd_and_hms(2026, 4, 24, 10, 12, 3)
                            .single()
                            .expect("timestamp"),
                        last_used_at: None,
                    },
                );
            registry
                .accounts
                .entry(self.plugin_id.clone())
                .or_default()
                .insert(
                    "y".to_string(),
                    AccountMetadata {
                        alias: "y".to_string(),
                        email: Some("y@example.com".to_string()),
                        org_name: None,
                        plan: None,
                        added_at: Utc
                            .with_ymd_and_hms(2026, 4, 24, 10, 12, 4)
                            .single()
                            .expect("timestamp"),
                        last_used_at: None,
                    },
                );

            registry.save_atomic(&self.paths).expect("save registry");
        }

        fn make_state(&self, auth: &[u8], settings: &[u8]) -> StoredAccountState {
            StoredAccountState {
                credentials: Some(auth.to_vec()),
                aux: vec![StoredAuxState {
                    active_path: self.active_settings_path(),
                    backup_relative_path: self.backup_relative_path(&self.active_settings_path()),
                    content: SnapshotContent::File(settings.to_vec()),
                }],
            }
        }
    }

    #[test]
    fn use_account_switches_live_state_and_updates_registry() {
        let harness = TestHarness::new();
        harness.write_manifest();
        harness.seed_registry();
        harness.set_live_state(b"{\"token\":\"x\"}", b"{\"profile\":\"x\"}");

        let target_state = harness.make_state(b"{\"token\":\"y\"}", b"{\"profile\":\"y\"}");
        save_account_state(&harness.paths, &harness.plugin_id, "y", &target_state)
            .expect("save target account");

        let report = use_account_inner(
            &harness.plugin_id,
            "y",
            Some(harness.config_dir.clone()),
            &harness.home_dir,
            SwitchHooks::default(),
        )
        .expect("switch");

        assert_eq!(report.previous_active.as_deref(), Some("x"));
        assert_eq!(
            fs::read(harness.active_auth_path()).expect("read live auth"),
            b"{\"token\":\"y\"}"
        );
        assert_eq!(
            fs::read(harness.active_settings_path()).expect("read live settings"),
            b"{\"profile\":\"y\"}"
        );

        let registry = Registry::load(&harness.paths).expect("load registry");
        assert_eq!(
            registry.active.get(&harness.plugin_id).cloned().flatten(),
            Some("y".to_string())
        );
        assert!(registry
            .accounts
            .get(&harness.plugin_id)
            .and_then(|accounts| accounts.get("y"))
            .and_then(|account| account.last_used_at)
            .is_some());

        let manifest = load_plugin_manifest(&harness.paths, &harness.plugin_id).expect("manifest");
        let store = store::resolve_active_store_with_home_dir(
            &manifest.credential_store,
            harness.home_dir.clone(),
        )
        .expect("resolve store");
        let backed_up = read_account_state(
            &harness.paths,
            &harness.plugin_id,
            "x",
            &manifest,
            &store,
            &harness.home_dir,
        )
        .expect("read source backup");

        assert_eq!(backed_up.credentials, Some(b"{\"token\":\"x\"}".to_vec()));
        assert_eq!(
            backed_up.aux[0].content,
            SnapshotContent::File(b"{\"profile\":\"x\"}".to_vec())
        );
    }

    #[test]
    fn use_account_rolls_back_after_primary_write_failure() {
        let harness = TestHarness::new();
        harness.write_manifest();
        harness.seed_registry();
        harness.set_live_state(b"{\"token\":\"x\"}", b"{\"profile\":\"x\"}");

        let target_state = harness.make_state(b"{\"token\":\"y\"}", b"{\"profile\":\"y\"}");
        save_account_state(&harness.paths, &harness.plugin_id, "y", &target_state)
            .expect("save target account");

        let error = use_account_inner(
            &harness.plugin_id,
            "y",
            Some(harness.config_dir.clone()),
            &harness.home_dir,
            SwitchHooks {
                fail_after_primary_write: true,
            },
        )
        .expect_err("switch should fail");

        assert!(matches!(error, SwitchError::RollbackSucceeded { .. }));
        assert_eq!(error.exit_code(), 10);
        assert_eq!(
            fs::read(harness.active_auth_path()).expect("read live auth"),
            b"{\"token\":\"x\"}"
        );
        assert_eq!(
            fs::read(harness.active_settings_path()).expect("read live settings"),
            b"{\"profile\":\"x\"}"
        );

        let registry = Registry::load(&harness.paths).expect("load registry");
        assert_eq!(
            registry.active.get(&harness.plugin_id).cloned().flatten(),
            Some("x".to_string())
        );
    }
}
