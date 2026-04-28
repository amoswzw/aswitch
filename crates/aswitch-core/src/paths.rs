use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use directories::BaseDirs;
use fs2::FileExt;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AswitchPaths {
    pub root: PathBuf,
    pub plugins_dir: PathBuf,
    pub accounts_dir: PathBuf,
    pub usage_cache_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub registry_file: PathBuf,
    pub lock_file: PathBuf,
    pub log_file: PathBuf,
}

impl AswitchPaths {
    pub fn resolve(config_dir: Option<PathBuf>) -> Result<Self, PathsError> {
        let root = match config_dir {
            Some(path) => path,
            None => default_root_dir()?,
        };

        Ok(Self {
            plugins_dir: root.join("plugins"),
            accounts_dir: root.join("accounts"),
            usage_cache_dir: root.join("usage_cache"),
            logs_dir: root.join("logs"),
            registry_file: root.join("registry.json"),
            lock_file: root.join(".lock"),
            log_file: root.join("logs").join("aswitch.log"),
            root,
        })
    }

    pub fn ensure(&self) -> Result<(), PathsError> {
        create_dir_all(&self.root)?;
        create_dir_all(&self.plugins_dir)?;
        create_dir_all(&self.accounts_dir)?;
        create_dir_all(&self.usage_cache_dir)?;
        create_dir_all(&self.logs_dir)?;
        Ok(())
    }

    pub fn registry_temp_path(&self, pid: u32) -> PathBuf {
        self.root.join(format!("registry.json.tmp.{pid}"))
    }

    pub fn registry_backup_path(&self, timestamp: &str) -> PathBuf {
        self.root.join(format!("registry.json.bak.{timestamp}"))
    }

    pub fn lock_file(&self, timeout: Duration) -> Result<File, PathsError> {
        self.ensure()?;

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.lock_file)
            .map_err(|source| PathsError::OpenFile {
                path: self.lock_file.clone(),
                source,
            })?;

        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(file),
                Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(PathsError::LockTimeout {
                            path: self.lock_file.clone(),
                            timeout,
                        });
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(source) => {
                    return Err(PathsError::LockFile {
                        path: self.lock_file.clone(),
                        source,
                    });
                }
            }
        }
    }
}

pub fn home_dir() -> Result<PathBuf, PathsError> {
    let base_dirs = BaseDirs::new().ok_or(PathsError::HomeDirectoryUnavailable)?;
    Ok(base_dirs.home_dir().to_path_buf())
}

pub fn expand_user_path(path: &str) -> Result<PathBuf, PathsError> {
    let home_dir = home_dir()?;
    Ok(expand_user_path_from(path, &home_dir))
}

fn default_root_dir() -> Result<PathBuf, PathsError> {
    Ok(home_dir()?.join(".aswitch"))
}

fn create_dir_all(path: &Path) -> Result<(), PathsError> {
    fs::create_dir_all(path).map_err(|source| PathsError::CreateDirectory {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug, Error)]
pub enum PathsError {
    #[error("home directory is unavailable")]
    HomeDirectoryUnavailable,
    #[error("failed to open file {path}")]
    OpenFile {
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
    #[error("failed to lock file {path}")]
    LockFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("timed out after {timeout:?} waiting for lock file {path}")]
    LockTimeout { path: PathBuf, timeout: Duration },
}

pub(crate) fn expand_user_path_from(path: &str, home_dir: &Path) -> PathBuf {
    if path == "~" {
        return home_dir.to_path_buf();
    }

    if let Some(stripped) = path.strip_prefix("~/") {
        return home_dir.join(stripped);
    }

    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{expand_user_path_from, AswitchPaths};

    #[test]
    fn ensure_creates_expected_directory_layout() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join(".aswitch");
        let paths = AswitchPaths::resolve(Some(root.clone())).expect("paths");

        paths.ensure().expect("ensure");

        assert!(root.is_dir());
        assert!(paths.plugins_dir.is_dir());
        assert!(paths.accounts_dir.is_dir());
        assert!(paths.usage_cache_dir.is_dir());
        assert!(paths.logs_dir.is_dir());
        assert_eq!(paths.registry_file, root.join("registry.json"));
        assert_eq!(paths.lock_file, root.join(".lock"));
        assert_eq!(paths.log_file, root.join("logs").join("aswitch.log"));
    }

    #[test]
    fn expand_user_path_rewrites_tilde_prefix() {
        let home_dir = Path::new("/tmp/aswitch-home");

        assert_eq!(expand_user_path_from("~", home_dir), home_dir);
        assert_eq!(
            expand_user_path_from("~/nested/file.json", home_dir),
            home_dir.join("nested/file.json")
        );
        assert_eq!(
            expand_user_path_from("/already/absolute", home_dir),
            Path::new("/already/absolute")
        );
        assert_eq!(
            expand_user_path_from("relative/path", home_dir),
            Path::new("relative/path")
        );
    }
}
