use std::collections::{hash_map::DefaultHasher, BTreeMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, TimeZone, Utc};
use glob::glob;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::identity::{decode_jwt_payload, get_json_path, Identity};
use crate::paths::{self, AswitchPaths, PathsError};
use crate::plugin::{self, load_manifest, UsageFormat, UsageSource, UsageSourceKind};
use crate::registry::{AccountMetadata, Registry, RegistryError};
use crate::store::{self, CredentialStore, StoreError, StoreResolveError};

const DEFAULT_CACHE_TTL_S: u64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageWindow {
    Today,
    Last24h,
    Last7d,
    CurrentMonth,
    Last30d,
    All,
}

impl UsageWindow {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "today" => Some(Self::Today),
            "last_24h" => Some(Self::Last24h),
            "last_7d" => Some(Self::Last7d),
            "current_month" => Some(Self::CurrentMonth),
            "last_30d" => Some(Self::Last30d),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Last24h => "last_24h",
            Self::Last7d => "last_7d",
            Self::CurrentMonth => "current_month",
            Self::Last30d => "last_30d",
            Self::All => "all",
        }
    }

    fn bounds(self, now: DateTime<Utc>) -> Result<WindowBounds, UsageError> {
        match self {
            Self::Today => {
                let naive = now.date_naive();
                let from = naive
                    .and_hms_opt(0, 0, 0)
                    .ok_or(UsageError::InvalidWindow(self.as_str().to_string()))?;
                let from = Utc.from_utc_datetime(&from);
                Ok(WindowBounds {
                    from: Some(from),
                    to: Some(from + ChronoDuration::days(1)),
                })
            }
            Self::Last24h => Ok(WindowBounds {
                from: Some(now - ChronoDuration::hours(24)),
                to: Some(now),
            }),
            Self::Last7d => Ok(WindowBounds {
                from: Some(now - ChronoDuration::days(7)),
                to: Some(now),
            }),
            Self::CurrentMonth => {
                let from = Utc
                    .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
                    .single()
                    .ok_or(UsageError::InvalidWindow(self.as_str().to_string()))?;
                let (year, month) = if now.month() == 12 {
                    (now.year() + 1, 1)
                } else {
                    (now.year(), now.month() + 1)
                };
                let to = Utc
                    .with_ymd_and_hms(year, month, 1, 0, 0, 0)
                    .single()
                    .ok_or(UsageError::InvalidWindow(self.as_str().to_string()))?;
                Ok(WindowBounds {
                    from: Some(from),
                    to: Some(to),
                })
            }
            Self::Last30d => Ok(WindowBounds {
                from: Some(now - ChronoDuration::days(30)),
                to: Some(now),
            }),
            Self::All => Ok(WindowBounds {
                from: None,
                to: None,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageSelection {
    Local,
    Api,
    Both,
}

impl UsageSelection {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "local" => Some(Self::Local),
            "api" => Some(Self::Api),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Api => "api",
            Self::Both => "both",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, f64>,
}

impl UsageMetrics {
    fn add_metric(&mut self, name: &str, value: f64) {
        match name {
            "requests" => add_option(&mut self.requests, value),
            "tokens_in" => add_option(&mut self.tokens_in, value),
            "tokens_out" => add_option(&mut self.tokens_out, value),
            "cost_usd" => add_option(&mut self.cost_usd, value),
            other => {
                *self.extra.entry(other.to_string()).or_insert(0.0) += value;
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.requests.is_none()
            && self.tokens_in.is_none()
            && self.tokens_out.is_none()
            && self.cost_usd.is_none()
            && self.extra.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageSourceSummary {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_or_url: Option<String>,
    #[serde(default, skip_serializing_if = "UsageMetrics::is_empty")]
    pub metrics: UsageMetrics,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub quota: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub plugin_id: String,
    pub alias: String,
    pub window: UsageWindow,
    pub source: UsageSelection,
    #[serde(default, skip_serializing_if = "UsageMetrics::is_empty")]
    pub metrics: UsageMetrics,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_metrics: Option<UsageMetrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_metrics: Option<UsageMetrics>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub quota: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<UsageSourceSummary>,
    #[serde(default)]
    pub cached: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CollectUsageOptions {
    pub window: Option<UsageWindow>,
    pub source: Option<UsageSelection>,
    pub refresh: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UsageCacheEntry {
    saved_at: DateTime<Utc>,
    ttl_s: u64,
    snapshot: UsageSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UsageCacheStatus {
    pub path: PathBuf,
    pub saved_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ttl_s: u64,
    pub fresh: bool,
}

#[derive(Clone, Copy, Debug)]
struct WindowBounds {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

impl WindowBounds {
    fn contains(&self, timestamp: DateTime<Utc>) -> bool {
        if let Some(from) = self.from {
            if timestamp < from {
                return false;
            }
        }
        if let Some(to) = self.to {
            if timestamp >= to {
                return false;
            }
        }
        true
    }
}

#[derive(Debug, Error)]
pub enum UsageError {
    #[error("failed to resolve aswitch paths")]
    Paths(#[from] PathsError),
    #[error("failed to load or save registry")]
    Registry(#[from] RegistryError),
    #[error("failed to load plugin manifest")]
    Manifest(#[from] plugin::ManifestLoadError),
    #[error("failed to resolve credential store")]
    StoreResolve(#[from] StoreResolveError),
    #[error("credential store operation failed")]
    Store(#[from] StoreError),
    #[error("account {plugin_id}/{alias} does not exist")]
    AccountNotFound { plugin_id: String, alias: String },
    #[error("plugin {plugin_id} manifest not found at {path}")]
    PluginNotFound { plugin_id: String, path: PathBuf },
    #[error("invalid usage window {0}")]
    InvalidWindow(String),
    #[error("invalid glob pattern {pattern}: {message}")]
    InvalidGlobPattern { pattern: String, message: String },
    #[error("failed to read usage input {path}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse usage json from {path}: {message}")]
    ParseJson { path: PathBuf, message: String },
    #[error("http request failed for {url}: {message}")]
    HttpRequest { url: String, message: String },
    #[error("failed to write cache file {path}")]
    WriteCache {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create cache directory {path}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove cache path {path}")]
    RemovePath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn collect_usage(
    plugin_id: &str,
    alias: &str,
    options: CollectUsageOptions,
) -> Result<UsageSnapshot, UsageError> {
    collect_usage_with_config_dir(plugin_id, alias, None, options)
}

pub fn collect_usage_with_config_dir(
    plugin_id: &str,
    alias: &str,
    config_dir: Option<PathBuf>,
    options: CollectUsageOptions,
) -> Result<UsageSnapshot, UsageError> {
    let paths = AswitchPaths::resolve(config_dir)?;
    let registry = Registry::load(&paths)?;
    let metadata = registry
        .accounts
        .get(plugin_id)
        .and_then(|items| items.get(alias))
        .cloned()
        .ok_or_else(|| UsageError::AccountNotFound {
            plugin_id: plugin_id.to_string(),
            alias: alias.to_string(),
        })?;
    let manifest = load_plugin_manifest(&paths, plugin_id)?;
    let selection = determine_selection(&manifest.manifest.usage_source, options.source);
    let window = determine_window(&manifest.manifest.usage_source, options.window)?;
    let bounds = window.bounds(Utc::now())?;
    let selected_sources = select_sources(&manifest.manifest.usage_source, selection);
    let ttl_s = selected_sources
        .iter()
        .filter_map(|source| source.cache_ttl_s)
        .min()
        .unwrap_or(DEFAULT_CACHE_TTL_S);
    let cache_path = cache_path(
        &paths,
        plugin_id,
        alias,
        window,
        selection,
        &selected_sources,
    )?;

    if !options.refresh {
        if let Some(mut snapshot) = read_cache_if_fresh(&cache_path, ttl_s)? {
            snapshot.cached = true;
            return Ok(snapshot);
        }
    }

    let active_alias = registry.active.get(plugin_id).cloned().flatten();
    let is_active = active_alias.as_deref() == Some(alias);
    let identity = identity_from_metadata(&metadata);
    let mut warnings = Vec::new();
    let mut local_metrics = UsageMetrics::default();
    let mut api_metrics = UsageMetrics::default();
    let mut local_any = false;
    let mut api_any = false;
    let mut quota = Map::new();
    let mut sources = Vec::new();

    for source in selected_sources {
        match source.kind {
            UsageSourceKind::LocalLog => {
                let summary = collect_local_source(&source, &bounds)?;
                if !summary.metrics.is_empty() {
                    merge_metrics(&mut local_metrics, &summary.metrics);
                    local_any = true;
                }
                sources.push(summary);
            }
            UsageSourceKind::ProviderApi => {
                if !is_active {
                    warnings.push(format!(
                        "skipped provider_api for inactive account {plugin_id}/{alias}"
                    ));
                    continue;
                }

                let credential_bytes =
                    read_active_credentials(&manifest.manifest.credential_store)?;
                let summary = collect_api_source(&source, &bounds, &identity, &credential_bytes)?;
                if !summary.metrics.is_empty() {
                    merge_metrics(&mut api_metrics, &summary.metrics);
                    api_any = true;
                }
                quota.extend(summary.quota.clone());
                sources.push(summary);
            }
        }
    }

    let metrics = if local_any {
        local_metrics.clone()
    } else {
        api_metrics.clone()
    };

    if sources.is_empty() {
        warnings.push(format!(
            "plugin {plugin_id} has no usable usage sources for {}",
            selection.as_str()
        ));
    }

    let snapshot = UsageSnapshot {
        plugin_id: plugin_id.to_string(),
        alias: alias.to_string(),
        window,
        source: selection,
        metrics,
        local_metrics: local_any.then_some(local_metrics),
        api_metrics: api_any.then_some(api_metrics),
        quota,
        warnings,
        sources,
        cached: false,
    };

    write_cache(&cache_path, ttl_s, &snapshot)?;
    Ok(snapshot)
}

pub fn clear_cache(plugin_filter: Option<&str>) -> Result<usize, UsageError> {
    clear_cache_with_config_dir(None, plugin_filter)
}

pub fn clear_cache_with_config_dir(
    config_dir: Option<PathBuf>,
    plugin_filter: Option<&str>,
) -> Result<usize, UsageError> {
    let paths = AswitchPaths::resolve(config_dir)?;
    if !paths.usage_cache_dir.exists() {
        return Ok(0);
    }

    let mut removed = 0;
    let entries = fs::read_dir(&paths.usage_cache_dir).map_err(|source| UsageError::ReadFile {
        path: paths.usage_cache_dir.clone(),
        source,
    })?;

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if let Some(filter) = plugin_filter {
            if entry.file_name() != filter {
                continue;
            }
        }

        if path.is_dir() {
            fs::remove_dir_all(&path).map_err(|source| UsageError::RemovePath {
                path: path.clone(),
                source,
            })?;
            removed += 1;
        } else if path.exists() {
            fs::remove_file(&path).map_err(|source| UsageError::RemovePath {
                path: path.clone(),
                source,
            })?;
            removed += 1;
        }
    }

    Ok(removed)
}

pub fn inspect_cache(
    plugin_id: &str,
    alias: &str,
    options: CollectUsageOptions,
) -> Result<Option<UsageCacheStatus>, UsageError> {
    inspect_cache_with_config_dir(plugin_id, alias, None, options)
}

pub fn inspect_cache_with_config_dir(
    plugin_id: &str,
    alias: &str,
    config_dir: Option<PathBuf>,
    options: CollectUsageOptions,
) -> Result<Option<UsageCacheStatus>, UsageError> {
    let paths = AswitchPaths::resolve(config_dir)?;
    let manifest = load_plugin_manifest(&paths, plugin_id)?;
    let selection = determine_selection(&manifest.manifest.usage_source, options.source);
    let window = determine_window(&manifest.manifest.usage_source, options.window)?;
    let selected_sources = select_sources(&manifest.manifest.usage_source, selection);
    let ttl_s = selected_sources
        .iter()
        .filter_map(|source| source.cache_ttl_s)
        .min()
        .unwrap_or(DEFAULT_CACHE_TTL_S);
    let cache_path = cache_path(
        &paths,
        plugin_id,
        alias,
        window,
        selection,
        &selected_sources,
    )?;

    read_cache_status(&cache_path, ttl_s)
}

fn load_plugin_manifest(
    paths: &AswitchPaths,
    plugin_id: &str,
) -> Result<plugin::LoadedManifest, UsageError> {
    let path = paths.plugins_dir.join(plugin_id).join("plugin.toml");
    if !path.is_file() {
        return Err(UsageError::PluginNotFound {
            plugin_id: plugin_id.to_string(),
            path,
        });
    }
    Ok(load_manifest(&path)?)
}

fn determine_selection(
    sources: &[UsageSource],
    requested: Option<UsageSelection>,
) -> UsageSelection {
    if let Some(requested) = requested {
        return requested;
    }

    if sources
        .iter()
        .any(|source| source.kind == UsageSourceKind::LocalLog)
    {
        UsageSelection::Local
    } else if sources
        .iter()
        .any(|source| source.kind == UsageSourceKind::ProviderApi)
    {
        UsageSelection::Api
    } else {
        UsageSelection::Local
    }
}

fn determine_window(
    sources: &[UsageSource],
    requested: Option<UsageWindow>,
) -> Result<UsageWindow, UsageError> {
    if let Some(requested) = requested {
        return Ok(requested);
    }

    let default = sources
        .iter()
        .find_map(|source| source.default_window.as_deref())
        .unwrap_or("current_month");
    UsageWindow::parse(default).ok_or_else(|| UsageError::InvalidWindow(default.to_string()))
}

fn select_sources(sources: &[UsageSource], selection: UsageSelection) -> Vec<UsageSource> {
    sources
        .iter()
        .filter(|source| match selection {
            UsageSelection::Local => source.kind == UsageSourceKind::LocalLog,
            UsageSelection::Api => source.kind == UsageSourceKind::ProviderApi,
            UsageSelection::Both => true,
        })
        .cloned()
        .collect()
}

fn collect_local_source(
    source: &UsageSource,
    bounds: &WindowBounds,
) -> Result<UsageSourceSummary, UsageError> {
    let raw_path = source.path.clone().unwrap_or_default();
    let expanded = paths::expand_user_path(&raw_path)?;
    let pattern = expanded.to_string_lossy().to_string();
    let mut metrics = UsageMetrics::default();
    let mut warnings = Vec::new();

    let iter = glob(&pattern).map_err(|error| UsageError::InvalidGlobPattern {
        pattern: pattern.clone(),
        message: error.to_string(),
    })?;

    for entry in iter {
        let path = match entry {
            Ok(path) => path,
            Err(error) => {
                warnings.push(error.to_string());
                continue;
            }
        };

        match source.format.unwrap_or(UsageFormat::Jsonl) {
            UsageFormat::Jsonl => collect_jsonl_file(source, bounds, &path, &mut metrics)?,
            UsageFormat::Json => collect_json_file(source, bounds, &path, &mut metrics)?,
            UsageFormat::RegexLines => {
                warnings.push(format!(
                    "regex_lines is not yet supported for {}",
                    path.display()
                ));
            }
        }
    }

    Ok(UsageSourceSummary {
        kind: "local_log".to_string(),
        path_or_url: Some(raw_path),
        metrics,
        quota: Map::new(),
        warnings,
    })
}

fn collect_jsonl_file(
    source: &UsageSource,
    bounds: &WindowBounds,
    path: &Path,
    metrics: &mut UsageMetrics,
) -> Result<(), UsageError> {
    let file = fs::File::open(path).map_err(|source| UsageError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line.map_err(|source| UsageError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let record =
            serde_json::from_str::<Value>(&line).map_err(|error| UsageError::ParseJson {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        collect_record(source, bounds, &record, metrics);
    }

    Ok(())
}

fn collect_json_file(
    source: &UsageSource,
    bounds: &WindowBounds,
    path: &Path,
    metrics: &mut UsageMetrics,
) -> Result<(), UsageError> {
    let contents = fs::read_to_string(path).map_err(|source| UsageError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    let value =
        serde_json::from_str::<Value>(&contents).map_err(|error| UsageError::ParseJson {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;

    match value {
        Value::Array(items) => {
            for record in items {
                collect_record(source, bounds, &record, metrics);
            }
        }
        record => collect_record(source, bounds, &record, metrics),
    }

    Ok(())
}

fn collect_record(
    source: &UsageSource,
    bounds: &WindowBounds,
    record: &Value,
    metrics: &mut UsageMetrics,
) {
    let Some(timestamp_pointer) = source.timestamp_pointer.as_deref() else {
        return;
    };
    let Some(timestamp_value) = get_json_path(record, timestamp_pointer) else {
        return;
    };
    let Some(timestamp) = parse_timestamp(&timestamp_value) else {
        return;
    };
    if !bounds.contains(timestamp) {
        return;
    }
    if !matches_record_filter(source.record_filter.as_deref(), record) {
        return;
    }

    for (name, expr) in &source.metric_map {
        if expr == "$count" {
            metrics.add_metric(name, 1.0);
        } else if let Some(filter) = expr.strip_prefix("$count_if:") {
            if matches_record_filter(Some(filter), record) {
                metrics.add_metric(name, 1.0);
            }
        } else if let Some(value) = get_json_path(record, expr).and_then(value_to_f64) {
            metrics.add_metric(name, value);
        }
    }
}

fn collect_api_source(
    source: &UsageSource,
    bounds: &WindowBounds,
    identity: &Identity,
    credential_bytes: &[u8],
) -> Result<UsageSourceSummary, UsageError> {
    let credential_json = serde_json::from_slice::<Value>(credential_bytes).ok();
    let url_template = source.url.clone().unwrap_or_default();
    let url = expand_template(
        &url_template,
        credential_bytes,
        credential_json.as_ref(),
        identity,
        bounds,
    );

    let mut request = ureq::get(&url).timeout(Duration::from_secs(10));
    for (name, value) in &source.headers {
        let expanded = expand_template(
            value,
            credential_bytes,
            credential_json.as_ref(),
            identity,
            bounds,
        );
        request = request.set(name, &expanded);
    }

    let response = request.call().map_err(|error| UsageError::HttpRequest {
        url: url.clone(),
        message: error.to_string(),
    })?;
    let body = response
        .into_string()
        .map_err(|error| UsageError::HttpRequest {
            url: url.clone(),
            message: error.to_string(),
        })?;
    let value = serde_json::from_str::<Value>(&body).map_err(|error| UsageError::HttpRequest {
        url: url.clone(),
        message: error.to_string(),
    })?;

    let mut metrics = UsageMetrics::default();
    for (name, pointer) in &source.response_metric_pointer {
        if let Some(value) = get_json_path(&value, pointer).and_then(value_to_f64) {
            metrics.add_metric(name, value);
        }
    }

    let mut quota = Map::new();
    for (name, pointer) in &source.response_quota_pointer {
        if let Some(value) = get_json_path(&value, pointer) {
            quota.insert(name.to_string(), value);
        }
    }

    Ok(UsageSourceSummary {
        kind: "provider_api".to_string(),
        path_or_url: Some(url),
        metrics,
        quota,
        warnings: Vec::new(),
    })
}

fn expand_template(
    template: &str,
    credential_bytes: &[u8],
    credential_json: Option<&Value>,
    identity: &Identity,
    bounds: &WindowBounds,
) -> String {
    let mut result = String::new();
    let mut rest = template;

    while let Some(start) = rest.find("${") {
        result.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let Some(end) = tail.find('}') else {
            result.push_str(&rest[start..]);
            return result;
        };
        let key = &tail[..end];
        result.push_str(&resolve_template_value(
            key,
            credential_bytes,
            credential_json,
            identity,
            bounds,
        ));
        rest = &tail[end + 1..];
    }

    result.push_str(rest);
    result
}

fn resolve_template_value(
    key: &str,
    credential_bytes: &[u8],
    credential_json: Option<&Value>,
    identity: &Identity,
    bounds: &WindowBounds,
) -> String {
    if let Some(pointer) = key.strip_prefix("cred:") {
        return credential_json
            .and_then(|value| get_json_path(value, pointer))
            .and_then(|value| value_to_string(&value))
            .unwrap_or_default();
    }

    if let Some(claim) = key.strip_prefix("cred_jwt:") {
        let jwt = extract_raw_credential_string(credential_bytes, credential_json);
        return jwt
            .and_then(|jwt| decode_jwt_payload(&jwt).ok())
            .and_then(|value| get_json_path(&value, claim))
            .and_then(|value| value_to_string(&value))
            .unwrap_or_default();
    }

    if key == "window:from" {
        return bounds
            .from
            .map(|time| time.to_rfc3339())
            .unwrap_or_default();
    }
    if key == "window:to" {
        return bounds.to.map(|time| time.to_rfc3339()).unwrap_or_default();
    }
    if key == "window:from_date" {
        return bounds
            .from
            .map(|time| time.date_naive().to_string())
            .unwrap_or_default();
    }
    if key == "window:to_date" {
        return bounds
            .to
            .map(|time| time.date_naive().to_string())
            .unwrap_or_default();
    }
    if let Some(field) = key.strip_prefix("account:") {
        return match field {
            "email" => identity.email.clone().unwrap_or_default(),
            "org_name" => identity.org_name.clone().unwrap_or_default(),
            "plan" => identity.plan.clone().unwrap_or_default(),
            other => identity
                .extra
                .get(other)
                .and_then(value_to_string)
                .unwrap_or_default(),
        };
    }

    String::new()
}

fn extract_raw_credential_string(
    credential_bytes: &[u8],
    credential_json: Option<&Value>,
) -> Option<String> {
    if let Some(Value::String(value)) = credential_json {
        return Some(value.clone());
    }

    String::from_utf8(credential_bytes.to_vec())
        .ok()
        .map(|value| value.trim().trim_matches('"').to_string())
}

fn cache_path(
    paths: &AswitchPaths,
    plugin_id: &str,
    alias: &str,
    window: UsageWindow,
    selection: UsageSelection,
    sources: &[UsageSource],
) -> Result<PathBuf, UsageError> {
    let plugin_dir = paths.usage_cache_dir.join(plugin_id);
    fs::create_dir_all(&plugin_dir).map_err(|source| UsageError::CreateDirectory {
        path: plugin_dir.clone(),
        source,
    })?;

    let serialized = serde_json::to_string(sources).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    let digest = hasher.finish();
    Ok(plugin_dir.join(format!(
        "{alias}.{}.{}.{}.json",
        window.as_str(),
        selection.as_str(),
        digest
    )))
}

fn read_cache_if_fresh(path: &Path, ttl_s: u64) -> Result<Option<UsageSnapshot>, UsageError> {
    if !path.is_file() {
        return Ok(None);
    }

    let contents = fs::read_to_string(path).map_err(|source| UsageError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    let entry = serde_json::from_str::<UsageCacheEntry>(&contents).map_err(|error| {
        UsageError::ParseJson {
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    })?;
    if (Utc::now() - entry.saved_at).num_seconds() >= ttl_s as i64 {
        return Ok(None);
    }
    Ok(Some(entry.snapshot))
}

fn read_cache_status(path: &Path, ttl_s: u64) -> Result<Option<UsageCacheStatus>, UsageError> {
    if !path.is_file() {
        return Ok(None);
    }

    let contents = fs::read_to_string(path).map_err(|source| UsageError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    let entry = serde_json::from_str::<UsageCacheEntry>(&contents).map_err(|error| {
        UsageError::ParseJson {
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    })?;
    let ttl_s = entry.ttl_s.max(ttl_s);
    let expires_at = entry.saved_at + ChronoDuration::seconds(ttl_s as i64);

    Ok(Some(UsageCacheStatus {
        path: path.to_path_buf(),
        saved_at: entry.saved_at,
        expires_at,
        ttl_s,
        fresh: Utc::now() < expires_at,
    }))
}

fn write_cache(path: &Path, ttl_s: u64, snapshot: &UsageSnapshot) -> Result<(), UsageError> {
    let entry = UsageCacheEntry {
        saved_at: Utc::now(),
        ttl_s,
        snapshot: snapshot.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&entry).map_err(|error| UsageError::ParseJson {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let mut file = fs::File::create(path).map_err(|source| UsageError::WriteCache {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(&bytes)
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|source| UsageError::WriteCache {
            path: path.to_path_buf(),
            source,
        })
}

fn read_active_credentials(
    credential_store: &plugin::CredentialStore,
) -> Result<Vec<u8>, UsageError> {
    let store = store::resolve_active_store(credential_store)?;
    if !store.exists()? && store.allows_missing_active() {
        return Ok(Vec::new());
    }
    Ok(store.read_active()?)
}

fn identity_from_metadata(metadata: &AccountMetadata) -> Identity {
    Identity {
        email: metadata.email.clone(),
        org_name: metadata.org_name.clone(),
        plan: metadata.plan.clone(),
        extra: Map::new(),
    }
}

fn merge_metrics(target: &mut UsageMetrics, source: &UsageMetrics) {
    if let Some(value) = source.requests {
        target.add_metric("requests", value);
    }
    if let Some(value) = source.tokens_in {
        target.add_metric("tokens_in", value);
    }
    if let Some(value) = source.tokens_out {
        target.add_metric("tokens_out", value);
    }
    if let Some(value) = source.cost_usd {
        target.add_metric("cost_usd", value);
    }
    for (name, value) in &source.extra {
        target.add_metric(name, *value);
    }
}

fn add_option(target: &mut Option<f64>, value: f64) {
    match target {
        Some(current) => *current += value,
        None => *target = Some(value),
    }
}

fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .map(|value| value.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                text.parse::<i64>()
                    .ok()
                    .and_then(|epoch| Utc.timestamp_opt(epoch, 0).single())
            }),
        Value::Number(number) => number
            .as_i64()
            .and_then(|epoch| Utc.timestamp_opt(epoch, 0).single())
            .or_else(|| {
                number
                    .as_f64()
                    .and_then(|epoch| Utc.timestamp_opt(epoch as i64, 0).single())
            }),
        _ => None,
    }
}

fn matches_record_filter(filter: Option<&str>, record: &Value) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    eval_or_expression(filter, record)
}

fn eval_or_expression(expr: &str, record: &Value) -> bool {
    split_expression(expr, "||")
        .into_iter()
        .any(|part| eval_and_expression(part, record))
}

fn eval_and_expression(expr: &str, record: &Value) -> bool {
    split_expression(expr, "&&")
        .into_iter()
        .all(|part| eval_condition(part, record))
}

fn eval_condition(expr: &str, record: &Value) -> bool {
    let expr = expr.trim();
    if expr.is_empty() {
        return true;
    }

    if let Some((left, right)) = split_condition(expr, "==") {
        return compare_condition(left, right, true, record);
    }
    if let Some((left, right)) = split_condition(expr, "!=") {
        return compare_condition(left, right, false, record);
    }

    get_json_path(record, expr)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn split_expression<'a>(expr: &'a str, token: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let bytes = expr.as_bytes();
    let token_bytes = token.as_bytes();
    let mut quote = None;

    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' => {
                if quote == Some(bytes[index]) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(bytes[index]);
                }
                index += 1;
            }
            _ if quote.is_none()
                && index + token_bytes.len() <= bytes.len()
                && &bytes[index..index + token_bytes.len()] == token_bytes =>
            {
                parts.push(expr[start..index].trim());
                index += token_bytes.len();
                start = index;
            }
            _ => index += 1,
        }
    }

    parts.push(expr[start..].trim());
    parts
}

fn split_condition<'a>(expr: &'a str, token: &str) -> Option<(&'a str, &'a str)> {
    let bytes = expr.as_bytes();
    let token_bytes = token.as_bytes();
    let mut index = 0;
    let mut quote = None;

    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' => {
                if quote == Some(bytes[index]) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(bytes[index]);
                }
                index += 1;
            }
            _ if quote.is_none()
                && index + token_bytes.len() <= bytes.len()
                && &bytes[index..index + token_bytes.len()] == token_bytes =>
            {
                return Some((
                    expr[..index].trim(),
                    expr[index + token_bytes.len()..].trim(),
                ));
            }
            _ => index += 1,
        }
    }

    None
}

fn compare_condition(left: &str, right: &str, equal: bool, record: &Value) -> bool {
    let left = get_json_path(record, left).unwrap_or(Value::Null);
    let right = parse_literal(right);
    if equal {
        values_equal(&left, &right)
    } else {
        !values_equal(&left, &right)
    }
}

fn parse_literal(value: &str) -> Value {
    let trimmed = value.trim();
    if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        || (trimmed.starts_with('"') && trimmed.ends_with('"'))
    {
        return Value::String(trimmed[1..trimmed.len() - 1].to_string());
    }
    if trimmed == "true" {
        return Value::Bool(true);
    }
    if trimmed == "false" {
        return Value::Bool(false);
    }
    if trimmed == "null" {
        return Value::Null;
    }
    if let Ok(number) = trimmed.parse::<i64>() {
        return Value::Number(number.into());
    }
    if let Ok(number) = trimmed.parse::<f64>() {
        return serde_json::Number::from_f64(number)
            .map(Value::Number)
            .unwrap_or(Value::Null);
    }
    Value::String(trimmed.to_string())
}

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Null, Value::Null) => true,
        _ => left == right,
    }
}

fn value_to_f64(value: Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::thread;

    use chrono::{TimeZone, Utc};
    use serde_json::Map;

    use super::{
        clear_cache_with_config_dir, collect_usage_with_config_dir, expand_template,
        matches_record_filter, CollectUsageOptions, UsageSelection, WindowBounds,
    };
    use crate::identity::Identity;
    use crate::paths::AswitchPaths;

    fn write_manifest(
        config_dir: &Path,
        auth_path: &Path,
        sessions_glob: &str,
        api_url: Option<&str>,
    ) {
        let plugin_dir = config_dir.join("plugins/demo");
        fs::create_dir_all(&plugin_dir).expect("plugin dir");
        let mut manifest = format!(
            r#"
id = "demo"
display_name = "Demo"
version = "1.0.0"
author = "tests"
description = "usage tests"
platforms = ["macos", "linux"]

[credential_store]
kind = "file"
path = "{}"
permissions = 384

[login]
cmd = ["demo", "login"]
ready_marker_kind = "file"
ready_marker_timeout_s = 1
ready_marker_path = "{}"

[[usage_source]]
kind = "local_log"
path = "{}"
format = "jsonl"
timestamp_pointer = "timestamp"
record_filter = "type == 'assistant'"
metric_map = {{ requests = "$count", tokens_in = "usage.prompt_tokens", tokens_out = "usage.completion_tokens" }}
default_window = "current_month"
"#,
            auth_path.display(),
            auth_path.display(),
            sessions_glob
        );
        if let Some(api_url) = api_url {
            manifest.push_str(&format!(
                r#"

[[usage_source]]
kind = "provider_api"
method = "GET"
url = "{api_url}?email=${{account:email}}&from=${{window:from_date}}"
response_metric_pointer = {{ requests = "data.request_count" }}
response_quota_pointer = {{ limit_tokens = "data.limit" }}
"#
            ));
        }
        fs::write(plugin_dir.join("plugin.toml"), manifest).expect("manifest");
    }

    fn seed_registry(paths: &AswitchPaths) {
        let registry = serde_json::json!({
            "version": 1,
            "active": { "demo": "work" },
            "accounts": {
                "demo": {
                    "work": {
                        "alias": "work",
                        "email": "amos@example.com",
                        "org_name": null,
                        "plan": "plus",
                        "added_at": "2026-04-24T00:00:00Z",
                        "last_used_at": "2026-04-24T00:00:00Z"
                    }
                }
            }
        });
        fs::write(
            &paths.registry_file,
            serde_json::to_vec_pretty(&registry).expect("serialize registry"),
        )
        .expect("write registry");
    }

    #[test]
    fn usage_collects_local_jsonl_and_uses_cache() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config_dir = temp_dir.path().join(".aswitch");
        let paths = AswitchPaths::resolve(Some(config_dir.clone())).expect("paths");
        paths.ensure().expect("ensure");

        let auth_path = temp_dir.path().join("auth.json");
        fs::write(&auth_path, br#"{"token":"demo"}"#).expect("auth");
        let sessions_dir = temp_dir.path().join("sessions");
        fs::create_dir_all(&sessions_dir).expect("sessions dir");
        fs::write(
            sessions_dir.join("one.jsonl"),
            format!(
                "{{\"timestamp\":\"{}\",\"type\":\"assistant\",\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":4}}}}\n{{\"timestamp\":\"{}\",\"type\":\"user\",\"usage\":{{\"prompt_tokens\":8,\"completion_tokens\":1}}}}\n",
                Utc::now().to_rfc3339(),
                Utc::now().to_rfc3339(),
            ),
        )
        .expect("sessions");

        write_manifest(
            &config_dir,
            &auth_path,
            &format!("{}/**/*.jsonl", sessions_dir.display()),
            None,
        );
        seed_registry(&paths);

        let first = collect_usage_with_config_dir(
            "demo",
            "work",
            Some(config_dir.clone()),
            CollectUsageOptions::default(),
        )
        .expect("first collect");
        assert_eq!(first.metrics.requests, Some(1.0));
        assert_eq!(first.metrics.tokens_in, Some(10.0));
        assert!(!first.cached);

        let second = collect_usage_with_config_dir(
            "demo",
            "work",
            Some(config_dir.clone()),
            CollectUsageOptions::default(),
        )
        .expect("second collect");
        assert!(second.cached);
        assert_eq!(second.metrics.requests, Some(1.0));
    }

    #[test]
    fn usage_collects_provider_api_for_active_account() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config_dir = temp_dir.path().join(".aswitch");
        let paths = AswitchPaths::resolve(Some(config_dir.clone())).expect("paths");
        paths.ensure().expect("ensure");

        let auth_path = temp_dir.path().join("auth.json");
        fs::write(&auth_path, br#"{"access_token":"demo"}"#).expect("auth");
        let sessions_dir = temp_dir.path().join("sessions");
        fs::create_dir_all(&sessions_dir).expect("sessions dir");
        fs::write(sessions_dir.join("empty.jsonl"), "").expect("sessions");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut request = String::new();
                let _ = reader.read_line(&mut request);
                assert!(request.contains("email=amos@example.com"));
                let body = "{\"data\":{\"request_count\":5,\"limit\":12345}}";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).expect("write");
            }
        });

        write_manifest(
            &config_dir,
            &auth_path,
            &format!("{}/**/*.jsonl", sessions_dir.display()),
            Some(&format!("http://{}", addr)),
        );
        seed_registry(&paths);

        let snapshot = collect_usage_with_config_dir(
            "demo",
            "work",
            Some(config_dir.clone()),
            CollectUsageOptions {
                source: Some(UsageSelection::Both),
                ..CollectUsageOptions::default()
            },
        )
        .expect("collect usage");

        assert_eq!(
            snapshot.api_metrics.and_then(|item| item.requests),
            Some(5.0)
        );
        assert_eq!(
            snapshot
                .quota
                .get("limit_tokens")
                .and_then(|value| value.as_i64()),
            Some(12345)
        );
    }

    #[test]
    fn usage_collects_codex_token_count_events() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config_dir = temp_dir.path().join(".aswitch");
        let paths = AswitchPaths::resolve(Some(config_dir.clone())).expect("paths");
        paths.ensure().expect("ensure");

        let auth_path = temp_dir.path().join("auth.json");
        fs::write(&auth_path, br#"{"token":"demo"}"#).expect("auth");
        let sessions_dir = temp_dir.path().join("sessions");
        fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let fixture = vec![
            serde_json::json!({
                "timestamp": Utc::now().to_rfc3339(),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": serde_json::Value::Null,
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": Utc::now().to_rfc3339(),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 120,
                            "cached_input_tokens": 50,
                            "output_tokens": 30,
                            "reasoning_output_tokens": 7,
                        }
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": Utc::now().to_rfc3339(),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 80,
                            "cached_input_tokens": 20,
                            "output_tokens": 10,
                            "reasoning_output_tokens": 3,
                        }
                    }
                }
            })
            .to_string(),
        ]
        .join("\n")
            + "\n";
        fs::write(sessions_dir.join("codex.jsonl"), fixture).expect("sessions");

        let plugin_dir = config_dir.join("plugins/demo");
        fs::create_dir_all(&plugin_dir).expect("plugin dir");
        fs::write(
            plugin_dir.join("plugin.toml"),
            format!(
                r#"
id = "demo"
display_name = "Demo"
version = "1.0.0"
author = "tests"
description = "usage tests"
platforms = ["macos", "linux"]

[credential_store]
kind = "file"
path = "{}"
permissions = 384

[login]
cmd = ["demo", "login"]
ready_marker_kind = "file"
ready_marker_timeout_s = 1
ready_marker_path = "{}"

[[usage_source]]
kind = "local_log"
path = "{}"
format = "jsonl"
timestamp_pointer = "timestamp"
record_filter = "type == 'event_msg' && payload.type == 'token_count' && payload.info != null"
metric_map = {{ requests = "$count", tokens_in = "payload.info.last_token_usage.input_tokens", tokens_out = "payload.info.last_token_usage.output_tokens", cached_input_tokens = "payload.info.last_token_usage.cached_input_tokens", reasoning_output_tokens = "payload.info.last_token_usage.reasoning_output_tokens" }}
default_window = "current_month"
"#,
                auth_path.display(),
                auth_path.display(),
                format!("{}/**/*.jsonl", sessions_dir.display()),
            ),
        )
        .expect("manifest");
        seed_registry(&paths);

        let snapshot = collect_usage_with_config_dir(
            "demo",
            "work",
            Some(config_dir.clone()),
            CollectUsageOptions::default(),
        )
        .expect("collect usage");

        assert_eq!(snapshot.metrics.requests, Some(2.0));
        assert_eq!(snapshot.metrics.tokens_in, Some(200.0));
        assert_eq!(snapshot.metrics.tokens_out, Some(40.0));
        assert_eq!(
            snapshot.metrics.extra.get("cached_input_tokens").copied(),
            Some(70.0)
        );
        assert_eq!(
            snapshot
                .metrics
                .extra
                .get("reasoning_output_tokens")
                .copied(),
            Some(10.0)
        );
    }

    #[test]
    fn usage_collects_gemini_chat_records_without_counting_toolcall_updates() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config_dir = temp_dir.path().join(".aswitch");
        let paths = AswitchPaths::resolve(Some(config_dir.clone())).expect("paths");
        paths.ensure().expect("ensure");

        let auth_path = temp_dir.path().join("auth.json");
        fs::write(&auth_path, br#"{"token":"demo"}"#).expect("auth");
        let sessions_dir = temp_dir.path().join("tmp/project/chats");
        fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let fixture = vec![
            serde_json::json!({
                "sessionId": "abc",
                "startTime": Utc::now().to_rfc3339(),
                "lastUpdated": Utc::now().to_rfc3339(),
                "kind": "main",
            })
            .to_string(),
            serde_json::json!({
                "id": "m1",
                "timestamp": Utc::now().to_rfc3339(),
                "type": "gemini",
                "tokens": {
                    "input": 100,
                    "output": 12,
                    "cached": 20,
                    "thoughts": 5,
                    "tool": 2,
                }
            })
            .to_string(),
            serde_json::json!({
                "id": "m1",
                "timestamp": Utc::now().to_rfc3339(),
                "type": "gemini",
                "tokens": {
                    "input": 100,
                    "output": 12,
                    "cached": 20,
                    "thoughts": 5,
                    "tool": 2,
                },
                "toolCalls": [{"id": "call1"}],
            })
            .to_string(),
            serde_json::json!({
                "id": "m2",
                "timestamp": Utc::now().to_rfc3339(),
                "type": "gemini",
                "tokens": {
                    "input": 80,
                    "output": 10,
                    "cached": 8,
                    "thoughts": 3,
                    "tool": 1,
                }
            })
            .to_string(),
        ]
        .join("\n")
            + "\n";
        fs::write(sessions_dir.join("gemini.jsonl"), fixture).expect("sessions");

        let plugin_dir = config_dir.join("plugins/demo");
        fs::create_dir_all(&plugin_dir).expect("plugin dir");
        fs::write(
            plugin_dir.join("plugin.toml"),
            format!(
                r#"
id = "demo"
display_name = "Demo"
version = "1.0.0"
author = "tests"
description = "usage tests"
platforms = ["macos", "linux"]

[credential_store]
kind = "file"
path = "{}"
permissions = 384

[login]
cmd = ["demo", "login"]
ready_marker_kind = "file"
ready_marker_timeout_s = 1
ready_marker_path = "{}"

[[usage_source]]
kind = "local_log"
path = "{}"
format = "jsonl"
timestamp_pointer = "timestamp"
record_filter = "type == 'gemini' && toolCalls == null"
metric_map = {{ requests = "$count", tokens_in = "tokens.input", tokens_out = "tokens.output", cached_input_tokens = "tokens.cached", reasoning_output_tokens = "tokens.thoughts", tool_tokens = "tokens.tool" }}
default_window = "current_month"
"#,
                auth_path.display(),
                auth_path.display(),
                format!("{}/**/*.jsonl", temp_dir.path().join("tmp").display()),
            ),
        )
        .expect("manifest");
        seed_registry(&paths);

        let snapshot = collect_usage_with_config_dir(
            "demo",
            "work",
            Some(config_dir.clone()),
            CollectUsageOptions::default(),
        )
        .expect("collect usage");

        assert_eq!(snapshot.metrics.requests, Some(2.0));
        assert_eq!(snapshot.metrics.tokens_in, Some(180.0));
        assert_eq!(snapshot.metrics.tokens_out, Some(22.0));
        assert_eq!(
            snapshot.metrics.extra.get("cached_input_tokens").copied(),
            Some(28.0)
        );
        assert_eq!(
            snapshot
                .metrics
                .extra
                .get("reasoning_output_tokens")
                .copied(),
            Some(8.0)
        );
        assert_eq!(
            snapshot.metrics.extra.get("tool_tokens").copied(),
            Some(3.0)
        );
    }

    #[test]
    fn clear_cache_removes_cached_plugin_directory() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config_dir = temp_dir.path().join(".aswitch");
        let paths = AswitchPaths::resolve(Some(config_dir.clone())).expect("paths");
        fs::create_dir_all(paths.usage_cache_dir.join("demo")).expect("cache dir");
        fs::write(paths.usage_cache_dir.join("demo/cache.json"), "{}").expect("cache file");

        let removed =
            clear_cache_with_config_dir(Some(config_dir.clone()), Some("demo")).expect("clear");
        assert_eq!(removed, 1);
        assert!(!paths.usage_cache_dir.join("demo").exists());
    }

    #[test]
    fn record_filter_supports_and_or_expressions() {
        let record = serde_json::json!({
            "type": "assistant",
            "role": "assistant",
            "usage": { "prompt_tokens": 10 }
        });
        assert!(matches_record_filter(
            Some("type == 'assistant' && role == 'assistant'"),
            &record
        ));
        assert!(matches_record_filter(
            Some("type == 'assistant' || role == 'user'"),
            &record
        ));
        assert!(!matches_record_filter(
            Some("type != 'assistant' && role == 'assistant'"),
            &record
        ));
    }

    #[test]
    fn expand_template_supports_window_cred_and_account_values() {
        let identity = Identity {
            email: Some("amos@example.com".to_string()),
            org_name: None,
            plan: Some("plus".to_string()),
            extra: Map::new(),
        };
        let credential_json = serde_json::json!({ "token": "abc" });
        let bounds = WindowBounds {
            from: Some(
                Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0)
                    .single()
                    .expect("from"),
            ),
            to: Some(
                Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
                    .single()
                    .expect("to"),
            ),
        };
        let rendered = expand_template(
            "Bearer ${cred:token} ${account:email} ${window:from_date}",
            br#"{"token":"abc"}"#,
            Some(&credential_json),
            &identity,
            &bounds,
        );
        assert_eq!(rendered, "Bearer abc amos@example.com 2026-04-01");
    }
}
