use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use chrono::{DateTime, Duration, TimeZone, Utc};
use glob::glob;
use serde_json::{Map, Value};
use thiserror::Error;

pub const CODEX_PLUGIN_ID: &str = "codex";

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodexQuotaProbe {
    pub quota: Map<String, Value>,
    pub updated_credentials: Option<Vec<u8>>,
}

#[derive(Debug, Error)]
pub enum CodexError {
    #[error("failed to parse Codex credentials")]
    ParseCredentials(#[from] serde_json::Error),
    #[error("Codex credentials do not contain an access token")]
    MissingAccessToken,
    #[error("Codex credentials do not contain a refresh token")]
    MissingRefreshToken,
    #[error("Codex usage API request to {url} failed: {message}")]
    ApiRequest { url: String, message: String },
    #[error("Codex authentication is required")]
    AuthenticationRequired,
}

pub fn fetch_quota_summary(credential_bytes: &[u8]) -> Result<CodexQuotaProbe, CodexError> {
    let mut credentials = serde_json::from_slice::<Value>(credential_bytes)?;
    let mut updated_credentials = None;

    let mut response = match fetch_usage_response(&credentials) {
        Ok(response) => response,
        Err(CodexError::AuthenticationRequired) => {
            refresh_credentials(&mut credentials)?;
            updated_credentials = Some(
                serde_json::to_vec_pretty(&credentials).map_err(CodexError::ParseCredentials)?,
            );
            fetch_usage_response(&credentials)?
        }
        Err(error) => return Err(error),
    };

    if response.is_null() {
        response = Value::Object(Map::new());
    }

    Ok(CodexQuotaProbe {
        quota: quota_from_usage_response(&response, Utc::now()),
        updated_credentials,
    })
}

pub fn local_quota_summary_from_home(home_dir: &Path) -> Option<Map<String, Value>> {
    let pattern = home_dir.join(".codex/sessions/**/*.jsonl");
    let pattern = pattern.to_string_lossy().into_owned();

    let mut latest = None::<(String, Map<String, Value>)>;
    for entry in glob(&pattern).ok()? {
        let Ok(path) = entry else {
            continue;
        };
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };

        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Some((timestamp, quota)) = local_quota_summary_from_line(&line) else {
                continue;
            };
            if latest
                .as_ref()
                .map(|(current, _)| timestamp > *current)
                .unwrap_or(true)
            {
                latest = Some((timestamp, quota));
            }
        }
    }

    latest.map(|(_, quota)| quota)
}

fn fetch_usage_response(credentials: &Value) -> Result<Value, CodexError> {
    let access_token = access_token(credentials)?;
    let account_id = credentials
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let mut request = ureq::get(CODEX_USAGE_URL)
        .timeout(std::time::Duration::from_secs(5))
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("Accept", "application/json")
        .set("User-Agent", "aswitch");
    if let Some(account_id) = account_id.as_deref() {
        request = request.set("ChatGPT-Account-Id", account_id);
    }

    match request.call() {
        Ok(response) => response
            .into_json::<Value>()
            .map_err(|error| CodexError::ApiRequest {
                url: CODEX_USAGE_URL.to_string(),
                message: error.to_string(),
            }),
        Err(ureq::Error::Status(status, response)) if status == 401 || status == 403 => {
            let _ = response.into_string();
            Err(CodexError::AuthenticationRequired)
        }
        Err(ureq::Error::Status(_, response)) => Err(CodexError::ApiRequest {
            url: CODEX_USAGE_URL.to_string(),
            message: response
                .into_string()
                .unwrap_or_else(|error| error.to_string()),
        }),
        Err(ureq::Error::Transport(error)) => Err(CodexError::ApiRequest {
            url: CODEX_USAGE_URL.to_string(),
            message: error.to_string(),
        }),
    }
}

fn refresh_credentials(credentials: &mut Value) -> Result<(), CodexError> {
    let refresh_token = credentials
        .pointer("/tokens/refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(CodexError::MissingRefreshToken)?;

    let body = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}",
        CODEX_CLIENT_ID,
        urlencoding::encode(refresh_token)
    );

    let response = match ureq::post(CODEX_REFRESH_URL)
        .timeout(std::time::Duration::from_secs(5))
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
    {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) if status == 400 || status == 401 => {
            let _ = response.into_string();
            return Err(CodexError::AuthenticationRequired);
        }
        Err(ureq::Error::Status(_, response)) => {
            return Err(CodexError::ApiRequest {
                url: CODEX_REFRESH_URL.to_string(),
                message: response
                    .into_string()
                    .unwrap_or_else(|error| error.to_string()),
            });
        }
        Err(ureq::Error::Transport(error)) => {
            return Err(CodexError::ApiRequest {
                url: CODEX_REFRESH_URL.to_string(),
                message: error.to_string(),
            });
        }
    };

    let refreshed = response
        .into_json::<Value>()
        .map_err(|error| CodexError::ApiRequest {
            url: CODEX_REFRESH_URL.to_string(),
            message: error.to_string(),
        })?;

    let Some(new_access_token) = refreshed
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Err(CodexError::ApiRequest {
            url: CODEX_REFRESH_URL.to_string(),
            message: "refresh response did not contain access_token".to_string(),
        });
    };

    if let Some(slot) = credentials.pointer_mut("/tokens/access_token") {
        *slot = Value::String(new_access_token.to_string());
    }
    if let Some(new_refresh_token) = refreshed
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        if let Some(slot) = credentials.pointer_mut("/tokens/refresh_token") {
            *slot = Value::String(new_refresh_token.to_string());
        }
    }
    if let Some(new_id_token) = refreshed
        .get("id_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        if let Some(slot) = credentials.pointer_mut("/tokens/id_token") {
            *slot = Value::String(new_id_token.to_string());
        }
    }

    let refreshed_at = Utc::now().to_rfc3339();
    if let Some(slot) = credentials.pointer_mut("/last_refresh") {
        *slot = Value::String(refreshed_at);
    }

    Ok(())
}

fn access_token(credentials: &Value) -> Result<String, CodexError> {
    credentials
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or(CodexError::MissingAccessToken)
}

fn quota_from_usage_response(value: &Value, now: DateTime<Utc>) -> Map<String, Value> {
    let mut quota = Map::new();

    if let Some(plan) = value
        .get("plan_type")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        quota.insert("plan".to_string(), Value::String(plan.to_string()));
    }

    if let Some(balance) = value
        .pointer("/credits/balance")
        .and_then(number_from_value)
    {
        quota.insert("credits_balance".to_string(), Value::from(balance));
    }

    let session =
        codex_window_to_quota(value.pointer("/rate_limit/primary_window"), "session", now);
    let weekly =
        codex_window_to_quota(value.pointer("/rate_limit/secondary_window"), "weekly", now);

    if let Some(window) = session.as_ref() {
        insert_window(&mut quota, "session", window);
    }
    if let Some(window) = weekly.as_ref() {
        insert_window(&mut quota, "weekly", window);
    }

    if let Some(main) = pick_main_window(session.as_ref(), weekly.as_ref()) {
        quota.insert(
            "remaining_percent".to_string(),
            Value::from(main.remaining_percent),
        );
        quota.insert("used_percent".to_string(), Value::from(main.used_percent));
        if let Some(reset_time) = main.reset_time.as_ref() {
            quota.insert("reset_time".to_string(), Value::String(reset_time.clone()));
        }
        quota.insert(
            "quota_scope".to_string(),
            Value::String(main.scope.to_string()),
        );
    }

    quota
}

fn local_quota_summary_from_line(line: &str) -> Option<(String, Map<String, Value>)> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    if value.pointer("/payload/type")?.as_str()? != "token_count" {
        return None;
    }
    let timestamp = value.get("timestamp")?.as_str()?.to_string();
    let rate_limits = value.pointer("/payload/rate_limits")?;
    let plan_type = value
        .pointer("/payload/plan_type")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut quota = Map::new();
    if let Some(plan_type) = plan_type {
        quota.insert("plan".to_string(), Value::String(plan_type));
    }

    let session = local_window_to_quota(rate_limits.get("primary"), "session");
    let weekly = local_window_to_quota(rate_limits.get("secondary"), "weekly");

    if let Some(window) = session.as_ref() {
        insert_window(&mut quota, "session", window);
    }
    if let Some(window) = weekly.as_ref() {
        insert_window(&mut quota, "weekly", window);
    }

    if let Some(main) = pick_main_window(session.as_ref(), weekly.as_ref()) {
        quota.insert(
            "remaining_percent".to_string(),
            Value::from(main.remaining_percent),
        );
        quota.insert("used_percent".to_string(), Value::from(main.used_percent));
        if let Some(reset_time) = main.reset_time.as_ref() {
            quota.insert("reset_time".to_string(), Value::String(reset_time.clone()));
        }
        quota.insert(
            "quota_scope".to_string(),
            Value::String(main.scope.to_string()),
        );
    }

    Some((timestamp, quota))
}

#[derive(Clone, Debug, PartialEq)]
struct WindowQuota {
    scope: &'static str,
    remaining_percent: f64,
    used_percent: f64,
    reset_time: Option<String>,
}

fn codex_window_to_quota(
    window: Option<&Value>,
    scope: &'static str,
    now: DateTime<Utc>,
) -> Option<WindowQuota> {
    let window = window?;
    let used_percent = window.get("used_percent").and_then(Value::as_f64)?;
    Some(WindowQuota {
        scope,
        remaining_percent: (100.0 - used_percent).clamp(0.0, 100.0),
        used_percent: used_percent.clamp(0.0, 100.0),
        reset_time: window_reset_from_api(window, now),
    })
}

fn local_window_to_quota(window: Option<&Value>, scope: &'static str) -> Option<WindowQuota> {
    let window = window?;
    let used_percent = window.get("used_percent").and_then(Value::as_f64)?;
    Some(WindowQuota {
        scope,
        remaining_percent: (100.0 - used_percent).clamp(0.0, 100.0),
        used_percent: used_percent.clamp(0.0, 100.0),
        reset_time: window
            .get("resets_at")
            .and_then(number_from_value)
            .and_then(unix_seconds_to_rfc3339),
    })
}

fn insert_window(quota: &mut Map<String, Value>, prefix: &str, window: &WindowQuota) {
    quota.insert(
        format!("{prefix}_remaining_percent"),
        Value::from(window.remaining_percent),
    );
    quota.insert(
        format!("{prefix}_used_percent"),
        Value::from(window.used_percent),
    );
    if let Some(reset_time) = window.reset_time.as_ref() {
        quota.insert(
            format!("{prefix}_reset_time"),
            Value::String(reset_time.clone()),
        );
    }
}

fn pick_main_window<'a>(
    session: Option<&'a WindowQuota>,
    weekly: Option<&'a WindowQuota>,
) -> Option<&'a WindowQuota> {
    match (session, weekly) {
        (Some(session), Some(weekly)) => {
            if weekly.remaining_percent <= session.remaining_percent {
                Some(weekly)
            } else {
                Some(session)
            }
        }
        (Some(session), None) => Some(session),
        (None, Some(weekly)) => Some(weekly),
        (None, None) => None,
    }
}

fn window_reset_from_api(window: &Value, now: DateTime<Utc>) -> Option<String> {
    if let Some(reset_at) = window.get("reset_at").and_then(number_from_value) {
        return unix_seconds_to_rfc3339(reset_at);
    }

    let seconds = window
        .get("reset_after_seconds")
        .and_then(number_from_value)?;
    Some((now + Duration::seconds(seconds as i64)).to_rfc3339())
}

fn unix_seconds_to_rfc3339(value: f64) -> Option<String> {
    Utc.timestamp_opt(value as i64, 0)
        .single()
        .map(|timestamp| timestamp.to_rfc3339())
}

fn number_from_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.parse::<f64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{local_quota_summary_from_line, quota_from_usage_response};
    use chrono::Utc;

    #[test]
    fn codex_api_prefers_most_constrained_window() {
        let value = json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 0,
                    "reset_at": 1777174973
                },
                "secondary_window": {
                    "used_percent": 100,
                    "reset_at": 1777423747
                }
            }
        });

        let quota = quota_from_usage_response(&value, Utc::now());
        assert_eq!(quota.get("plan"), Some(&json!("plus")));
        assert_eq!(quota.get("remaining_percent"), Some(&json!(0.0)));
        assert_eq!(quota.get("used_percent"), Some(&json!(100.0)));
        assert_eq!(quota.get("weekly_remaining_percent"), Some(&json!(0.0)));
        assert_eq!(quota.get("session_remaining_percent"), Some(&json!(100.0)));
        assert_eq!(quota.get("quota_scope"), Some(&json!("weekly")));
    }

    #[test]
    fn codex_local_rate_limit_line_extracts_quota() {
        let line = r#"{"timestamp":"2026-04-24T18:25:40.670Z","type":"event_msg","payload":{"type":"token_count","plan_type":"plus","rate_limits":{"primary":{"used_percent":7.0,"resets_at":1777010074},"secondary":{"used_percent":60.0,"resets_at":1777423747}}}}"#;
        let (_, quota) = local_quota_summary_from_line(line).expect("quota");
        assert_eq!(quota.get("plan"), Some(&json!("plus")));
        assert_eq!(quota.get("session_remaining_percent"), Some(&json!(93.0)));
        assert_eq!(quota.get("weekly_remaining_percent"), Some(&json!(40.0)));
        assert_eq!(quota.get("remaining_percent"), Some(&json!(40.0)));
    }
}
