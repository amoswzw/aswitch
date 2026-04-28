use std::collections::BTreeMap;
use std::process::Command;

use crate::store::{CredentialStore, StoreError};

const SECURITY_BIN: &str = "/usr/bin/security";
#[cfg(target_os = "linux")]
const SECRET_CONTENT_TYPE: &str = "text/plain";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeychainStore {
    backend: KeychainBackend,
    allow_empty_active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum KeychainBackend {
    Macos {
        service: String,
        account: String,
    },
    Linux {
        schema: String,
        attributes: BTreeMap<String, String>,
    },
}

impl KeychainStore {
    pub fn macos(service: String, account: String, allow_empty_active: bool) -> Self {
        Self {
            backend: KeychainBackend::Macos { service, account },
            allow_empty_active,
        }
    }

    pub fn linux(
        schema: String,
        attributes: BTreeMap<String, String>,
        allow_empty_active: bool,
    ) -> Self {
        Self {
            backend: KeychainBackend::Linux { schema, attributes },
            allow_empty_active,
        }
    }

    pub fn allow_empty_active(&self) -> bool {
        self.allow_empty_active
    }

    pub fn macos_service(&self) -> Option<&str> {
        match &self.backend {
            KeychainBackend::Macos { service, .. } => Some(service),
            KeychainBackend::Linux { .. } => None,
        }
    }

    pub fn macos_account(&self) -> Option<&str> {
        match &self.backend {
            KeychainBackend::Macos { account, .. } => Some(account),
            KeychainBackend::Linux { .. } => None,
        }
    }
}

impl CredentialStore for KeychainStore {
    fn read_active(&self) -> Result<Vec<u8>, StoreError> {
        match &self.backend {
            KeychainBackend::Macos { service, account } => {
                read_macos_password(service, account, self.allow_empty_active)
            }
            KeychainBackend::Linux { schema, attributes } => {
                read_linux_secret(schema, attributes, self.allow_empty_active)
            }
        }
    }

    fn write_active(&self, bytes: &[u8]) -> Result<(), StoreError> {
        match &self.backend {
            KeychainBackend::Macos { service, account } => {
                write_macos_password(service, account, bytes)
            }
            KeychainBackend::Linux { schema, attributes } => {
                write_linux_secret(schema, attributes, bytes)
            }
        }
    }

    fn clear_active(&self) -> Result<(), StoreError> {
        match &self.backend {
            KeychainBackend::Macos { service, account } => delete_macos_password(service, account),
            KeychainBackend::Linux { schema, attributes } => clear_linux_secret(schema, attributes),
        }
    }

    fn exists(&self) -> Result<bool, StoreError> {
        match &self.backend {
            KeychainBackend::Macos { service, account } => macos_password_exists(service, account),
            KeychainBackend::Linux { schema, attributes } => {
                linux_secret_exists(schema, attributes)
            }
        }
    }

    fn allows_missing_active(&self) -> bool {
        self.allow_empty_active
    }
}

fn read_macos_password(
    service: &str,
    account: &str,
    allow_empty_active: bool,
) -> Result<Vec<u8>, StoreError> {
    let output = Command::new(SECURITY_BIN)
        .args(["find-generic-password", "-w", "-s", service, "-a", account])
        .output()
        .map_err(|source| StoreError::CommandSpawn {
            program: SECURITY_BIN.into(),
            source,
        })?;

    match output.status.code() {
        Some(0) => Ok(trim_trailing_newline(output.stdout)),
        Some(44) if allow_empty_active => Ok(Vec::new()),
        Some(44) => Err(StoreError::KeychainItemNotFound {
            service: service.to_string(),
            account: account.to_string(),
        }),
        _ => Err(command_failure(
            "find-generic-password",
            service,
            account,
            output.status.code(),
            &output.stderr,
        )),
    }
}

fn write_macos_password(service: &str, account: &str, bytes: &[u8]) -> Result<(), StoreError> {
    let password = String::from_utf8(bytes.to_vec())
        .map_err(|source| StoreError::InvalidKeychainEncoding { source })?;
    let output = Command::new(SECURITY_BIN)
        .args([
            "add-generic-password",
            "-U",
            "-s",
            service,
            "-a",
            account,
            "-w",
            &password,
        ])
        .output()
        .map_err(|source| StoreError::CommandSpawn {
            program: SECURITY_BIN.into(),
            source,
        })?;

    match output.status.code() {
        Some(0) => Ok(()),
        _ => Err(command_failure(
            "add-generic-password",
            service,
            account,
            output.status.code(),
            &output.stderr,
        )),
    }
}

fn macos_password_exists(service: &str, account: &str) -> Result<bool, StoreError> {
    let output = Command::new(SECURITY_BIN)
        .args(["find-generic-password", "-s", service, "-a", account])
        .output()
        .map_err(|source| StoreError::CommandSpawn {
            program: SECURITY_BIN.into(),
            source,
        })?;

    match output.status.code() {
        Some(0) => Ok(true),
        Some(44) => Ok(false),
        _ => Err(command_failure(
            "find-generic-password",
            service,
            account,
            output.status.code(),
            &output.stderr,
        )),
    }
}

fn delete_macos_password(service: &str, account: &str) -> Result<(), StoreError> {
    let output = Command::new(SECURITY_BIN)
        .args(["delete-generic-password", "-s", service, "-a", account])
        .output()
        .map_err(|source| StoreError::CommandSpawn {
            program: SECURITY_BIN.into(),
            source,
        })?;

    match output.status.code() {
        Some(0) | Some(44) => Ok(()),
        _ => Err(command_failure(
            "delete-generic-password",
            service,
            account,
            output.status.code(),
            &output.stderr,
        )),
    }
}

fn command_failure(
    action: &'static str,
    service: &str,
    account: &str,
    status_code: Option<i32>,
    stderr: &[u8],
) -> StoreError {
    StoreError::CommandFailure {
        program: SECURITY_BIN.into(),
        action,
        service: service.to_string(),
        account: account.to_string(),
        status: status_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by signal".into()),
        stderr: String::from_utf8_lossy(stderr).trim().to_string(),
    }
}

fn trim_trailing_newline(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }

    bytes
}

#[cfg(target_os = "linux")]
fn read_linux_secret(
    schema: &str,
    attributes: &BTreeMap<String, String>,
    allow_empty_active: bool,
) -> Result<Vec<u8>, StoreError> {
    let ss = linux_secret_service_connect("read", schema, attributes)?;
    let item = match linux_find_item(&ss, attributes)? {
        Some(item) => item,
        None if allow_empty_active => return Ok(Vec::new()),
        None => {
            return Err(StoreError::SecretServiceItemNotFound {
                attributes: format_attributes(attributes),
            });
        }
    };

    item.get_secret()
        .map_err(|source| linux_failure("read", schema, attributes, source))
}

#[cfg(not(target_os = "linux"))]
fn read_linux_secret(
    _schema: &str,
    _attributes: &BTreeMap<String, String>,
    _allow_empty_active: bool,
) -> Result<Vec<u8>, StoreError> {
    Err(StoreError::UnsupportedKeychainBackend)
}

#[cfg(target_os = "linux")]
fn write_linux_secret(
    schema: &str,
    attributes: &BTreeMap<String, String>,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let ss = linux_secret_service_connect("write", schema, attributes)?;

    if let Some(item) = linux_find_item(&ss, attributes)? {
        return item
            .set_secret(bytes, SECRET_CONTENT_TYPE)
            .map_err(|source| linux_failure("write", schema, attributes, source));
    }

    let collection = ss
        .get_any_collection()
        .map_err(|source| linux_failure("get_any_collection", schema, attributes, source))?;
    let attr_refs = to_attr_refs(attributes);
    collection
        .create_item(schema, attr_refs, bytes, true, SECRET_CONTENT_TYPE)
        .map_err(|source| linux_failure("create_item", schema, attributes, source))?;

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn write_linux_secret(
    _schema: &str,
    _attributes: &BTreeMap<String, String>,
    _bytes: &[u8],
) -> Result<(), StoreError> {
    Err(StoreError::UnsupportedKeychainBackend)
}

#[cfg(target_os = "linux")]
fn linux_secret_exists(
    schema: &str,
    attributes: &BTreeMap<String, String>,
) -> Result<bool, StoreError> {
    let ss = linux_secret_service_connect("exists", schema, attributes)?;
    let exists = linux_find_item(&ss, attributes)?.is_some();
    Ok(exists)
}

#[cfg(not(target_os = "linux"))]
fn linux_secret_exists(
    _schema: &str,
    _attributes: &BTreeMap<String, String>,
) -> Result<bool, StoreError> {
    Err(StoreError::UnsupportedKeychainBackend)
}

#[cfg(target_os = "linux")]
fn clear_linux_secret(
    schema: &str,
    attributes: &BTreeMap<String, String>,
) -> Result<(), StoreError> {
    let ss = linux_secret_service_connect("delete", schema, attributes)?;
    let search = ss
        .search_items(to_attr_refs(attributes))
        .map_err(|source| linux_failure("search_items", schema, attributes, source))?;

    for item in search.unlocked {
        item.delete()
            .map_err(|source| linux_failure("delete", schema, attributes, source))?;
    }

    for item in search.locked {
        item.unlock()
            .map_err(|source| linux_failure("unlock", schema, attributes, source))?;
        item.delete()
            .map_err(|source| linux_failure("delete", schema, attributes, source))?;
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn clear_linux_secret(
    _schema: &str,
    _attributes: &BTreeMap<String, String>,
) -> Result<(), StoreError> {
    Err(StoreError::UnsupportedKeychainBackend)
}

#[cfg(target_os = "linux")]
fn linux_secret_service_connect(
    action: &'static str,
    schema: &str,
    attributes: &BTreeMap<String, String>,
) -> Result<secret_service::blocking::SecretService<'static>, StoreError> {
    use secret_service::{blocking::SecretService, EncryptionType};

    SecretService::connect(EncryptionType::Dh)
        .map_err(|source| linux_failure(action, schema, attributes, source))
}

#[cfg(target_os = "linux")]
fn linux_find_item<'a>(
    ss: &'a secret_service::blocking::SecretService<'a>,
    attributes: &BTreeMap<String, String>,
) -> Result<Option<secret_service::blocking::Item<'a>>, StoreError> {
    let search = ss
        .search_items(to_attr_refs(attributes))
        .map_err(|source| StoreError::SecretServiceFailure {
            action: "search_items",
            locator: format_attributes(attributes),
            message: source.to_string(),
        })?;

    if let Some(item) = search.unlocked.into_iter().next() {
        return Ok(Some(item));
    }

    if let Some(item) = search.locked.into_iter().next() {
        item.unlock()
            .map_err(|source| StoreError::SecretServiceFailure {
                action: "unlock",
                locator: format_attributes(attributes),
                message: source.to_string(),
            })?;
        return Ok(Some(item));
    }

    Ok(None)
}

#[cfg(target_os = "linux")]
fn to_attr_refs(attributes: &BTreeMap<String, String>) -> std::collections::HashMap<&str, &str> {
    attributes
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect()
}

#[cfg(target_os = "linux")]
fn format_attributes(attributes: &BTreeMap<String, String>) -> String {
    attributes
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(target_os = "linux")]
fn linux_failure(
    action: &'static str,
    schema: &str,
    attributes: &BTreeMap<String, String>,
    source: impl std::fmt::Display,
) -> StoreError {
    StoreError::SecretServiceFailure {
        action,
        locator: format!("{schema} [{}]", format_attributes(attributes)),
        message: source.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use std::sync::atomic::{AtomicU64, Ordering};
    #[cfg(target_os = "macos")]
    use std::thread;
    #[cfg(target_os = "macos")]
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::store::{CredentialStore, StoreError};

    use super::{Command, KeychainStore, SECURITY_BIN};

    #[cfg(target_os = "macos")]
    static TEST_KEYCHAIN_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn linux_backend_reports_unsupported() {
        let store = KeychainStore::linux(
            "org.freedesktop.Secret.Generic".into(),
            Default::default(),
            false,
        );

        assert!(matches!(
            store.read_active().expect_err("read should fail"),
            StoreError::UnsupportedKeychainBackend
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires writable macOS keychain access"]
    fn macos_keychain_round_trip_and_overwrite() {
        let scope = TestKeychainScope::new(false);

        scope
            .store
            .write_active(b"first-value")
            .expect("first write");
        assert!(wait_until_exists(&scope.store), "exists after first write");
        assert_eq!(
            scope.store.read_active().expect("first read"),
            b"first-value"
        );

        scope
            .store
            .write_active(b"second-value")
            .expect("second write");
        assert_eq!(
            scope.store.read_active().expect("second read"),
            b"second-value"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires writable macOS keychain access"]
    fn macos_allow_empty_active_returns_empty_for_missing_item() {
        let scope = TestKeychainScope::new(true);

        assert!(!scope.store.exists().expect("missing item should not exist"));
        assert_eq!(scope.store.read_active().expect("read missing"), b"");
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires writable macOS keychain access"]
    fn macos_missing_item_without_allow_empty_returns_not_found() {
        let scope = TestKeychainScope::new(false);

        assert!(matches!(
            scope
                .store
                .read_active()
                .expect_err("missing item should error"),
            StoreError::KeychainItemNotFound { .. }
        ));
    }

    #[cfg(target_os = "macos")]
    struct TestKeychainScope {
        service: String,
        account: String,
        store: KeychainStore,
    }

    #[cfg(target_os = "macos")]
    impl TestKeychainScope {
        fn new(allow_empty_active: bool) -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos();
            let counter = TEST_KEYCHAIN_COUNTER.fetch_add(1, Ordering::Relaxed);
            let service = format!("aswitch-test-{suffix}-{counter}");
            let account = "aswitch-test-account".to_string();

            delete_test_item(&service, &account).expect("cleanup before test");

            Self {
                store: KeychainStore::macos(service.clone(), account.clone(), allow_empty_active),
                service,
                account,
            }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for TestKeychainScope {
        fn drop(&mut self) {
            let _ = delete_test_item(&self.service, &self.account);
        }
    }

    #[cfg(target_os = "macos")]
    fn delete_test_item(service: &str, account: &str) -> Result<(), String> {
        let output = Command::new(SECURITY_BIN)
            .args(["delete-generic-password", "-s", service, "-a", account])
            .output()
            .map_err(|error| error.to_string())?;

        match output.status.code() {
            Some(0) | Some(44) => Ok(()),
            _ => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        }
    }

    #[cfg(target_os = "macos")]
    fn wait_until_exists(store: &KeychainStore) -> bool {
        for _ in 0..10 {
            if store.exists().unwrap_or(false) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }
}
