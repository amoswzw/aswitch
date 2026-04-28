use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::paths::{AswitchPaths, PathsError};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default = "default_registry_version")]
    pub version: u32,
    #[serde(default)]
    pub active: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub accounts: BTreeMap<String, BTreeMap<String, AccountMetadata>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: default_registry_version(),
            active: BTreeMap::new(),
            accounts: BTreeMap::new(),
        }
    }
}

impl Registry {
    pub fn load(paths: &AswitchPaths) -> Result<Self, RegistryError> {
        paths.ensure()?;

        let bytes = match fs::read(&paths.registry_file) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(source) => {
                return Err(RegistryError::Read {
                    path: paths.registry_file.clone(),
                    source,
                })
            }
        };

        match serde_json::from_slice(&bytes) {
            Ok(registry) => Ok(registry),
            Err(_source) => {
                let _backup_path = backup_corrupted_registry(paths)?;
                let empty = Self::default();
                empty.save_atomic(paths)?;
                Ok(empty)
            }
        }
    }

    pub fn save_atomic(&self, paths: &AswitchPaths) -> Result<(), RegistryError> {
        paths.ensure()?;

        let temp_path = paths.registry_temp_path(process::id());
        let payload = serde_json::to_vec_pretty(self)?;
        let mut file = create_temp_file(&temp_path)?;

        file.write_all(&payload)
            .map_err(|source| RegistryError::Write {
                path: temp_path.clone(),
                source,
            })?;
        file.write_all(b"\n")
            .map_err(|source| RegistryError::Write {
                path: temp_path.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| RegistryError::Write {
            path: temp_path.clone(),
            source,
        })?;

        fs::rename(&temp_path, &paths.registry_file).map_err(|source| RegistryError::Rename {
            from: temp_path,
            to: paths.registry_file.clone(),
            source,
        })?;

        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountMetadata {
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
}

fn default_registry_version() -> u32 {
    1
}

fn backup_corrupted_registry(paths: &AswitchPaths) -> Result<PathBuf, RegistryError> {
    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let backup_path = paths.registry_backup_path(&timestamp);

    fs::rename(&paths.registry_file, &backup_path).map_err(|source| RegistryError::Backup {
        from: paths.registry_file.clone(),
        to: backup_path.clone(),
        source,
    })?;

    Ok(backup_path)
}

fn create_temp_file(path: &PathBuf) -> Result<File, RegistryError> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }

    options.open(path).map_err(|source| RegistryError::Write {
        path: path.clone(),
        source,
    })
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("failed to prepare aswitch directories")]
    Paths(#[from] PathsError),
    #[error("failed to read registry file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to backup corrupted registry from {from} to {to}")]
    Backup {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write registry file {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to rename registry temp file from {from} to {to}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize registry")]
    Serialize(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::TimeZone;

    use super::{AccountMetadata, Registry};
    use crate::paths::AswitchPaths;

    #[test]
    fn save_and_load_round_trip() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let paths = AswitchPaths::resolve(Some(temp_dir.path().join(".aswitch"))).expect("paths");
        let mut registry = Registry::default();
        let account = AccountMetadata {
            alias: "work-main".into(),
            email: Some("amos@example.com".into()),
            org_name: Some("Amos".into()),
            plan: Some("team".into()),
            added_at: chrono::Utc
                .with_ymd_and_hms(2026, 4, 24, 10, 12, 3)
                .single()
                .expect("timestamp"),
            last_used_at: None,
        };

        registry
            .active
            .insert("claude-code".into(), Some("work-main".into()));
        registry
            .accounts
            .entry("claude-code".into())
            .or_default()
            .insert(account.alias.clone(), account);

        registry.save_atomic(&paths).expect("save");
        let loaded = Registry::load(&paths).expect("load");

        assert_eq!(loaded, registry);
    }

    #[test]
    fn corrupted_registry_is_backed_up_and_reset() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let paths = AswitchPaths::resolve(Some(temp_dir.path().join(".aswitch"))).expect("paths");

        paths.ensure().expect("ensure");
        fs::write(&paths.registry_file, b"{ this is not valid json").expect("write corrupt");

        let reset = Registry::load(&paths).expect("load reinitialized registry");
        assert_eq!(reset, Registry::default());

        let backup_count = fs::read_dir(&paths.root)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("registry.json.bak.")
            })
            .count();

        assert_eq!(backup_count, 1);
        assert_eq!(
            fs::read_to_string(&paths.registry_file).expect("read registry"),
            "{\n  \"version\": 1,\n  \"active\": {},\n  \"accounts\": {}\n}\n"
        );
    }
}
