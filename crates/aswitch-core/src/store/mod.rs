pub mod file;
pub mod keychain;

use std::path::PathBuf;

use thiserror::Error;

use crate::paths::{self, PathsError};
use crate::plugin::{CredentialStore as ManifestCredentialStore, CredentialStoreKind, Platform};

pub trait CredentialStore {
    fn read_active(&self) -> Result<Vec<u8>, StoreError>;
    fn write_active(&self, bytes: &[u8]) -> Result<(), StoreError>;
    fn clear_active(&self) -> Result<(), StoreError>;
    fn exists(&self) -> Result<bool, StoreError>;
    fn allows_missing_active(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedCredentialStore {
    File(file::FileStore),
    Keychain(keychain::KeychainStore),
}

pub fn resolve_active_store(
    manifest_store: &ManifestCredentialStore,
) -> Result<ResolvedCredentialStore, StoreResolveError> {
    let home_dir = paths::home_dir()?;
    resolve_active_store_with_home_dir(manifest_store, home_dir)
}

pub(crate) fn resolve_active_store_with_home_dir(
    manifest_store: &ManifestCredentialStore,
    home_dir: PathBuf,
) -> Result<ResolvedCredentialStore, StoreResolveError> {
    let platform = Platform::current().ok_or(StoreResolveError::UnsupportedPlatform)?;
    resolve_active_store_for_platform(manifest_store, platform, home_dir)
}

fn resolve_active_store_for_platform(
    manifest_store: &ManifestCredentialStore,
    platform: Platform,
    home_dir: PathBuf,
) -> Result<ResolvedCredentialStore, StoreResolveError> {
    match manifest_store.kind {
        CredentialStoreKind::File => {
            let path = manifest_store
                .path
                .as_deref()
                .ok_or(StoreResolveError::MissingField("credential_store.path"))?;
            let permissions = manifest_store
                .permissions
                .ok_or(StoreResolveError::MissingField(
                    "credential_store.permissions",
                ))?;

            Ok(ResolvedCredentialStore::File(file::FileStore::new(
                paths::expand_user_path_from(path, &home_dir),
                permissions,
            )))
        }
        CredentialStoreKind::Keychain => {
            if platform == Platform::Linux
                && manifest_store.linux_fallback_kind == Some(CredentialStoreKind::File)
            {
                let path = manifest_store.linux_fallback_path.as_deref().ok_or(
                    StoreResolveError::MissingField("credential_store.linux_fallback_path"),
                )?;
                let permissions = manifest_store.linux_fallback_permissions.ok_or(
                    StoreResolveError::MissingField("credential_store.linux_fallback_permissions"),
                )?;

                return Ok(ResolvedCredentialStore::File(file::FileStore::new(
                    paths::expand_user_path_from(path, &home_dir),
                    permissions,
                )));
            }

            match platform {
                Platform::Macos => {
                    let service = manifest_store.macos_service.clone().ok_or(
                        StoreResolveError::MissingField("credential_store.macos_service"),
                    )?;
                    let account = manifest_store.macos_account.clone().ok_or(
                        StoreResolveError::MissingField("credential_store.macos_account"),
                    )?;

                    Ok(ResolvedCredentialStore::Keychain(
                        keychain::KeychainStore::macos(
                            service,
                            account,
                            manifest_store.allow_empty_active,
                        ),
                    ))
                }
                Platform::Linux => {
                    let schema = manifest_store.linux_schema.clone().ok_or(
                        StoreResolveError::MissingField("credential_store.linux_schema"),
                    )?;
                    if manifest_store.linux_attributes.is_empty() {
                        return Err(StoreResolveError::MissingField(
                            "credential_store.linux_attributes",
                        ));
                    }

                    Ok(ResolvedCredentialStore::Keychain(
                        keychain::KeychainStore::linux(
                            schema,
                            manifest_store.linux_attributes.clone(),
                            manifest_store.allow_empty_active,
                        ),
                    ))
                }
                Platform::Windows => Err(StoreResolveError::UnsupportedPlatform),
            }
        }
    }
}

impl CredentialStore for ResolvedCredentialStore {
    fn read_active(&self) -> Result<Vec<u8>, StoreError> {
        match self {
            ResolvedCredentialStore::File(store) => store.read_active(),
            ResolvedCredentialStore::Keychain(store) => store.read_active(),
        }
    }

    fn write_active(&self, bytes: &[u8]) -> Result<(), StoreError> {
        match self {
            ResolvedCredentialStore::File(store) => store.write_active(bytes),
            ResolvedCredentialStore::Keychain(store) => store.write_active(bytes),
        }
    }

    fn clear_active(&self) -> Result<(), StoreError> {
        match self {
            ResolvedCredentialStore::File(store) => store.clear_active(),
            ResolvedCredentialStore::Keychain(store) => store.clear_active(),
        }
    }

    fn exists(&self) -> Result<bool, StoreError> {
        match self {
            ResolvedCredentialStore::File(store) => store.exists(),
            ResolvedCredentialStore::Keychain(store) => store.exists(),
        }
    }

    fn allows_missing_active(&self) -> bool {
        match self {
            ResolvedCredentialStore::File(store) => store.allows_missing_active(),
            ResolvedCredentialStore::Keychain(store) => store.allows_missing_active(),
        }
    }
}

#[derive(Debug, Error)]
pub enum StoreResolveError {
    #[error("failed to resolve user paths")]
    Paths(#[from] PathsError),
    #[error("missing required manifest field: {0}")]
    MissingField(&'static str),
    #[error("unsupported current platform")]
    UnsupportedPlatform,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("credential file {path} does not exist")]
    NotFound { path: PathBuf },
    #[error("keychain item not found for service {service} account {account}")]
    KeychainItemNotFound { service: String, account: String },
    #[error("secret-service item not found for attributes {attributes}")]
    SecretServiceItemNotFound { attributes: String },
    #[error("failed to read credential file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create parent directory {path}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write credential file {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove credential file {path}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to rename credential temp file from {from} to {to}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("keychain credentials must be valid UTF-8")]
    InvalidKeychainEncoding {
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("failed to run command {program}")]
    CommandSpawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "command {program} failed during {action} for keychain item {service}/{account} with status {status}: {stderr}"
    )]
    CommandFailure {
        program: String,
        action: &'static str,
        service: String,
        account: String,
        status: String,
        stderr: String,
    },
    #[error("secret-service operation {action} failed for {locator}: {message}")]
    SecretServiceFailure {
        action: &'static str,
        locator: String,
        message: String,
    },
    #[error("keychain backend is not available on this platform")]
    UnsupportedKeychainBackend,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use crate::plugin::{
        CredentialStore as ManifestCredentialStore, CredentialStoreKind, Platform,
    };

    use super::{resolve_active_store_for_platform, CredentialStore, ResolvedCredentialStore};
    use crate::store::file::FileStore;

    #[test]
    fn resolves_plain_file_store() {
        let manifest_store = ManifestCredentialStore {
            kind: CredentialStoreKind::File,
            path: Some("~/.codex/auth.json".into()),
            permissions: Some(0o600),
            macos_service: None,
            macos_account: None,
            linux_schema: None,
            linux_attributes: BTreeMap::new(),
            linux_fallback_kind: None,
            linux_fallback_path: None,
            linux_fallback_permissions: None,
            allow_empty_active: false,
        };

        let resolved = resolve_active_store_for_platform(
            &manifest_store,
            Platform::Macos,
            PathBuf::from("/tmp/aswitch-home"),
        )
        .expect("resolve");

        match resolved {
            ResolvedCredentialStore::File(store) => {
                assert_eq!(
                    store.path(),
                    PathBuf::from("/tmp/aswitch-home/.codex/auth.json")
                );
                assert_eq!(store.permissions(), 0o600);
            }
            other => panic!("unexpected store type: {other:?}"),
        }
    }

    #[test]
    fn linux_keychain_fallback_resolves_to_file_store() {
        let manifest_store = ManifestCredentialStore {
            kind: CredentialStoreKind::Keychain,
            path: None,
            permissions: None,
            macos_service: Some("Claude Code-credentials".into()),
            macos_account: Some("${env:USER}".into()),
            linux_schema: None,
            linux_attributes: BTreeMap::new(),
            linux_fallback_kind: Some(CredentialStoreKind::File),
            linux_fallback_path: Some("~/.claude/.credentials.json".into()),
            linux_fallback_permissions: Some(0o600),
            allow_empty_active: false,
        };

        let resolved = resolve_active_store_for_platform(
            &manifest_store,
            Platform::Linux,
            PathBuf::from("/tmp/aswitch-home"),
        )
        .expect("resolve");

        match resolved {
            ResolvedCredentialStore::File(store) => {
                assert_eq!(
                    store.path(),
                    PathBuf::from("/tmp/aswitch-home/.claude/.credentials.json")
                );
                assert_eq!(store.permissions(), 0o600);
            }
            other => panic!("unexpected store type: {other:?}"),
        }
    }

    #[test]
    fn macos_keychain_store_resolves_with_allow_empty_active() {
        let manifest_store = ManifestCredentialStore {
            kind: CredentialStoreKind::Keychain,
            path: None,
            permissions: None,
            macos_service: Some("Claude Code-credentials".into()),
            macos_account: Some("amos".into()),
            linux_schema: Some("org.freedesktop.Secret.Generic".into()),
            linux_attributes: BTreeMap::from([("service".into(), "claude-code".into())]),
            linux_fallback_kind: None,
            linux_fallback_path: None,
            linux_fallback_permissions: None,
            allow_empty_active: true,
        };

        let resolved = resolve_active_store_for_platform(
            &manifest_store,
            Platform::Macos,
            PathBuf::from("/tmp/aswitch-home"),
        )
        .expect("resolve");

        match resolved {
            ResolvedCredentialStore::Keychain(store) => {
                assert_eq!(store.macos_service(), Some("Claude Code-credentials"));
                assert_eq!(store.macos_account(), Some("amos"));
                assert!(store.allow_empty_active());
            }
            other => panic!("unexpected store type: {other:?}"),
        }
    }

    #[test]
    fn resolved_store_dispatches_to_file_backend() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = ResolvedCredentialStore::File(FileStore::new(
            temp_dir.path().join(".codex/auth.json"),
            0o600,
        ));

        store.write_active(b"hello").expect("write");
        assert!(store.exists().expect("exists"));
        assert_eq!(store.read_active().expect("read"), b"hello");
    }
}
