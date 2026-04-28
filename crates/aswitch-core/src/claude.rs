use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use chrono::{DateTime, Duration, Local, LocalResult, TimeZone, Utc};
use glob::glob;
use regex::Regex;
use serde_json::{json, Map, Value};
use thiserror::Error;

pub const CLAUDE_PLUGIN_ID: &str = "claude-code";

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_REFRESH_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_SCOPES: &str = "user:profile user:inference user:sessions:claude_code";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeQuotaProbe {
    pub quota: Map<String, Value>,
    pub updated_credentials: Option<Vec<u8>>,
}

#[derive(Debug, Error)]
pub enum ClaudeError {
    #[error("failed to parse Claude credentials")]
    ParseCredentials(#[from] serde_json::Error),
    #[error("Claude credentials do not contain an access token")]
    MissingAccessToken,
    #[error("Claude credentials do not contain a refresh token")]
    MissingRefreshToken,
    #[error("Claude usage API is rate limited{retry_after}")]
    RateLimited { retry_after: String },
    #[error("Claude authentication is required")]
    AuthenticationRequired,
    #[error("Claude API request to {url} failed: {message}")]
    ApiRequest { url: String, message: String },
}

pub fn fetch_quota_summary(credential_bytes: &[u8]) -> Result<ClaudeQuotaProbe, ClaudeError> {
    let mut credentials = serde_json::from_slice::<Value>(credential_bytes)?;
    let mut updated_credentials = None;

    let response = match fetch_usage_response(&credentials) {
        Ok(response) => response,
        Err(ClaudeError::AuthenticationRequired) => {
            refresh_credentials(&mut credentials)?;
            updated_credentials = Some(
                serde_json::to_vec_pretty(&credentials).map_err(ClaudeError::ParseCredentials)?,
            );
            fetch_usage_response(&credentials)?
        }
        Err(error) => return Err(error),
    };

    let subscription_type = credentials
        .pointer("/claudeAiOauth/subscriptionType")
        .and_then(Value::as_str);
    let rate_limit_tier = credentials
        .pointer("/claudeAiOauth/rateLimitTier")
        .and_then(Value::as_str);

    Ok(ClaudeQuotaProbe {
        quota: quota_from_usage_response(&response, subscription_type, rate_limit_tier),
        updated_credentials,
    })
}

pub fn parse_cli_quota_output(raw_output: &str) -> Option<Map<String, Value>> {
    let collapsed = collapse_terminal_output(raw_output);
    if collapsed.is_empty() {
        return None;
    }

    let percent_pattern = Regex::new(r"(?i)(\d+(?:\.\d+)?)%(used|left)").ok()?;
    let reset_pattern =
        Regex::new(r"(?i)res\w*?([0-9]{1,2}(?::[0-9]{2})?(?:am|pm)\([^)]+\))").ok()?;

    let mut quota = Map::new();
    if let Some(captures) = percent_pattern.captures(&collapsed) {
        let value = captures.get(1)?.as_str().parse::<f64>().ok()?;
        match captures.get(2)?.as_str().to_ascii_lowercase().as_str() {
            "used" => {
                quota.insert("used_percent".to_string(), Value::from(value));
                quota.insert(
                    "remaining_percent".to_string(),
                    Value::from((100.0 - value).clamp(0.0, 100.0)),
                );
            }
            "left" => {
                quota.insert("remaining_percent".to_string(), Value::from(value));
                quota.insert(
                    "used_percent".to_string(),
                    Value::from((100.0 - value).clamp(0.0, 100.0)),
                );
            }
            _ => {}
        }
    }

    if let Some(captures) = reset_pattern.captures(&collapsed) {
        let human = captures.get(1)?.as_str();
        if let Some(reset_time) = parse_meridiem_reset(human) {
            quota.insert("reset_time".to_string(), Value::String(reset_time));
        }
    }

    if quota.is_empty() {
        None
    } else {
        Some(quota)
    }
}

pub fn local_quota_hint_from_home(home_dir: &Path) -> Option<Map<String, Value>> {
    let pattern = home_dir.join(".claude/projects/**/*.jsonl");
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
            let Some((timestamp, quota)) = local_quota_hint_from_line(&line) else {
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

    let (timestamp, quota) = latest?;
    // Only trust rate-limit hints from the current 5h rolling window. An older
    // "hit your limit" line means the user has long since recovered; surfacing
    // its 0% would mask fresher session-block data.
    if !timestamp_within_session_window(&timestamp, Utc::now()) {
        return None;
    }
    Some(quota)
}

fn timestamp_within_session_window(timestamp: &str, now: DateTime<Utc>) -> bool {
    let Ok(parsed) = DateTime::parse_from_rfc3339(timestamp) else {
        return false;
    };
    now.signed_duration_since(parsed.with_timezone(&Utc)) <= Duration::hours(SESSION_BLOCK_HOURS)
}

const SESSION_BLOCK_HOURS: i64 = 5;

pub fn local_session_quota_from_home(
    home_dir: &Path,
    plan: Option<&str>,
) -> Option<Map<String, Value>> {
    let pattern = home_dir.join(".claude/projects/**/*.jsonl");
    let pattern = pattern.to_string_lossy().into_owned();

    let mut entries: Vec<(DateTime<Utc>, u64)> = Vec::new();
    for entry in glob(&pattern).ok()? {
        let Ok(path) = entry else {
            continue;
        };
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Some(record) = parse_assistant_token_record(&line) {
                entries.push(record);
            }
        }
    }

    build_session_quota(&mut entries, Utc::now(), plan)
}

fn build_session_quota(
    entries: &mut [(DateTime<Utc>, u64)],
    now: DateTime<Utc>,
    plan: Option<&str>,
) -> Option<Map<String, Value>> {
    if entries.is_empty() {
        return None;
    }
    entries.sort_by_key(|(ts, _)| *ts);

    let block = active_session_block(entries, now)?;
    let mut quota = Map::new();
    quota.insert(
        "quota_scope".to_string(),
        Value::String("session".to_string()),
    );
    quota.insert(
        "reset_time".to_string(),
        Value::String(block.end.to_rfc3339()),
    );
    quota.insert(
        "session_reset_time".to_string(),
        Value::String(block.end.to_rfc3339()),
    );

    if let Some(limit) = plan_token_limit(plan) {
        let used_percent = ((block.tokens_used as f64 / limit as f64) * 100.0).clamp(0.0, 100.0);
        let remaining_percent = (100.0 - used_percent).clamp(0.0, 100.0);
        quota.insert("used_percent".to_string(), Value::from(used_percent));
        quota.insert(
            "remaining_percent".to_string(),
            Value::from(remaining_percent),
        );
        quota.insert(
            "session_used_percent".to_string(),
            Value::from(used_percent),
        );
        quota.insert(
            "session_remaining_percent".to_string(),
            Value::from(remaining_percent),
        );
    }

    Some(quota)
}

#[derive(Clone, Debug, PartialEq)]
struct SessionBlock {
    end: DateTime<Utc>,
    tokens_used: u64,
}

fn active_session_block(
    entries: &[(DateTime<Utc>, u64)],
    now: DateTime<Utc>,
) -> Option<SessionBlock> {
    let block_duration = Duration::hours(SESSION_BLOCK_HOURS);
    let gap_threshold = block_duration;

    let mut block_start: Option<DateTime<Utc>> = None;
    let mut block_tokens: u64 = 0;
    let mut last_ts: Option<DateTime<Utc>> = None;

    for (ts, tokens) in entries {
        match (block_start, last_ts) {
            (Some(start), Some(prev)) => {
                let gap = *ts - prev;
                let elapsed = *ts - start;
                if gap > gap_threshold || elapsed > block_duration {
                    block_start = Some(floor_to_hour_utc(*ts));
                    block_tokens = *tokens;
                } else {
                    block_tokens = block_tokens.saturating_add(*tokens);
                }
            }
            _ => {
                block_start = Some(floor_to_hour_utc(*ts));
                block_tokens = *tokens;
            }
        }
        last_ts = Some(*ts);
    }

    let start = block_start?;
    let end = start + block_duration;
    if now >= end {
        return None;
    }
    Some(SessionBlock {
        end,
        tokens_used: block_tokens,
    })
}

fn parse_assistant_token_record(line: &str) -> Option<(DateTime<Utc>, u64)> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("type").and_then(Value::as_str)? != "assistant" {
        return None;
    }
    let ts_str = value.get("timestamp").and_then(Value::as_str)?;
    let timestamp = DateTime::parse_from_rfc3339(ts_str)
        .ok()?
        .with_timezone(&Utc);
    let usage = value.pointer("/message/usage")?;
    Some((timestamp, usage_total_tokens(usage)))
}

fn usage_total_tokens(usage: &Value) -> u64 {
    const KEYS: &[&str] = &[
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ];
    let mut sum = 0u64;
    for key in KEYS {
        if let Some(value) = usage.get(*key).and_then(Value::as_u64) {
            sum = sum.saturating_add(value);
        }
    }
    sum
}

fn floor_to_hour_utc(ts: DateTime<Utc>) -> DateTime<Utc> {
    let seconds = ts.timestamp();
    let floored = seconds - seconds.rem_euclid(3600);
    Utc.timestamp_opt(floored, 0).single().unwrap_or(ts)
}

// Hardcoded plan token caps per 5h session window (ccusage observational defaults).
// Total tokens here = input + output + cache_creation + cache_read; the percentages
// are best-effort and may drift from Anthropic's billing.
fn plan_token_limit(plan: Option<&str>) -> Option<u64> {
    let normalized = plan?.to_ascii_lowercase().replace(['_', '-', ' '], "");
    match normalized.as_str() {
        "pro" => Some(19_000),
        "max" | "max5" | "max5x" => Some(88_000),
        "max20" | "max20x" => Some(220_000),
        _ => None,
    }
}

fn fetch_usage_response(credentials: &Value) -> Result<Value, ClaudeError> {
    let access_token = credentials
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(ClaudeError::MissingAccessToken)?;

    match ureq::get(CLAUDE_USAGE_URL)
        .timeout(std::time::Duration::from_secs(5))
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("Accept", "application/json")
        .set("Content-Type", "application/json")
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("User-Agent", "aswitch")
        .call()
    {
        Ok(response) => response
            .into_json::<Value>()
            .map_err(|error| ClaudeError::ApiRequest {
                url: CLAUDE_USAGE_URL.to_string(),
                message: error.to_string(),
            }),
        Err(ureq::Error::Status(status, response)) if status == 401 || status == 403 => {
            let _ = response.into_string();
            Err(ClaudeError::AuthenticationRequired)
        }
        Err(ureq::Error::Status(status, response)) if status == 429 => {
            let retry_after = response
                .header("retry-after")
                .map(|value| format!(" (retry after {value}s)"))
                .unwrap_or_default();
            let _ = response.into_string();
            Err(ClaudeError::RateLimited { retry_after })
        }
        Err(ureq::Error::Status(_, response)) => Err(ClaudeError::ApiRequest {
            url: CLAUDE_USAGE_URL.to_string(),
            message: response
                .into_string()
                .unwrap_or_else(|error| error.to_string()),
        }),
        Err(ureq::Error::Transport(error)) => Err(ClaudeError::ApiRequest {
            url: CLAUDE_USAGE_URL.to_string(),
            message: error.to_string(),
        }),
    }
}

fn refresh_credentials(credentials: &mut Value) -> Result<(), ClaudeError> {
    let refresh_token = credentials
        .pointer("/claudeAiOauth/refreshToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(ClaudeError::MissingRefreshToken)?;

    let response = match ureq::post(CLAUDE_REFRESH_URL)
        .timeout(std::time::Duration::from_secs(5))
        .set("Content-Type", "application/json")
        .send_json(json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLAUDE_CLIENT_ID,
            "scope": CLAUDE_SCOPES,
        })) {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) if status == 400 || status == 401 => {
            let _ = response.into_string();
            return Err(ClaudeError::AuthenticationRequired);
        }
        Err(ureq::Error::Status(status, response)) if status == 429 => {
            let retry_after = response
                .header("retry-after")
                .map(|value| format!(" (retry after {value}s)"))
                .unwrap_or_default();
            let _ = response.into_string();
            return Err(ClaudeError::RateLimited { retry_after });
        }
        Err(ureq::Error::Status(_, response)) => {
            return Err(ClaudeError::ApiRequest {
                url: CLAUDE_REFRESH_URL.to_string(),
                message: response
                    .into_string()
                    .unwrap_or_else(|error| error.to_string()),
            });
        }
        Err(ureq::Error::Transport(error)) => {
            return Err(ClaudeError::ApiRequest {
                url: CLAUDE_REFRESH_URL.to_string(),
                message: error.to_string(),
            });
        }
    };

    let refreshed = response
        .into_json::<Value>()
        .map_err(|error| ClaudeError::ApiRequest {
            url: CLAUDE_REFRESH_URL.to_string(),
            message: error.to_string(),
        })?;

    let Some(new_access_token) = refreshed
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Err(ClaudeError::ApiRequest {
            url: CLAUDE_REFRESH_URL.to_string(),
            message: "refresh response did not contain access_token".to_string(),
        });
    };

    if let Some(slot) = credentials.pointer_mut("/claudeAiOauth/accessToken") {
        *slot = Value::String(new_access_token.to_string());
    }
    if let Some(new_refresh_token) = refreshed
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        if let Some(slot) = credentials.pointer_mut("/claudeAiOauth/refreshToken") {
            *slot = Value::String(new_refresh_token.to_string());
        }
    }
    if let Some(expires_in) = refreshed.get("expires_in").and_then(Value::as_i64) {
        let expires_at = Utc::now().timestamp_millis() + expires_in * 1000;
        if let Some(slot) = credentials.pointer_mut("/claudeAiOauth/expiresAt") {
            *slot = Value::from(expires_at);
        }
    }

    Ok(())
}

fn quota_from_usage_response(
    value: &Value,
    subscription_type: Option<&str>,
    rate_limit_tier: Option<&str>,
) -> Map<String, Value> {
    let mut quota = Map::new();

    if let Some(plan) = subscription_type
        .filter(|value| !value.is_empty())
        .or(rate_limit_tier.filter(|value| !value.is_empty()))
    {
        quota.insert("plan".to_string(), Value::String(plan.to_string()));
    }

    let session = anthropic_window_to_quota(value.pointer("/five_hour"), "session");
    let weekly = anthropic_window_to_quota(value.pointer("/seven_day"), "weekly");
    let sonnet = anthropic_window_to_quota(value.pointer("/seven_day_sonnet"), "sonnet");
    let opus = anthropic_window_to_quota(value.pointer("/seven_day_opus"), "opus");

    if let Some(window) = session.as_ref() {
        insert_window(&mut quota, "session", window);
    }
    if let Some(window) = weekly.as_ref() {
        insert_window(&mut quota, "weekly", window);
    }
    if let Some(window) = sonnet.as_ref() {
        insert_window(&mut quota, "sonnet", window);
    }
    if let Some(window) = opus.as_ref() {
        insert_window(&mut quota, "opus", window);
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

fn anthropic_window_to_quota(window: Option<&Value>, scope: &'static str) -> Option<WindowQuota> {
    let window = window?;
    let used_percent = window.get("utilization").and_then(Value::as_f64)?;
    Some(WindowQuota {
        scope,
        remaining_percent: (100.0 - used_percent).clamp(0.0, 100.0),
        used_percent: used_percent.clamp(0.0, 100.0),
        reset_time: window
            .get("resets_at")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
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

fn local_quota_hint_from_line(line: &str) -> Option<(String, Map<String, Value>)> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let timestamp = value.get("timestamp")?.as_str()?.to_string();
    let error = value.get("error").and_then(Value::as_str);
    let text = value
        .pointer("/message/content")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find_map(|item| item.get("text").and_then(Value::as_str).map(str::to_string))
        })?;

    if error != Some("rate_limit") && !text.to_ascii_lowercase().contains("hit your limit") {
        return None;
    }

    let mut quota = Map::new();
    quota.insert("remaining_percent".to_string(), Value::from(0.0));
    quota.insert("used_percent".to_string(), Value::from(100.0));
    quota.insert(
        "quota_scope".to_string(),
        Value::String("session".to_string()),
    );

    if let Some(reset_time) = parse_reset_from_message(&text) {
        quota.insert("reset_time".to_string(), Value::String(reset_time));
    }

    Some((timestamp, quota))
}

fn collapse_terminal_output(raw_output: &str) -> String {
    let osc_pattern = Regex::new(r"\x1b\][^\x07]*(?:\x07|\x1b\\)").expect("valid regex");
    let csi_pattern = Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("valid regex");
    let stray_pattern = Regex::new(r"\[[<>=0-9;?A-Za-z]+").expect("valid regex");

    let stripped = osc_pattern.replace_all(raw_output, "");
    let stripped = csi_pattern.replace_all(&stripped, "");
    let stripped = stray_pattern.replace_all(&stripped, "");

    stripped
        .chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
}

fn parse_reset_from_message(text: &str) -> Option<String> {
    let pattern =
        Regex::new(r"(?i)resets?\s*([0-9]{1,2}(?::[0-9]{2})?\s*(?:am|pm)\s*\([^)]+\))").ok()?;
    let captures = pattern.captures(text)?;
    parse_meridiem_reset(captures.get(1)?.as_str())
}

fn parse_meridiem_reset(value: &str) -> Option<String> {
    let pattern = Regex::new(r"(?i)(\d{1,2})(?::(\d{2}))?\s*(am|pm)\s*\(([^)]+)\)").ok()?;
    let captures = pattern.captures(value)?;
    let hour = captures.get(1)?.as_str().parse::<u32>().ok()?;
    let minute = captures
        .get(2)
        .map(|value| value.as_str().parse::<u32>().ok())
        .unwrap_or(Some(0))?;
    let meridiem = captures.get(3)?.as_str().to_ascii_lowercase();

    let mut hour_24 = hour % 12;
    if meridiem == "pm" {
        hour_24 += 12;
    }

    let now = Local::now();
    let naive = now.date_naive().and_hms_opt(hour_24, minute, 0)?;
    let mut candidate = match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(first, _) => first,
        LocalResult::None => return None,
    };
    if candidate <= now {
        candidate += Duration::days(1);
    }

    Some(candidate.to_rfc3339())
}

#[derive(Clone, Debug, PartialEq)]
struct WindowQuota {
    scope: &'static str,
    remaining_percent: f64,
    used_percent: f64,
    reset_time: Option<String>,
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use serde_json::{json, Value};

    use super::{
        active_session_block, build_session_quota, local_quota_hint_from_line,
        parse_assistant_token_record, parse_cli_quota_output, parse_reset_from_message,
        plan_token_limit, quota_from_usage_response, timestamp_within_session_window,
        SESSION_BLOCK_HOURS,
    };

    #[test]
    fn claude_api_prefers_most_constrained_window() {
        let value = json!({
            "five_hour": {
                "utilization": 20.0,
                "resets_at": "2026-04-26T03:40:00Z"
            },
            "seven_day": {
                "utilization": 100.0,
                "resets_at": "2026-04-28T03:40:00Z"
            },
            "seven_day_sonnet": {
                "utilization": 10.0,
                "resets_at": "2026-04-28T03:40:00Z"
            }
        });

        let quota = quota_from_usage_response(&value, Some("pro"), Some("default_claude_ai"));
        assert_eq!(quota.get("plan"), Some(&json!("pro")));
        assert_eq!(quota.get("session_remaining_percent"), Some(&json!(80.0)));
        assert_eq!(quota.get("weekly_remaining_percent"), Some(&json!(0.0)));
        assert_eq!(quota.get("remaining_percent"), Some(&json!(0.0)));
        assert_eq!(quota.get("used_percent"), Some(&json!(100.0)));
        assert_eq!(quota.get("quota_scope"), Some(&json!("weekly")));
    }

    #[test]
    fn claude_cli_output_extracts_remaining_and_reset() {
        let output = "Curretsession0%usedReses11:40am(Asia/Shanghai)Extrausage";
        let quota = parse_cli_quota_output(output).expect("quota");
        assert_eq!(quota.get("used_percent"), Some(&json!(0.0)));
        assert_eq!(quota.get("remaining_percent"), Some(&json!(100.0)));
        assert!(quota.get("reset_time").and_then(Value::as_str).is_some());
    }

    #[test]
    fn claude_local_rate_limit_line_extracts_hint() {
        let line = r#"{"timestamp":"2026-04-12T03:16:12.543Z","error":"rate_limit","message":{"content":[{"text":"You've hit your limit · resets 3pm (Asia/Shanghai)"}]}}"#;
        let (_, quota) = local_quota_hint_from_line(line).expect("quota");
        assert_eq!(quota.get("remaining_percent"), Some(&json!(0.0)));
        assert_eq!(quota.get("used_percent"), Some(&json!(100.0)));
        assert!(quota.get("reset_time").and_then(Value::as_str).is_some());
    }

    #[test]
    fn claude_reset_message_parser_handles_spaces() {
        assert!(
            parse_reset_from_message("You've hit your limit · resets 3pm (Asia/Shanghai)")
                .is_some()
        );
    }

    fn at(time: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(time)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn session_block_active_when_recent() {
        let entries = vec![
            (at("2026-04-25T20:30:00Z"), 4_000),
            (at("2026-04-25T22:15:00Z"), 6_000),
        ];
        let now = at("2026-04-25T23:00:00Z");
        let block = active_session_block(&entries, now).expect("active");
        // Block start floors to 20:00, end = 20:00 + 5h = 25:00 = next day 01:00.
        assert_eq!(block.end, at("2026-04-26T01:00:00Z"));
        assert_eq!(block.tokens_used, 10_000);
    }

    #[test]
    fn session_block_returns_none_when_expired() {
        let entries = vec![(at("2026-04-25T10:00:00Z"), 1_000)];
        let now = at("2026-04-25T23:00:00Z");
        assert!(active_session_block(&entries, now).is_none());
    }

    #[test]
    fn session_block_starts_new_after_long_gap() {
        let entries = vec![
            (at("2026-04-25T08:00:00Z"), 5_000),
            // 9 hour gap, so a new block starts at the next message.
            (at("2026-04-25T17:30:00Z"), 2_500),
        ];
        let now = at("2026-04-25T19:00:00Z");
        let block = active_session_block(&entries, now).expect("active");
        // New block aligned to 17:00, ends 22:00; only second-block tokens count.
        assert_eq!(block.end, at("2026-04-25T22:00:00Z"));
        assert_eq!(block.tokens_used, 2_500);
    }

    #[test]
    fn build_session_quota_computes_percentages_for_pro() {
        let mut entries = vec![(
            Utc::now() - Duration::minutes(30),
            // ~50% of pro 19_000 budget
            9_500,
        )];
        let quota = build_session_quota(&mut entries, Utc::now(), Some("pro")).expect("quota");
        let used = quota.get("used_percent").and_then(Value::as_f64).unwrap();
        let remaining = quota
            .get("remaining_percent")
            .and_then(Value::as_f64)
            .unwrap();
        assert!(
            used > 49.0 && used < 51.0,
            "unexpected used_percent: {used}"
        );
        assert!(
            (used + remaining - 100.0).abs() < 0.001,
            "used + remaining must be 100"
        );
        assert_eq!(quota.get("quota_scope"), Some(&json!("session")));
        assert!(quota.get("reset_time").and_then(Value::as_str).is_some());
    }

    #[test]
    fn build_session_quota_omits_percent_when_plan_unknown() {
        let mut entries = vec![(Utc::now() - Duration::minutes(10), 100)];
        let quota = build_session_quota(&mut entries, Utc::now(), None).expect("quota");
        assert!(quota.get("remaining_percent").is_none());
        assert!(quota.get("used_percent").is_none());
        assert!(quota.get("reset_time").and_then(Value::as_str).is_some());
    }

    #[test]
    fn parse_assistant_token_record_sums_usage_fields() {
        let line = r#"{"timestamp":"2026-04-25T22:00:00Z","type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":5,"cache_read_input_tokens":7}}}"#;
        let (ts, tokens) = parse_assistant_token_record(line).expect("record");
        assert_eq!(tokens, 42);
        assert_eq!(ts, at("2026-04-25T22:00:00Z"));
    }

    #[test]
    fn parse_assistant_token_record_skips_non_assistant() {
        let line = r#"{"timestamp":"2026-04-25T22:00:00Z","type":"user","message":{"usage":{"input_tokens":10}}}"#;
        assert!(parse_assistant_token_record(line).is_none());
    }

    #[test]
    fn plan_token_limit_normalizes_input() {
        assert_eq!(plan_token_limit(Some("pro")), Some(19_000));
        assert_eq!(plan_token_limit(Some("Pro")), Some(19_000));
        assert_eq!(plan_token_limit(Some("max5")), Some(88_000));
        assert_eq!(plan_token_limit(Some("max-5")), Some(88_000));
        assert_eq!(plan_token_limit(Some("max20x")), Some(220_000));
        assert_eq!(plan_token_limit(Some("default_claude_ai")), None);
        assert_eq!(plan_token_limit(None), None);
    }

    #[test]
    fn rate_limit_hint_freshness_is_bounded_by_session_window() {
        let now = at("2026-04-25T22:00:00Z");
        // 4h ago, within the window.
        assert!(timestamp_within_session_window("2026-04-25T18:00:00Z", now));
        // 6h ago, expired.
        assert!(!timestamp_within_session_window(
            "2026-04-25T16:00:00Z",
            now
        ));
        // Unparseable input is ignored.
        assert!(!timestamp_within_session_window("not a timestamp", now));
    }

    #[test]
    fn session_block_hours_constant_is_five() {
        // Anthropic's rolling-rate-limit window is 5h; touch the constant so
        // accidental edits trip CI.
        assert_eq!(SESSION_BLOCK_HOURS, 5);
        let _ = Utc.timestamp_opt(0, 0).single();
    }
}
