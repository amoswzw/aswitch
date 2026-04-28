use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use thiserror::Error;

use crate::paths::{self, AswitchPaths, PathsError};
use crate::plugin::{
    self, load_manifest, load_manifest_with_env_overrides, AuxFileKind, CredentialStoreKind,
    ManifestLoadError, SessionActivationConfig,
};

const AUX_DIR: &str = "aux";
const PRIMARY_CREDENTIALS_FILE: &str = "primary.creds";
const FILE_MODE: u32 = 0o600;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionActivationReport {
    pub plugin_id: String,
    pub alias: String,
    pub env_var: String,
    pub default_home: PathBuf,
    pub runtime_home: PathBuf,
    pub runtime_credential_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("failed to resolve aswitch paths")]
    Paths(#[from] PathsError),
    #[error("failed to load plugin manifest")]
    Manifest(#[from] ManifestLoadError),
    #[error("plugin {plugin_id} does not support terminal-scoped activation")]
    ActivationNotSupported { plugin_id: String },
    #[error("plugin {plugin_id} only supports terminal activation with file credentials")]
    UnsupportedCredentialStore { plugin_id: String },
    #[error("account backup for {plugin_id}/{alias} is missing at {path}")]
    AccountBackupMissing {
        plugin_id: String,
        alias: String,
        path: PathBuf,
    },
    #[error("plugin {plugin_id} credential path {credential_path} must stay under session_activation.default_home {default_home}")]
    CredentialPathOutsideDefaultHome {
        plugin_id: String,
        credential_path: PathBuf,
        default_home: PathBuf,
    },
    #[error("plugin {plugin_id} aux path {aux_path} must stay under session_activation.default_home {default_home}")]
    AuxPathOutsideDefaultHome {
        plugin_id: String,
        aux_path: PathBuf,
        default_home: PathBuf,
    },
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
    #[error("failed to remove path {path}")]
    RemovePath {
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
    #[error("failed to symlink path from {from} to {to}")]
    SymlinkPath {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn prepare_session_activation(
    plugin_id: &str,
    alias: &str,
) -> Result<SessionActivationReport, SessionError> {
    prepare_session_activation_with_config_dir(plugin_id, alias, None)
}

pub fn prepare_session_activation_with_config_dir(
    plugin_id: &str,
    alias: &str,
    config_dir: Option<PathBuf>,
) -> Result<SessionActivationReport, SessionError> {
    let paths = AswitchPaths::resolve(config_dir)?;
    let home_dir = paths::home_dir()?;
    let manifest_path = paths.plugins_dir.join(plugin_id).join("plugin.toml");
    let loaded = load_manifest(&manifest_path)?;
    let activation = activation_config(&loaded.manifest).ok_or_else(|| {
        SessionError::ActivationNotSupported {
            plugin_id: plugin_id.to_string(),
        }
    })?;

    let mut overrides = BTreeMap::new();
    overrides.insert(activation.env_var.clone(), String::new());
    let loaded = load_manifest_with_env_overrides(&manifest_path, &overrides)?;
    let manifest = loaded.manifest;

    if manifest.credential_store.kind != CredentialStoreKind::File {
        return Err(SessionError::UnsupportedCredentialStore {
            plugin_id: plugin_id.to_string(),
        });
    }

    let default_home = paths::expand_user_path_from(&activation.default_home, &home_dir);
    let credential_path = resolve_credential_path(&manifest, &home_dir)?;
    let relative_credential_path = credential_path.strip_prefix(&default_home).map_err(|_| {
        SessionError::CredentialPathOutsideDefaultHome {
            plugin_id: plugin_id.to_string(),
            credential_path: credential_path.clone(),
            default_home: default_home.clone(),
        }
    })?;

    let backup_path = paths
        .accounts_dir
        .join(plugin_id)
        .join(alias)
        .join(PRIMARY_CREDENTIALS_FILE);
    let credential_bytes = read_required_file_backup(plugin_id, alias, &backup_path)?;

    let runtime_home = paths.root.join("runtime").join(plugin_id).join(alias);
    create_directory_all(&runtime_home)?;
    materialize_shared_paths(&default_home, &runtime_home, &activation.shared_paths)?;

    let runtime_credential_path = runtime_home.join(relative_credential_path);
    write_file_atomic(&runtime_credential_path, &credential_bytes, FILE_MODE)?;

    for aux_file in &manifest.aux_files {
        let aux_path = paths::expand_user_path_from(&aux_file.path, &home_dir);
        let relative_aux_path = aux_path.strip_prefix(&default_home).map_err(|_| {
            SessionError::AuxPathOutsideDefaultHome {
                plugin_id: plugin_id.to_string(),
                aux_path: aux_path.clone(),
                default_home: default_home.clone(),
            }
        })?;
        let backup_relative_path = backup_relative_path(&aux_path, &home_dir);
        let backup_path = paths
            .accounts_dir
            .join(plugin_id)
            .join(alias)
            .join(AUX_DIR)
            .join(backup_relative_path);
        let content = read_snapshot_content(
            plugin_id,
            alias,
            &backup_path,
            aux_file.kind,
            aux_file.required,
        )?;
        materialize_content(&runtime_home.join(relative_aux_path), &content)?;
    }

    Ok(SessionActivationReport {
        plugin_id: plugin_id.to_string(),
        alias: alias.to_string(),
        env_var: activation.env_var,
        default_home,
        runtime_home,
        runtime_credential_path,
    })
}

pub fn session_env_var(plugin_id: &str) -> Result<String, SessionError> {
    session_env_var_with_config_dir(plugin_id, None)
}

pub fn session_env_var_with_config_dir(
    plugin_id: &str,
    config_dir: Option<PathBuf>,
) -> Result<String, SessionError> {
    let paths = AswitchPaths::resolve(config_dir)?;
    let manifest_path = paths.plugins_dir.join(plugin_id).join("plugin.toml");
    let loaded = load_manifest(&manifest_path)?;
    activation_config(&loaded.manifest)
        .map(|config| config.env_var)
        .ok_or_else(|| SessionError::ActivationNotSupported {
            plugin_id: plugin_id.to_string(),
        })
}

fn activation_config(manifest: &plugin::Manifest) -> Option<SessionActivationConfig> {
    manifest
        .session_activation
        .clone()
        .or_else(|| legacy_activation_config(&manifest.id))
}

fn legacy_activation_config(plugin_id: &str) -> Option<SessionActivationConfig> {
    if plugin_id != "codex" {
        return None;
    }

    Some(SessionActivationConfig {
        env_var: "CODEX_HOME".to_string(),
        default_home: "~/.codex".to_string(),
        shared_paths: vec![
            "config.toml".to_string(),
            "skills".to_string(),
            "rules".to_string(),
            "memories".to_string(),
            "vendor_imports".to_string(),
            "installation_id".to_string(),
            "version.json".to_string(),
        ],
    })
}

fn resolve_credential_path(
    manifest: &plugin::Manifest,
    home_dir: &Path,
) -> Result<PathBuf, SessionError> {
    let path = manifest.credential_store.path.as_deref().ok_or_else(|| {
        SessionError::UnsupportedCredentialStore {
            plugin_id: manifest.id.clone(),
        }
    })?;
    Ok(paths::expand_user_path_from(path, home_dir))
}

fn read_required_file_backup(
    plugin_id: &str,
    alias: &str,
    path: &Path,
) -> Result<Vec<u8>, SessionError> {
    fs::read(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            SessionError::AccountBackupMissing {
                plugin_id: plugin_id.to_string(),
                alias: alias.to_string(),
                path: path.to_path_buf(),
            }
        } else {
            SessionError::ReadFile {
                path: path.to_path_buf(),
                source,
            }
        }
    })
}

fn materialize_shared_paths(
    default_home: &Path,
    runtime_home: &Path,
    shared_paths: &[String],
) -> Result<(), SessionError> {
    for relative in shared_paths {
        let source = default_home.join(relative);
        if !source.exists() {
            continue;
        }

        let destination = runtime_home.join(relative);
        remove_path_if_exists(&destination)?;
        let parent = destination
            .parent()
            .ok_or_else(|| SessionError::CreateDirectory {
                path: destination.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path has no parent directory",
                ),
            })?;
        create_directory_all(parent)?;

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&source, &destination).map_err(|source_error| {
                SessionError::SymlinkPath {
                    from: source.clone(),
                    to: destination.clone(),
                    source: source_error,
                }
            })?;
        }

        #[cfg(not(unix))]
        {
            if source.is_dir() {
                copy_dir_recursive(&source, &destination)?;
            } else {
                let bytes = fs::read(&source).map_err(|source_error| SessionError::ReadFile {
                    path: source.clone(),
                    source: source_error,
                })?;
                write_file_atomic(&destination, &bytes, FILE_MODE)?;
            }
        }
    }

    Ok(())
}

fn read_snapshot_content(
    plugin_id: &str,
    alias: &str,
    path: &Path,
    kind: AuxFileKind,
    required: bool,
) -> Result<SnapshotContent, SessionError> {
    match kind {
        AuxFileKind::File => {
            if path.is_file() {
                return Ok(SnapshotContent::File(read_required_file_backup(
                    plugin_id, alias, path,
                )?));
            }
            if !path.exists() && !required {
                return Ok(SnapshotContent::Missing);
            }
            if !path.exists() {
                return Err(SessionError::AccountBackupMissing {
                    plugin_id: plugin_id.to_string(),
                    alias: alias.to_string(),
                    path: path.to_path_buf(),
                });
            }
            Err(SessionError::ReadFile {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "expected a file backup",
                ),
            })
        }
        AuxFileKind::Dir => {
            if path.is_dir() {
                return Ok(SnapshotContent::Dir(read_directory_snapshot(
                    plugin_id, alias, path,
                )?));
            }
            if !path.exists() && !required {
                return Ok(SnapshotContent::Missing);
            }
            if !path.exists() {
                return Err(SessionError::AccountBackupMissing {
                    plugin_id: plugin_id.to_string(),
                    alias: alias.to_string(),
                    path: path.to_path_buf(),
                });
            }
            Err(SessionError::ReadFile {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "expected a directory backup",
                ),
            })
        }
    }
}

fn read_directory_snapshot(
    plugin_id: &str,
    alias: &str,
    path: &Path,
) -> Result<DirectorySnapshot, SessionError> {
    let mut files = Vec::new();
    collect_directory_files(plugin_id, alias, path, path, &mut files)?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(DirectorySnapshot { files })
}

fn collect_directory_files(
    plugin_id: &str,
    alias: &str,
    root: &Path,
    current: &Path,
    files: &mut Vec<DirectoryFile>,
) -> Result<(), SessionError> {
    let mut entries = fs::read_dir(current)
        .map_err(|source| SessionError::ReadFile {
            path: current.to_path_buf(),
            source,
        })?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_directory_files(plugin_id, alias, root, &path, files)?;
        } else if path.is_file() {
            let relative_path = path
                .strip_prefix(root)
                .expect("directory traversal should stay under root")
                .to_path_buf();
            files.push(DirectoryFile {
                relative_path,
                bytes: read_required_file_backup(plugin_id, alias, &path)?,
            });
        } else {
            return Err(SessionError::ReadFile {
                path,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "expected a file entry in directory backup",
                ),
            });
        }
    }

    Ok(())
}

fn materialize_content(path: &Path, content: &SnapshotContent) -> Result<(), SessionError> {
    remove_path_if_exists(path)?;

    match content {
        SnapshotContent::Missing => Ok(()),
        SnapshotContent::File(bytes) => write_file_atomic(path, bytes, FILE_MODE),
        SnapshotContent::Dir(snapshot) => write_directory_contents(path, snapshot),
    }
}

fn write_directory_contents(path: &Path, snapshot: &DirectorySnapshot) -> Result<(), SessionError> {
    create_directory_all(path)?;
    for file in &snapshot.files {
        write_file_atomic(&path.join(&file.relative_path), &file.bytes, FILE_MODE)?;
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

#[cfg(not(unix))]
fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), SessionError> {
    create_directory_all(destination)?;
    for entry in fs::read_dir(source)
        .map_err(|source_error| SessionError::ReadFile {
            path: source.to_path_buf(),
            source: source_error,
        })?
        .filter_map(Result::ok)
    {
        let path = entry.path();
        let target = destination.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else {
            let bytes = fs::read(&path).map_err(|source_error| SessionError::ReadFile {
                path: path.clone(),
                source: source_error,
            })?;
            write_file_atomic(&target, &bytes, FILE_MODE)?;
        }
    }
    Ok(())
}

fn write_file_atomic(path: &Path, bytes: &[u8], permissions: u32) -> Result<(), SessionError> {
    let parent = path.parent().ok_or_else(|| SessionError::CreateDirectory {
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
        .map_err(|source| SessionError::WriteFile {
            path: temp_path.clone(),
            source,
        })?;
    file.write_all(bytes)
        .map_err(|source| SessionError::WriteFile {
            path: temp_path.clone(),
            source,
        })?;
    file.sync_all().map_err(|source| SessionError::WriteFile {
        path: temp_path.clone(),
        source,
    })?;

    fs::rename(&temp_path, path).map_err(|source| SessionError::RenamePath {
        from: temp_path,
        to: path.to_path_buf(),
        source,
    })
}

fn create_directory_all(path: &Path) -> Result<(), SessionError> {
    fs::create_dir_all(path).map_err(|source| SessionError::CreateDirectory {
        path: path.to_path_buf(),
        source,
    })
}

fn remove_path_if_exists(path: &Path) -> Result<(), SessionError> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };

    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|source| SessionError::RemovePath {
            path: path.to_path_buf(),
            source,
        })?;
    } else {
        fs::remove_file(path).map_err(|source| SessionError::RemovePath {
            path: path.to_path_buf(),
            source,
        })?;
    }

    Ok(())
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

    use super::{prepare_session_activation_with_config_dir, session_env_var_with_config_dir};
    use crate::paths::AswitchPaths;

    #[test]
    fn prepares_runtime_home_for_terminal_activation() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config_dir = temp_dir.path().join(".aswitch");
        let home_dir = temp_dir.path().join("home");
        fs::create_dir_all(home_dir.join(".codex")).expect("codex home");

        let paths = AswitchPaths::resolve(Some(config_dir.clone())).expect("paths");
        paths.ensure().expect("ensure");

        let plugin_dir = paths.plugins_dir.join("codex");
        fs::create_dir_all(&plugin_dir).expect("plugin dir");
        fs::write(
            plugin_dir.join("plugin.toml"),
            r#"
id = "codex"
display_name = "Codex"
version = "1.0.0"
author = "tests"
description = "tests"
platforms = ["macos", "linux"]

[credential_store]
kind = "file"
path = "~/.codex/auth.json"
permissions = 384

[session_activation]
env_var = "CODEX_HOME"
default_home = "~/.codex"
shared_paths = ["config.toml", "skills"]

[login]
cmd = ["codex", "login"]
ready_marker_kind = "file"
ready_marker_timeout_s = 1
ready_marker_path = "~/.codex/auth.json"
"#,
        )
        .expect("manifest");

        let account_dir = paths.accounts_dir.join("codex").join("work");
        fs::create_dir_all(&account_dir).expect("account dir");
        fs::write(
            account_dir.join("primary.creds"),
            br#"{"tokens":{"access_token":"work-token"}}"#,
        )
        .expect("primary creds");

        fs::write(home_dir.join(".codex/config.toml"), "model = \"gpt-5.4\"\n").expect("config");
        fs::create_dir_all(home_dir.join(".codex/skills")).expect("skills");

        let original_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home_dir);
        let report = prepare_session_activation_with_config_dir("codex", "work", Some(config_dir))
            .expect("prepare");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(report.env_var, "CODEX_HOME");
        assert_eq!(
            fs::read(report.runtime_home.join("auth.json")).expect("runtime auth"),
            br#"{"tokens":{"access_token":"work-token"}}"#
        );
        assert!(report.runtime_home.join("config.toml").exists());

        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(report.runtime_home.join("config.toml"))
                .expect("config metadata")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn reads_session_env_var_from_manifest() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config_dir = temp_dir.path().join(".aswitch");
        let paths = AswitchPaths::resolve(Some(config_dir.clone())).expect("paths");
        paths.ensure().expect("ensure");

        let plugin_dir = paths.plugins_dir.join("codex");
        fs::create_dir_all(&plugin_dir).expect("plugin dir");
        fs::write(
            plugin_dir.join("plugin.toml"),
            r#"
id = "codex"
display_name = "Codex"
version = "1.0.0"
author = "tests"
description = "tests"
platforms = ["macos", "linux"]

[credential_store]
kind = "file"
path = "~/.codex/auth.json"
permissions = 384

[session_activation]
env_var = "CODEX_HOME"
default_home = "~/.codex"

[login]
cmd = ["codex", "login"]
ready_marker_kind = "file"
ready_marker_timeout_s = 1
ready_marker_path = "~/.codex/auth.json"
"#,
        )
        .expect("manifest");

        let env_var = session_env_var_with_config_dir("codex", Some(config_dir)).expect("env var");
        assert_eq!(env_var, "CODEX_HOME");
    }

    #[test]
    fn prepares_runtime_home_with_aux_files_for_terminal_activation() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config_dir = temp_dir.path().join(".aswitch");
        let home_dir = temp_dir.path().join("home");
        fs::create_dir_all(home_dir.join(".gemini")).expect("gemini home");

        let paths = AswitchPaths::resolve(Some(config_dir.clone())).expect("paths");
        paths.ensure().expect("ensure");

        let plugin_dir = paths.plugins_dir.join("gemini");
        fs::create_dir_all(&plugin_dir).expect("plugin dir");
        fs::write(
            plugin_dir.join("plugin.toml"),
            r#"
id = "gemini"
display_name = "Gemini"
version = "1.0.0"
author = "tests"
description = "tests"
platforms = ["macos", "linux"]

[credential_store]
kind = "file"
path = "~/.gemini/oauth_creds.json"
permissions = 384

[session_activation]
env_var = "GEMINI_CLI_HOME"
default_home = "~"

[[aux_files]]
path = "~/.gemini/settings.json"
required = false
kind = "file"

[[aux_files]]
path = "~/.gemini/google_accounts.json"
required = false
kind = "file"

[login]
cmd = ["gemini"]
ready_marker_kind = "file"
ready_marker_timeout_s = 1
ready_marker_path = "~/.gemini/oauth_creds.json"
"#,
        )
        .expect("manifest");

        let account_dir = paths.accounts_dir.join("gemini").join("work");
        fs::create_dir_all(account_dir.join("aux/.gemini")).expect("account dir");
        fs::write(
            account_dir.join("primary.creds"),
            br#"{"refresh_token":"work-token"}"#,
        )
        .expect("primary creds");
        fs::write(
            account_dir.join("aux/.gemini/settings.json"),
            br#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#,
        )
        .expect("settings");
        fs::write(
            account_dir.join("aux/.gemini/google_accounts.json"),
            br#"{"active":"user@example.com"}"#,
        )
        .expect("accounts");

        let original_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home_dir);
        let report = prepare_session_activation_with_config_dir("gemini", "work", Some(config_dir))
            .expect("prepare");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(report.env_var, "GEMINI_CLI_HOME");
        assert_eq!(
            fs::read(report.runtime_home.join(".gemini/oauth_creds.json")).expect("runtime auth"),
            br#"{"refresh_token":"work-token"}"#
        );
        assert_eq!(
            fs::read(report.runtime_home.join(".gemini/settings.json")).expect("runtime settings"),
            br#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#
        );
        assert_eq!(
            fs::read(report.runtime_home.join(".gemini/google_accounts.json"))
                .expect("runtime accounts"),
            br#"{"active":"user@example.com"}"#
        );
    }
}
