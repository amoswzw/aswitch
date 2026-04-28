use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aswitch_core::account::AccountRecord;
use aswitch_core::claude;
use aswitch_core::codex;
use aswitch_core::gemini::{self, GeminiCodeAssistInfo};
use aswitch_core::paths::{self, AswitchPaths};
use aswitch_core::usage::{UsageMetrics, UsageSnapshot, UsageSourceSummary};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::scope::EffectiveAccountRecord;

const CLAUDE_PLUGIN_ID: &str = claude::CLAUDE_PLUGIN_ID;
const CLAUDE_API_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_CLI_USAGE_CMD: &str = "claude /usage";
const CLAUDE_LOCAL_LOG_HINT: &str = "~/.claude/projects/**/*.jsonl";
const CLAUDE_LOCAL_SESSION_HINT: &str = "~/.claude/projects/**/*.jsonl (session block)";
const CODEX_API_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_LOCAL_LOG_HINT: &str = "~/.codex/sessions/**/*.jsonl";
const GEMINI_API_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal";
const PRIMARY_CREDENTIALS_FILE: &str = "primary.creds";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ClaudeCodeInfo {
    email: Option<String>,
    org_name: Option<String>,
}

pub(crate) fn enrich_effective_accounts(rows: &mut [EffectiveAccountRecord]) {
    if rows
        .iter()
        .any(|row| row.plugin_id == CLAUDE_PLUGIN_ID && row.alias.is_some())
    {
        if let Some(info) = fetch_current_claude_info() {
            for row in rows
                .iter_mut()
                .filter(|row| row.plugin_id == CLAUDE_PLUGIN_ID && row.alias.is_some())
            {
                apply_claude_info(&mut row.email, &mut row.org_name, &info);
            }
        }
    }

    if !rows
        .iter()
        .any(|row| row.plugin_id == gemini::GEMINI_PLUGIN_ID && row.alias.is_some())
    {
        return;
    }

    let Ok(Some(info)) = gemini::fetch_current_code_assist_info() else {
        return;
    };

    for row in rows
        .iter_mut()
        .filter(|row| row.plugin_id == gemini::GEMINI_PLUGIN_ID && row.alias.is_some())
    {
        apply_code_assist_info(&mut row.org_name, &mut row.plan, &info);
    }
}

pub(crate) fn enrich_saved_accounts(
    rows: &mut [AccountRecord],
    effective_rows: &[EffectiveAccountRecord],
) {
    let current_claude_alias = effective_rows
        .iter()
        .find(|row| row.plugin_id == CLAUDE_PLUGIN_ID)
        .and_then(|row| row.alias.as_deref());
    if let Some(info) = fetch_current_claude_info() {
        if let Some(current_alias) = current_claude_alias {
            if let Some(row) = rows
                .iter_mut()
                .find(|row| row.plugin_id == CLAUDE_PLUGIN_ID && row.alias == current_alias)
            {
                apply_claude_info(&mut row.email, &mut row.org_name, &info);
            }
        } else {
            let mut claude_rows = rows
                .iter_mut()
                .filter(|row| row.plugin_id == CLAUDE_PLUGIN_ID)
                .collect::<Vec<_>>();
            if claude_rows.len() == 1 {
                let row = claude_rows.remove(0);
                apply_claude_info(&mut row.email, &mut row.org_name, &info);
            }
        }
    }

    let Some(current_alias) = effective_rows
        .iter()
        .find(|row| row.plugin_id == gemini::GEMINI_PLUGIN_ID)
        .and_then(|row| row.alias.as_deref())
    else {
        return;
    };

    let Ok(Some(info)) = gemini::fetch_current_code_assist_info() else {
        return;
    };

    if let Some(row) = rows
        .iter_mut()
        .find(|row| row.plugin_id == gemini::GEMINI_PLUGIN_ID && row.alias == current_alias)
    {
        apply_code_assist_info(&mut row.org_name, &mut row.plan, &info);
    }
}

pub(crate) fn enrich_usage_snapshot(
    snapshot: &mut UsageSnapshot,
    config_dir: Option<PathBuf>,
    is_effective: bool,
) {
    if apply_live_cache(snapshot, config_dir.clone()) {
        return;
    }

    let before = SnapshotMarks::from(snapshot);

    match snapshot.plugin_id.as_str() {
        gemini::GEMINI_PLUGIN_ID => {
            enrich_gemini_usage_snapshot(snapshot, config_dir.clone(), is_effective)
        }
        codex::CODEX_PLUGIN_ID => {
            enrich_codex_usage_snapshot(snapshot, config_dir.clone(), is_effective)
        }
        claude::CLAUDE_PLUGIN_ID => {
            enrich_claude_usage_snapshot(snapshot, config_dir.clone(), is_effective)
        }
        _ => return,
    }

    write_live_cache(snapshot, &before, config_dir);
}

struct SnapshotMarks {
    sources_len: usize,
    warnings_len: usize,
    quota_keys: std::collections::HashSet<String>,
}

impl SnapshotMarks {
    fn from(snapshot: &UsageSnapshot) -> Self {
        Self {
            sources_len: snapshot.sources.len(),
            warnings_len: snapshot.warnings.len(),
            quota_keys: snapshot.quota.keys().cloned().collect(),
        }
    }
}

const LIVE_CACHE_FRESH_SECS: u64 = 60;
const LIVE_CACHE_RATE_LIMITED_SECS: u64 = 300;
const LIVE_CACHE_FILE: &str = "live_quota.json";

#[derive(Serialize, Deserialize)]
struct LiveCache {
    saved_at: DateTime<Utc>,
    ttl_s: u64,
    quota: Map<String, Value>,
    sources: Vec<UsageSourceSummary>,
    warnings: Vec<String>,
}

fn live_cache_path(plugin_id: &str, alias: &str, config_dir: Option<PathBuf>) -> Option<PathBuf> {
    let paths = AswitchPaths::resolve(config_dir).ok()?;
    Some(
        paths
            .usage_cache_dir
            .join(plugin_id)
            .join(alias)
            .join(LIVE_CACHE_FILE),
    )
}

fn apply_live_cache(snapshot: &mut UsageSnapshot, config_dir: Option<PathBuf>) -> bool {
    let Some(path) = live_cache_path(&snapshot.plugin_id, &snapshot.alias, config_dir) else {
        return false;
    };
    let Ok(bytes) = fs::read(&path) else {
        return false;
    };
    let Ok(cache) = serde_json::from_slice::<LiveCache>(&bytes) else {
        let _ = fs::remove_file(&path);
        return false;
    };

    let age = Utc::now()
        .signed_duration_since(cache.saved_at)
        .num_seconds();
    if age < 0 || age as u64 >= cache.ttl_s {
        return false;
    }

    snapshot.quota.extend(cache.quota.clone());
    snapshot.sources.extend(cache.sources.iter().cloned());
    snapshot.warnings.extend(cache.warnings.iter().cloned());
    true
}

fn write_live_cache(snapshot: &UsageSnapshot, before: &SnapshotMarks, config_dir: Option<PathBuf>) {
    let Some(path) = live_cache_path(&snapshot.plugin_id, &snapshot.alias, config_dir) else {
        return;
    };

    let new_sources: Vec<UsageSourceSummary> = snapshot
        .sources
        .iter()
        .skip(before.sources_len)
        .cloned()
        .collect();
    let new_warnings: Vec<String> = snapshot
        .warnings
        .iter()
        .skip(before.warnings_len)
        .cloned()
        .collect();
    let new_quota: Map<String, Value> = snapshot
        .quota
        .iter()
        .filter(|(key, _)| !before.quota_keys.contains(key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();

    if new_sources.is_empty() && new_warnings.is_empty() && new_quota.is_empty() {
        // Nothing to memoize; leave cache untouched so older fresh data still wins.
        return;
    }

    let rate_limited = new_warnings
        .iter()
        .any(|warning| warning.to_ascii_lowercase().contains("rate"));
    let ttl_s = if rate_limited {
        LIVE_CACHE_RATE_LIMITED_SECS
    } else {
        LIVE_CACHE_FRESH_SECS
    };

    let cache = LiveCache {
        saved_at: Utc::now(),
        ttl_s,
        quota: new_quota,
        sources: new_sources,
        warnings: new_warnings,
    };

    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = fs::write(&path, bytes);
    }
}

fn enrich_gemini_usage_snapshot(
    snapshot: &mut UsageSnapshot,
    config_dir: Option<PathBuf>,
    is_effective: bool,
) {
    if has_source_kind(snapshot, "gemini_api") {
        return;
    }

    let credential_bytes =
        match read_saved_credentials(&snapshot.plugin_id, &snapshot.alias, config_dir) {
            Ok(bytes) => bytes,
            Err(error) => {
                if is_effective {
                    return enrich_current_gemini_usage_snapshot(snapshot, error);
                }
                snapshot
                    .warnings
                    .push(format!("failed to load saved Gemini credentials: {error}"));
                return;
            }
        };

    let info = match gemini::fetch_code_assist_info(&credential_bytes) {
        Ok(info) => info,
        Err(error) => {
            if is_effective {
                return enrich_current_gemini_usage_snapshot(snapshot, error.to_string());
            }
            snapshot
                .warnings
                .push(format!("failed to fetch live Gemini quota: {error}"));
            return;
        }
    };

    let Some(project_id) = info.project_id.as_deref() else {
        snapshot.warnings.push(
            "Gemini quota is unavailable because the saved Gemini account has no project id"
                .to_string(),
        );
        return;
    };

    match gemini::fetch_quota_summary(&credential_bytes, project_id, info.tier_name.as_deref()) {
        Ok(quota) => apply_quota_source(snapshot, "gemini_api", GEMINI_API_URL, quota),
        Err(error) => {
            if is_effective {
                enrich_current_gemini_usage_snapshot(snapshot, error.to_string());
            } else {
                snapshot
                    .warnings
                    .push(format!("failed to fetch live Gemini quota: {error}"));
            }
        }
    }
}

fn enrich_current_gemini_usage_snapshot(snapshot: &mut UsageSnapshot, cause: impl ToString) {
    match gemini::fetch_current_quota_summary() {
        Ok(Some(quota)) => apply_quota_source(snapshot, "gemini_api", GEMINI_API_URL, quota),
        Ok(None) => snapshot.warnings.push(format!(
            "failed to fetch live Gemini quota from saved credentials: {}; current Gemini credentials are unavailable",
            cause.to_string()
        )),
        Err(error) => snapshot.warnings.push(format!(
            "failed to fetch live Gemini quota from saved credentials: {}; current Gemini quota probe also failed: {error}",
            cause.to_string()
        )),
    }
}

fn enrich_codex_usage_snapshot(
    snapshot: &mut UsageSnapshot,
    config_dir: Option<PathBuf>,
    is_effective: bool,
) {
    if has_source_kind(snapshot, "codex_api") || has_source_kind(snapshot, "codex_local_log") {
        return;
    }

    let mut warning = None::<String>;
    match read_saved_credentials(&snapshot.plugin_id, &snapshot.alias, config_dir.clone()) {
        Ok(credential_bytes) => match codex::fetch_quota_summary(&credential_bytes) {
            Ok(probe) => {
                if let Some(updated_credentials) = probe.updated_credentials {
                    let _ = write_saved_credentials(
                        &snapshot.plugin_id,
                        &snapshot.alias,
                        config_dir.clone(),
                        &updated_credentials,
                    );
                }
                apply_quota_source(snapshot, "codex_api", CODEX_API_URL, probe.quota);
            }
            Err(error) => {
                warning = Some(format!("failed to fetch live Codex quota: {error}"));
            }
        },
        Err(error) => {
            warning = Some(format!("failed to load saved Codex credentials: {error}"));
        }
    }

    if snapshot.quota.is_empty() && is_effective {
        if let Ok(home_dir) = paths::home_dir() {
            if let Some(quota) = codex::local_quota_summary_from_home(&home_dir) {
                apply_quota_source(snapshot, "codex_local_log", CODEX_LOCAL_LOG_HINT, quota);
            }
        }
    }

    if snapshot.quota.is_empty() {
        if let Some(warning) = warning {
            snapshot.warnings.push(warning);
        }
    }
}

fn enrich_claude_usage_snapshot(
    snapshot: &mut UsageSnapshot,
    config_dir: Option<PathBuf>,
    is_effective: bool,
) {
    if has_source_kind(snapshot, "claude_api")
        || has_source_kind(snapshot, "claude_cli")
        || has_source_kind(snapshot, "claude_local_hint")
        || has_source_kind(snapshot, "claude_local_session")
    {
        return;
    }

    let mut warning = None::<String>;
    let mut plan: Option<String> = None;
    let mut api_rate_limited = false;
    match read_saved_credentials(&snapshot.plugin_id, &snapshot.alias, config_dir.clone()) {
        Ok(credential_bytes) => {
            plan = claude_plan_from_credentials(&credential_bytes);
            match claude::fetch_quota_summary(&credential_bytes) {
                Ok(probe) => {
                    if let Some(updated_credentials) = probe.updated_credentials {
                        let _ = write_saved_credentials(
                            &snapshot.plugin_id,
                            &snapshot.alias,
                            config_dir.clone(),
                            &updated_credentials,
                        );
                    }
                    apply_quota_source(snapshot, "claude_api", CLAUDE_API_URL, probe.quota);
                }
                Err(error) => {
                    if matches!(error, claude::ClaudeError::RateLimited { .. }) {
                        api_rate_limited = true;
                    }
                    warning = Some(format!("failed to fetch live Claude quota: {error}"));
                }
            }
        }
        Err(error) => {
            warning = Some(format!("failed to load saved Claude credentials: {error}"));
        }
    }

    // `claude /usage` hits the same endpoint we just got rate-limited from, so
    // it would only burn another ~4s timeout. Skip and go straight to local data.
    if snapshot.quota.is_empty() && is_effective && !api_rate_limited {
        if let Some(quota) = fetch_current_claude_cli_quota() {
            apply_quota_source(snapshot, "claude_cli", CLAUDE_CLI_USAGE_CMD, quota);
        }
    }

    if snapshot.quota.is_empty() && is_effective {
        if let Ok(home_dir) = paths::home_dir() {
            if let Some(quota) = claude::local_quota_hint_from_home(&home_dir) {
                apply_quota_source(snapshot, "claude_local_hint", CLAUDE_LOCAL_LOG_HINT, quota);
            }
        }
    }

    if snapshot.quota.is_empty() && is_effective {
        if let Ok(home_dir) = paths::home_dir() {
            if let Some(quota) = claude::local_session_quota_from_home(&home_dir, plan.as_deref()) {
                apply_quota_source(
                    snapshot,
                    "claude_local_session",
                    CLAUDE_LOCAL_SESSION_HINT,
                    quota,
                );
            }
        }
    }

    if snapshot.quota.is_empty() {
        if let Some(warning) = warning {
            snapshot.warnings.push(warning);
        }
    }
}

fn claude_plan_from_credentials(credential_bytes: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<Value>(credential_bytes).ok()?;
    value
        .pointer("/claudeAiOauth/subscriptionType")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            value
                .pointer("/claudeAiOauth/rateLimitTier")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
        .map(str::to_string)
}

fn read_saved_credentials(
    plugin_id: &str,
    alias: &str,
    config_dir: Option<PathBuf>,
) -> Result<Vec<u8>, String> {
    let paths = AswitchPaths::resolve(config_dir).map_err(|error| error.to_string())?;
    let path = paths
        .accounts_dir
        .join(plugin_id)
        .join(alias)
        .join(PRIMARY_CREDENTIALS_FILE);
    fs::read(&path).map_err(|error| format!("{}: {}", path.display(), error))
}

fn write_saved_credentials(
    plugin_id: &str,
    alias: &str,
    config_dir: Option<PathBuf>,
    bytes: &[u8],
) -> Result<(), String> {
    let paths = AswitchPaths::resolve(config_dir).map_err(|error| error.to_string())?;
    let path = paths
        .accounts_dir
        .join(plugin_id)
        .join(alias)
        .join(PRIMARY_CREDENTIALS_FILE);
    fs::write(&path, bytes).map_err(|error| format!("{}: {}", path.display(), error))
}

fn apply_quota_source(
    snapshot: &mut UsageSnapshot,
    kind: &str,
    path_or_url: &str,
    quota: serde_json::Map<String, Value>,
) {
    if quota.is_empty() {
        return;
    }

    snapshot.quota.extend(quota.clone());
    snapshot.sources.push(UsageSourceSummary {
        kind: kind.to_string(),
        path_or_url: Some(path_or_url.to_string()),
        metrics: UsageMetrics::default(),
        quota,
        warnings: Vec::new(),
    });
}

fn has_source_kind(snapshot: &UsageSnapshot, kind: &str) -> bool {
    snapshot.sources.iter().any(|source| source.kind == kind)
}

fn fetch_current_claude_cli_quota() -> Option<serde_json::Map<String, Value>> {
    let output = capture_command_output(
        "script",
        &["-q", "/dev/null", "claude", "/usage"],
        Duration::from_secs(4),
    )?;
    claude::parse_cli_quota_output(&output)
}

fn capture_command_output(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let start = Instant::now();
    loop {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    child
        .wait_with_output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn apply_code_assist_info(
    org_name: &mut Option<String>,
    plan: &mut Option<String>,
    info: &GeminiCodeAssistInfo,
) {
    if let Some(project_id) = info.project_id.as_ref() {
        *org_name = Some(project_id.clone());
    }
    if let Some(tier_name) = info.tier_name.as_ref() {
        *plan = Some(tier_name.clone());
    }
}

fn apply_claude_info(
    email: &mut Option<String>,
    org_name: &mut Option<String>,
    info: &ClaudeCodeInfo,
) {
    if email.is_none() {
        *email = info.email.clone();
    }
    if org_name.is_none() {
        *org_name = info.org_name.clone();
    }
}

fn fetch_current_claude_info() -> Option<ClaudeCodeInfo> {
    let home_dir = paths::home_dir().ok()?;
    fetch_current_claude_info_from_home(&home_dir)
}

fn fetch_current_claude_info_from_home(home_dir: &Path) -> Option<ClaudeCodeInfo> {
    let backup = latest_claude_backup_info(home_dir);
    if backup
        .as_ref()
        .map(|info| info.email.is_some() || info.org_name.is_some())
        .unwrap_or(false)
    {
        return backup;
    }

    latest_claude_telemetry_email(home_dir).map(|email| ClaudeCodeInfo {
        email: Some(email),
        org_name: None,
    })
}

fn latest_claude_backup_info(home_dir: &Path) -> Option<ClaudeCodeInfo> {
    let backup_dir = home_dir.join(".claude/backups");
    let mut paths = fs::read_dir(&backup_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with(".claude.json.backup."))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();

    paths.sort_by(|left, right| right.file_name().cmp(&left.file_name()));

    for path in paths {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&contents) else {
            continue;
        };
        let info = claude_info_from_value(&value);
        if info.email.is_some() || info.org_name.is_some() {
            return Some(info);
        }
    }

    None
}

fn claude_info_from_value(value: &Value) -> ClaudeCodeInfo {
    ClaudeCodeInfo {
        email: value
            .pointer("/oauthAccount/emailAddress")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                value
                    .get("userEmail")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
        org_name: value
            .pointer("/oauthAccount/organizationName")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn latest_claude_telemetry_email(home_dir: &Path) -> Option<String> {
    let telemetry_dir = home_dir.join(".claude/telemetry");
    let paths = fs::read_dir(&telemetry_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();

    let mut latest = None::<(String, String)>;

    for path in paths {
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };

        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Some((timestamp, email)) = claude_telemetry_email_from_line(&line) else {
                continue;
            };

            if latest
                .as_ref()
                .map(|(current, _)| timestamp > *current)
                .unwrap_or(true)
            {
                latest = Some((timestamp, email));
            }
        }
    }

    latest.map(|(_, email)| email)
}

fn claude_telemetry_email_from_line(line: &str) -> Option<(String, String)> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let event = value.get("event_data")?;
    let email = event.get("email")?.as_str()?.to_string();
    let timestamp = event
        .get("client_timestamp")
        .and_then(Value::as_str)
        .or_else(|| value.get("timestamp").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();

    Some((timestamp, email))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{claude_info_from_value, claude_telemetry_email_from_line};

    #[test]
    fn claude_backup_payload_extracts_email_and_org() {
        let value = json!({
            "oauthAccount": {
                "emailAddress": "claude@example.com",
                "organizationName": "Claude Org"
            }
        });

        let info = claude_info_from_value(&value);
        assert_eq!(info.email.as_deref(), Some("claude@example.com"));
        assert_eq!(info.org_name.as_deref(), Some("Claude Org"));
    }

    #[test]
    fn claude_telemetry_line_extracts_email() {
        let line = r#"{"event_data":{"client_timestamp":"2026-04-24T00:10:17.557Z","email":"claude@example.com"}}"#;

        let parsed = claude_telemetry_email_from_line(line);
        assert_eq!(
            parsed,
            Some((
                "2026-04-24T00:10:17.557Z".to_string(),
                "claude@example.com".to_string()
            ))
        );
    }
}
