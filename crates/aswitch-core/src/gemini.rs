use std::env;
use std::fs;
use std::path::PathBuf;

use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::paths;

pub const GEMINI_PLUGIN_ID: &str = "gemini";

const CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com/v1internal";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeminiCodeAssistInfo {
    pub project_id: Option<String>,
    pub tier_name: Option<String>,
}

#[derive(Debug, Error)]
pub enum GeminiError {
    #[error("failed to resolve home directory")]
    Paths(#[from] crate::paths::PathsError),
    #[error("failed to read Gemini credentials from {path}")]
    ReadCredentials {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse Gemini credentials")]
    ParseCredentials(#[from] serde_json::Error),
    #[error("Gemini credentials do not contain an access token")]
    MissingAccessToken,
    #[error("Gemini access token is expired")]
    AccessTokenExpired,
    #[error("Gemini API request to {url} failed: {message}")]
    ApiRequest { url: String, message: String },
}

#[derive(Debug, Deserialize)]
struct GeminiCredentials {
    access_token: Option<String>,
    expiry_date: Option<i64>,
}

pub fn fetch_current_code_assist_info() -> Result<Option<GeminiCodeAssistInfo>, GeminiError> {
    let credential_bytes = match read_current_credentials() {
        Ok(bytes) => bytes,
        Err(GeminiError::ReadCredentials { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };

    Ok(Some(fetch_code_assist_info(&credential_bytes)?))
}

pub fn fetch_current_quota_summary() -> Result<Option<Map<String, Value>>, GeminiError> {
    let credential_bytes = match read_current_credentials() {
        Ok(bytes) => bytes,
        Err(GeminiError::ReadCredentials { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };

    let info = fetch_code_assist_info(&credential_bytes)?;
    let Some(project_id) = info.project_id.as_deref() else {
        return Ok(None);
    };

    Ok(Some(fetch_quota_summary(
        &credential_bytes,
        project_id,
        info.tier_name.as_deref(),
    )?))
}

pub fn fetch_code_assist_info(
    credential_bytes: &[u8],
) -> Result<GeminiCodeAssistInfo, GeminiError> {
    let access_token = current_access_token(credential_bytes)?;
    let url = format!("{CODE_ASSIST_ENDPOINT}:loadCodeAssist");
    let value = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(5))
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {access_token}"))
        .send_json(json!({
            "metadata": {
                "ideType": "IDE_UNSPECIFIED",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI"
            }
        }))
        .map_err(|error| GeminiError::ApiRequest {
            url: url.clone(),
            message: error.to_string(),
        })?
        .into_json::<Value>()
        .map_err(|error| GeminiError::ApiRequest {
            url: url.clone(),
            message: error.to_string(),
        })?;

    Ok(GeminiCodeAssistInfo {
        project_id: value
            .get("cloudaicompanionProject")
            .and_then(Value::as_str)
            .map(str::to_string),
        tier_name: value
            .pointer("/paidTier/name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                value
                    .pointer("/currentTier/name")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            }),
    })
}

pub fn fetch_quota_summary(
    credential_bytes: &[u8],
    project_id: &str,
    tier_name: Option<&str>,
) -> Result<Map<String, Value>, GeminiError> {
    let access_token = current_access_token(credential_bytes)?;
    let url = format!("{CODE_ASSIST_ENDPOINT}:retrieveUserQuota");
    let value = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(5))
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {access_token}"))
        .send_json(json!({ "project": project_id }))
        .map_err(|error| GeminiError::ApiRequest {
            url: url.clone(),
            message: error.to_string(),
        })?
        .into_json::<Value>()
        .map_err(|error| GeminiError::ApiRequest {
            url: url.clone(),
            message: error.to_string(),
        })?;

    let mut quota = Map::new();
    quota.insert(
        "project_id".to_string(),
        Value::String(project_id.to_string()),
    );
    if let Some(tier_name) = tier_name.filter(|value| !value.is_empty()) {
        quota.insert("plan".to_string(), Value::String(tier_name.to_string()));
    }

    let Some(buckets) = value.get("buckets").and_then(Value::as_array) else {
        return Ok(quota);
    };

    let mut fraction_sum = 0.0;
    let mut fraction_count = 0usize;
    let mut latest_reset = None::<String>;

    for bucket in buckets {
        let Some(remaining_fraction) = bucket.get("remainingFraction").and_then(Value::as_f64)
        else {
            continue;
        };
        fraction_sum += remaining_fraction;
        fraction_count += 1;

        if let Some(reset_time) = bucket.get("resetTime").and_then(Value::as_str) {
            if latest_reset
                .as_deref()
                .map(|current| reset_time > current)
                .unwrap_or(true)
            {
                latest_reset = Some(reset_time.to_string());
            }
        }
    }

    quota.insert(
        "bucket_count".to_string(),
        Value::from(buckets.len() as u64),
    );

    if fraction_count > 0 {
        let remaining_percent = (fraction_sum / fraction_count as f64) * 100.0;
        let used_percent = 100.0 - remaining_percent;
        quota.insert(
            "remaining_percent".to_string(),
            Value::from(remaining_percent),
        );
        quota.insert("used_percent".to_string(), Value::from(used_percent));
    }

    if let Some(reset_time) = latest_reset {
        quota.insert("reset_time".to_string(), Value::String(reset_time));
    }

    Ok(quota)
}

fn read_current_credentials() -> Result<Vec<u8>, GeminiError> {
    let path = current_credentials_path()?;
    fs::read(&path).map_err(|source| GeminiError::ReadCredentials { path, source })
}

fn current_credentials_path() -> Result<PathBuf, GeminiError> {
    let base = match env::var_os("GEMINI_CLI_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => paths::home_dir()?,
    };
    Ok(base.join(".gemini/oauth_creds.json"))
}

fn current_access_token(credential_bytes: &[u8]) -> Result<String, GeminiError> {
    let credentials = serde_json::from_slice::<GeminiCredentials>(credential_bytes)?;
    let access_token = credentials
        .access_token
        .filter(|value| !value.is_empty())
        .ok_or(GeminiError::MissingAccessToken)?;

    if let Some(expiry_ms) = credentials.expiry_date {
        let now_ms = Utc::now().timestamp_millis();
        if expiry_ms <= now_ms + 30_000 {
            return Err(GeminiError::AccessTokenExpired);
        }
    }

    Ok(access_token)
}
