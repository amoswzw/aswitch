use std::fs;
use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::paths::{self, PathsError};
use crate::plugin::{IdentityExtract, IdentitySource, Manifest, ValueSource};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityExtraction {
    pub identity: Identity,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

pub fn extract(manifest: &Manifest, credential_bytes: &[u8]) -> Result<Identity, IdentityError> {
    Ok(extract_with_warnings(manifest, credential_bytes)?.identity)
}

pub fn extract_with_warnings(
    manifest: &Manifest,
    credential_bytes: &[u8],
) -> Result<IdentityExtraction, IdentityError> {
    let home_dir = paths::home_dir()?;
    Ok(extract_with_home_dir(manifest, credential_bytes, &home_dir))
}

pub(crate) fn extract_with_home_dir(
    manifest: &Manifest,
    credential_bytes: &[u8],
    home_dir: &Path,
) -> IdentityExtraction {
    let credential_json = serde_json::from_slice::<Value>(credential_bytes).ok();
    let mut identity = Identity::default();
    let mut warnings = Vec::new();

    for rule in &manifest.identity_extract {
        let value = extract_rule_value(
            rule,
            false,
            credential_json.as_ref(),
            home_dir,
            &mut warnings,
        )
        .or_else(|| {
            extract_rule_value(
                rule,
                true,
                credential_json.as_ref(),
                home_dir,
                &mut warnings,
            )
        });

        if let Some(value) = value {
            assign_identity_field(&mut identity, &rule.field, value);
        }
    }

    IdentityExtraction { identity, warnings }
}

fn extract_rule_value(
    rule: &IdentityExtract,
    fallback: bool,
    credential_json: Option<&Value>,
    home_dir: &Path,
    warnings: &mut Vec<String>,
) -> Option<Value> {
    let source = if fallback {
        rule.fallback_source
    } else {
        Some(rule.source)
    }?;

    match source {
        IdentitySource::JsonFile => {
            let json = load_json_file(select_path(rule, fallback), home_dir, warnings)?;
            get_json_path(&json, select_pointer(rule, fallback)?)
        }
        IdentitySource::JsonValue => {
            let json = credential_json?;
            get_json_path(json, select_pointer(rule, fallback)?)
        }
        IdentitySource::JwtClaim => extract_from_jwt_claim(
            select_jwt_value_source(rule, fallback),
            select_jwt_pointer(rule, fallback),
            select_claim_pointer(rule, fallback),
            select_path(rule, fallback),
            credential_json,
            home_dir,
            warnings,
        ),
        IdentitySource::JsonTopKeys => extract_json_top_keys(
            select_json_top_keys_source(rule, fallback),
            select_path(rule, fallback),
            credential_json,
            home_dir,
            warnings,
        ),
        IdentitySource::Literal => select_literal(rule, fallback).map(Value::String),
    }
}

fn extract_from_jwt_claim(
    value_source: Option<ValueSource>,
    jwt_pointer: Option<&str>,
    claim_pointer: Option<&str>,
    file_path: Option<&str>,
    credential_json: Option<&Value>,
    home_dir: &Path,
    warnings: &mut Vec<String>,
) -> Option<Value> {
    let jwt = extract_string_from_value_source(
        value_source?,
        jwt_pointer?,
        file_path,
        credential_json,
        home_dir,
        warnings,
    )?;

    let payload = match decode_jwt_payload(&jwt) {
        Ok(payload) => payload,
        Err(source) => {
            warnings.push(format!("failed to decode jwt payload: {source}"));
            return None;
        }
    };

    get_json_path(&payload, claim_pointer?)
}

fn extract_json_top_keys(
    value_source: Option<ValueSource>,
    file_path: Option<&str>,
    credential_json: Option<&Value>,
    home_dir: &Path,
    warnings: &mut Vec<String>,
) -> Option<Value> {
    let json = match value_source? {
        ValueSource::JsonValue => credential_json.cloned(),
        ValueSource::JsonFile => load_json_file(file_path, home_dir, warnings),
    }?;

    let Value::Object(map) = json else {
        return None;
    };

    let mut keys = map.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    Some(Value::String(keys.join(", ")))
}

fn extract_string_from_value_source(
    value_source: ValueSource,
    pointer: &str,
    file_path: Option<&str>,
    credential_json: Option<&Value>,
    home_dir: &Path,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let value = match value_source {
        ValueSource::JsonValue => {
            let json = credential_json?;
            get_json_path(json, pointer)
        }
        ValueSource::JsonFile => {
            let json = load_json_file(file_path, home_dir, warnings)?;
            get_json_path(&json, pointer)
        }
    }?;

    value_to_string(&value)
}

fn load_json_file(
    path: Option<&str>,
    home_dir: &Path,
    warnings: &mut Vec<String>,
) -> Option<Value> {
    let expanded_path = paths::expand_user_path_from(path?, home_dir);
    let contents = match fs::read_to_string(&expanded_path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return None,
        Err(source) => {
            warnings.push(format!(
                "failed to read json file {}: {source}",
                expanded_path.display()
            ));
            return None;
        }
    };

    match serde_json::from_str(&contents) {
        Ok(json) => Some(json),
        Err(source) => {
            warnings.push(format!(
                "failed to parse json file {}: {source}",
                expanded_path.display()
            ));
            None
        }
    }
}

pub(crate) fn decode_jwt_payload(jwt: &str) -> Result<Value, JwtDecodeError> {
    let mut parts = jwt.split('.');
    let _header = parts.next().ok_or(JwtDecodeError::InvalidFormat)?;
    let payload = parts.next().ok_or(JwtDecodeError::InvalidFormat)?;
    let _signature = parts.next().ok_or(JwtDecodeError::InvalidFormat)?;

    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(JwtDecodeError::Base64)?;
    serde_json::from_slice(&decoded).map_err(JwtDecodeError::Json)
}

pub(crate) fn get_json_path(value: &Value, pointer: &str) -> Option<Value> {
    if pointer.is_empty() {
        return Some(value.clone());
    }

    if let Value::Object(map) = value {
        if let Some(exact) = map.get(pointer) {
            return Some(exact.clone());
        }
    }

    let segments = parse_pointer(pointer)?;
    let mut current = value;

    for segment in segments {
        match segment {
            PathSegment::Field(field) => {
                current = current.get(field)?;
            }
            PathSegment::Index(index) => {
                current = current.get(index)?;
            }
        }
    }

    Some(current.clone())
}

fn parse_pointer(pointer: &str) -> Option<Vec<PathSegment<'_>>> {
    let mut segments = Vec::new();
    let mut start = 0;
    let bytes = pointer.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'.' => {
                if start == index {
                    return None;
                }
                segments.push(PathSegment::Field(&pointer[start..index]));
                index += 1;
                start = index;
            }
            b'[' => {
                if start < index {
                    segments.push(PathSegment::Field(&pointer[start..index]));
                }

                let close = pointer[index + 1..].find(']')? + index + 1;
                let segment = &pointer[index + 1..close];
                let parsed = segment.parse::<usize>().ok()?;
                segments.push(PathSegment::Index(parsed));
                index = close + 1;
                start = index;

                if index < bytes.len() && bytes[index] == b'.' {
                    index += 1;
                    start = index;
                }
            }
            _ => {
                index += 1;
            }
        }
    }

    if start < pointer.len() {
        segments.push(PathSegment::Field(&pointer[start..]));
    }

    Some(segments)
}

fn assign_identity_field(identity: &mut Identity, field: &str, value: Value) {
    match field {
        "email" => identity.email = value_to_string(&value),
        "org_name" => identity.org_name = value_to_string(&value),
        "plan" => identity.plan = value_to_string(&value),
        other => {
            identity.extra.insert(other.to_string(), value);
        }
    }
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(value).ok(),
    }
}

fn select_path<'a>(rule: &'a IdentityExtract, fallback: bool) -> Option<&'a str> {
    if fallback {
        rule.fallback_json_file_path.as_deref()
    } else {
        rule.json_file_path.as_deref()
    }
}

fn select_pointer<'a>(rule: &'a IdentityExtract, fallback: bool) -> Option<&'a str> {
    if fallback {
        rule.fallback_json_pointer.as_deref()
    } else {
        rule.json_pointer.as_deref()
    }
}

fn select_jwt_value_source(rule: &IdentityExtract, fallback: bool) -> Option<ValueSource> {
    if fallback {
        rule.fallback_jwt_from
    } else {
        rule.jwt_from
    }
}

fn select_json_top_keys_source(rule: &IdentityExtract, fallback: bool) -> Option<ValueSource> {
    if fallback {
        rule.fallback_json_top_keys_from
    } else {
        rule.json_top_keys_from
    }
}

fn select_jwt_pointer<'a>(rule: &'a IdentityExtract, fallback: bool) -> Option<&'a str> {
    if fallback {
        rule.fallback_jwt_json_pointer.as_deref()
    } else {
        rule.jwt_json_pointer.as_deref()
    }
}

fn select_claim_pointer<'a>(rule: &'a IdentityExtract, fallback: bool) -> Option<&'a str> {
    if fallback {
        rule.fallback_claim_pointer.as_deref()
    } else {
        rule.claim_pointer.as_deref()
    }
}

fn select_literal(rule: &IdentityExtract, fallback: bool) -> Option<String> {
    if fallback {
        rule.fallback_literal.clone()
    } else {
        rule.literal.clone()
    }
}

enum PathSegment<'a> {
    Field(&'a str),
    Index(usize),
}

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("failed to resolve user paths")]
    Paths(#[from] PathsError),
}

#[derive(Debug, Error)]
pub(crate) enum JwtDecodeError {
    #[error("jwt must contain three segments")]
    InvalidFormat,
    #[error("invalid base64url payload")]
    Base64(#[source] base64::DecodeError),
    #[error("invalid jwt payload json")]
    Json(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use serde_json::{json, Value};

    use crate::plugin::load_manifest;

    use super::{extract_with_home_dir, get_json_path, Identity};

    #[test]
    fn get_json_path_supports_dotted_paths_and_array_indexes() {
        let value = json!({
            "oauthAccount": {
                "emails": [
                    {"address": "amos@example.com"}
                ]
            }
        });

        assert_eq!(
            get_json_path(&value, "oauthAccount.emails[0].address"),
            Some(json!("amos@example.com"))
        );
        assert_eq!(get_json_path(&value, "oauthAccount.missing"), None);
    }

    #[test]
    fn extract_supports_json_value_source() {
        let manifest = crate::plugin::Manifest {
            id: "json-value".into(),
            display_name: "json-value".into(),
            version: "1.0.0".into(),
            author: "test".into(),
            description: "test".into(),
            platforms: vec![crate::plugin::Platform::Macos],
            credential_store: crate::plugin::CredentialStore {
                kind: crate::plugin::CredentialStoreKind::File,
                path: Some("~/.test/auth.json".into()),
                permissions: Some(0o600),
                macos_service: None,
                macos_account: None,
                linux_schema: None,
                linux_attributes: Default::default(),
                linux_fallback_kind: None,
                linux_fallback_path: None,
                linux_fallback_permissions: None,
                allow_empty_active: false,
            },
            session_activation: None,
            aux_files: vec![],
            identity_extract: vec![crate::plugin::IdentityExtract {
                field: "plan".into(),
                source: crate::plugin::IdentitySource::JsonValue,
                json_file_path: None,
                json_pointer: Some("plan".into()),
                fallback_source: None,
                fallback_json_file_path: None,
                fallback_json_pointer: None,
                jwt_from: None,
                jwt_json_pointer: None,
                claim_pointer: None,
                fallback_jwt_from: None,
                fallback_jwt_json_pointer: None,
                fallback_claim_pointer: None,
                json_top_keys_from: None,
                fallback_json_top_keys_from: None,
                literal: None,
                fallback_literal: None,
            }],
            login: crate::plugin::LoginConfig {
                cmd: vec!["test".into()],
                ready_marker_kind: crate::plugin::ReadyMarkerKind::File,
                ready_marker_timeout_s: 30,
                ready_marker_path: Some("~/.test/auth.json".into()),
            },
            usage_source: vec![],
        };

        let extraction = extract_with_home_dir(
            &manifest,
            br#"{"plan":"plus"}"#,
            Path::new("/tmp/aswitch-home"),
        );

        assert_eq!(
            extraction.identity,
            Identity {
                email: None,
                org_name: None,
                plan: Some("plus".into()),
                extra: Default::default(),
            }
        );
    }

    #[test]
    fn official_manifests_extract_expected_identity() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let home_dir = temp_dir.path();

        let claude_manifest = load_official_manifest("claude-code");
        write_json(
            &home_dir.join(".claude/.claude.json"),
            json!({
                "oauthAccount": {
                    "emailAddress": "amos@example.com",
                    "organizationName": "Amos",
                    "organizationUuid": "org_123"
                }
            }),
        );
        let claude = extract_with_home_dir(&claude_manifest, br#"{}"#, home_dir);
        assert_eq!(claude.identity.email.as_deref(), Some("amos@example.com"));
        assert_eq!(claude.identity.org_name.as_deref(), Some("Amos"));
        assert_eq!(
            claude.identity.extra.get("org_uuid"),
            Some(&json!("org_123"))
        );

        let codex_manifest = load_official_manifest("codex");
        let codex = extract_with_home_dir(
            &codex_manifest,
            credential_json_with_id_token(json!({
                "email": "amos@example.com",
                "https://api.openai.com/auth.chatgpt_account_id": "acct_123",
                "https://api.openai.com/auth.chatgpt_user_id": "user_123",
                "https://api.openai.com/auth.chatgpt_plan_type": "plus"
            }))
            .as_bytes(),
            home_dir,
        );
        assert_eq!(codex.identity.email.as_deref(), Some("amos@example.com"));
        assert_eq!(codex.identity.plan.as_deref(), Some("plus"));
        assert_eq!(
            codex.identity.extra.get("chatgpt_account_id"),
            Some(&json!("acct_123"))
        );
        assert_eq!(
            codex.identity.extra.get("chatgpt_user_id"),
            Some(&json!("user_123"))
        );

        let gemini_manifest = load_official_manifest("gemini");
        write_json(&home_dir.join(".gemini/settings.json"), json!({}));
        let gemini = extract_with_home_dir(
            &gemini_manifest,
            serde_json::to_string(&json!({
                "id_token": make_jwt(json!({"email": "gemini@example.com"}))
            }))
            .expect("serialize")
            .as_bytes(),
            home_dir,
        );
        assert_eq!(gemini.identity.email.as_deref(), Some("gemini@example.com"));

        let opencode_manifest = load_official_manifest("opencode");
        let opencode = extract_with_home_dir(
            &opencode_manifest,
            serde_json::to_string(&json!({
                "anthropic": {
                    "access": make_jwt(json!({"email": "opencode@example.com"}))
                },
                "openai": {
                    "api_key": "sk-test"
                }
            }))
            .expect("serialize")
            .as_bytes(),
            home_dir,
        );
        assert_eq!(
            opencode.identity.email.as_deref(),
            Some("opencode@example.com")
        );
        assert_eq!(
            opencode.identity.extra.get("providers"),
            Some(&json!("anthropic, openai"))
        );
    }

    fn load_official_manifest(id: &str) -> crate::plugin::Manifest {
        let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/bundled-plugins")
            .join(id)
            .join("plugin.toml");

        load_manifest(&manifest_path)
            .expect("load manifest")
            .manifest
    }

    fn write_json(path: &Path, value: Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, serde_json::to_vec(&value).expect("serialize")).expect("write");
    }

    fn make_jwt(payload: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("payload"));
        format!("{header}.{payload}.signature")
    }

    fn credential_json_with_id_token(payload: Value) -> String {
        serde_json::to_string(&json!({
            "tokens": {
                "id_token": make_jwt(payload)
            }
        }))
        .expect("serialize")
    }
}
