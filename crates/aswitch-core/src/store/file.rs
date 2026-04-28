use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

use crate::store::{CredentialStore, StoreError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStore {
    path: PathBuf,
    permissions: u32,
}

impl FileStore {
    pub fn new(path: PathBuf, permissions: u32) -> Self {
        Self { path, permissions }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn permissions(&self) -> u32 {
        self.permissions
    }

    fn temp_path(&self) -> PathBuf {
        let file_name = self
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("credential");

        self.path
            .with_file_name(format!("{file_name}.tmp.{}", process::id()))
    }
}

impl CredentialStore for FileStore {
    fn read_active(&self) -> Result<Vec<u8>, StoreError> {
        fs::read(&self.path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => StoreError::NotFound {
                path: self.path.clone(),
            },
            _ => StoreError::Read {
                path: self.path.clone(),
                source,
            },
        })
    }

    fn write_active(&self, bytes: &[u8]) -> Result<(), StoreError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| StoreError::CreateDirectory {
                path: self.path.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "credential path has no parent directory",
                ),
            })?;

        fs::create_dir_all(parent).map_err(|source| StoreError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;

        let temp_path = self.temp_path();
        let mut file = create_temp_file(&temp_path, self.permissions)?;
        file.write_all(bytes).map_err(|source| StoreError::Write {
            path: temp_path.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| StoreError::Write {
            path: temp_path.clone(),
            source,
        })?;

        fs::rename(&temp_path, &self.path).map_err(|source| StoreError::Rename {
            from: temp_path,
            to: self.path.clone(),
            source,
        })?;

        Ok(())
    }

    fn clear_active(&self) -> Result<(), StoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Remove {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn exists(&self) -> Result<bool, StoreError> {
        Ok(self.path.is_file())
    }
}

fn create_temp_file(path: &Path, permissions: u32) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(permissions);
    }

    options.open(path).map_err(|source| StoreError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use crate::store::file::FileStore;
    use crate::store::CredentialStore;

    #[test]
    fn round_trip_reads_and_overwrites_file_contents() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = FileStore::new(temp_dir.path().join(".codex/auth.json"), 0o600);

        assert!(!store.exists().expect("exists before write"));

        store
            .write_active(br#"{"access_token":"first"}"#)
            .expect("first write");
        assert!(store.exists().expect("exists after write"));
        assert_eq!(
            store.read_active().expect("first read"),
            br#"{"access_token":"first"}"#
        );

        store
            .write_active(br#"{"access_token":"second"}"#)
            .expect("second write");
        assert_eq!(
            store.read_active().expect("second read"),
            br#"{"access_token":"second"}"#
        );

        let entries = fs::read_dir(temp_dir.path().join(".codex"))
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert_eq!(entries, vec!["auth.json"]);
    }

    #[test]
    fn write_preserves_requested_permissions() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = FileStore::new(temp_dir.path().join(".codex/auth.json"), 0o600);

        store.write_active(b"{}").expect("write");

        #[cfg(unix)]
        {
            let mode = fs::metadata(store.path())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn clear_active_removes_existing_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = FileStore::new(temp_dir.path().join(".codex/auth.json"), 0o600);

        store.write_active(b"{}").expect("write");
        assert!(store.exists().expect("exists after write"));

        store.clear_active().expect("clear");
        assert!(!store.exists().expect("exists after clear"));
    }
}
