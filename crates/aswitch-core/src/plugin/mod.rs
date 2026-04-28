use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub platforms: Vec<Platform>,
    pub credential_store: CredentialStore,
    #[serde(default)]
    pub session_activation: Option<SessionActivationConfig>,
    #[serde(default)]
    pub aux_files: Vec<AuxFile>,
    #[serde(default)]
    pub identity_extract: Vec<IdentityExtract>,
    pub login: LoginConfig,
    #[serde(default)]
    pub usage_source: Vec<UsageSource>,
}

impl Manifest {
    pub fn validate(&self) -> Result<(), ManifestValidationError> {
        require_non_empty("id", &self.id)?;
        require_non_empty("display_name", &self.display_name)?;
        require_non_empty("version", &self.version)?;
        require_non_empty("author", &self.author)?;
        require_non_empty("description", &self.description)?;

        if self.platforms.is_empty() {
            return Err(ManifestValidationError(
                "platforms must declare at least one supported platform".into(),
            ));
        }

        self.credential_store.validate(&self.platforms)?;
        if let Some(config) = &self.session_activation {
            config.validate()?;
        }
        self.login.validate()?;

        for (index, aux_file) in self.aux_files.iter().enumerate() {
            aux_file.validate(index)?;
        }

        for (index, identity_rule) in self.identity_extract.iter().enumerate() {
            identity_rule.validate(index)?;
        }

        for (index, usage_source) in self.usage_source.iter().enumerate() {
            usage_source.validate(index)?;
        }

        Ok(())
    }

    pub fn supports_current_platform(&self) -> bool {
        match Platform::current() {
            Some(platform) => self.platforms.contains(&platform),
            None => false,
        }
    }

    fn expand_env_placeholders_with<F>(&mut self, env_lookup: &F) -> Vec<String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut warnings = Vec::new();

        self.credential_store
            .expand_env_placeholders(env_lookup, &mut warnings);
        if let Some(config) = &mut self.session_activation {
            config.expand_env_placeholders(env_lookup, &mut warnings);
        }
        self.login
            .expand_env_placeholders(env_lookup, &mut warnings);

        for aux_file in &mut self.aux_files {
            aux_file.expand_env_placeholders(env_lookup, &mut warnings);
        }

        for identity_rule in &mut self.identity_extract {
            identity_rule.expand_env_placeholders(env_lookup, &mut warnings);
        }

        for usage_source in &mut self.usage_source {
            usage_source.expand_env_placeholders(env_lookup, &mut warnings);
        }

        warnings
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Macos,
    Linux,
    Windows,
}

impl Platform {
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Macos)
        } else if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else if cfg!(target_os = "windows") {
            Some(Self::Windows)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialStore {
    pub kind: CredentialStoreKind,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub permissions: Option<u32>,
    #[serde(default)]
    pub macos_service: Option<String>,
    #[serde(default)]
    pub macos_account: Option<String>,
    #[serde(default)]
    pub linux_schema: Option<String>,
    #[serde(default)]
    pub linux_attributes: BTreeMap<String, String>,
    #[serde(default)]
    pub linux_fallback_kind: Option<CredentialStoreKind>,
    #[serde(default)]
    pub linux_fallback_path: Option<String>,
    #[serde(default)]
    pub linux_fallback_permissions: Option<u32>,
    #[serde(default)]
    pub allow_empty_active: bool,
}

impl CredentialStore {
    fn validate(&self, platforms: &[Platform]) -> Result<(), ManifestValidationError> {
        match self.kind {
            CredentialStoreKind::File => {
                require_option("credential_store.path", self.path.as_ref())?;
                require_option("credential_store.permissions", self.permissions.as_ref())?;
            }
            CredentialStoreKind::Keychain => {
                if platforms.contains(&Platform::Macos) {
                    require_option(
                        "credential_store.macos_service",
                        self.macos_service.as_ref(),
                    )?;
                    require_option(
                        "credential_store.macos_account",
                        self.macos_account.as_ref(),
                    )?;
                }

                if platforms.contains(&Platform::Linux) {
                    let has_secret_service =
                        self.linux_schema.is_some() && !self.linux_attributes.is_empty();
                    let has_file_fallback = self.linux_fallback_kind
                        == Some(CredentialStoreKind::File)
                        && self.linux_fallback_path.is_some()
                        && self.linux_fallback_permissions.is_some();

                    if !has_secret_service && !has_file_fallback {
                        return Err(ManifestValidationError(
                            "credential_store on linux requires secret-service fields or a file fallback".into(),
                        ));
                    }
                }

                if self.linux_fallback_kind.is_some()
                    && self.linux_fallback_kind != Some(CredentialStoreKind::File)
                {
                    return Err(ManifestValidationError(
                        "credential_store.linux_fallback_kind currently only supports \"file\""
                            .into(),
                    ));
                }
            }
        }

        Ok(())
    }

    fn expand_env_placeholders<F>(&mut self, env_lookup: &F, warnings: &mut Vec<String>)
    where
        F: Fn(&str) -> Option<String>,
    {
        expand_option_string(&mut self.path, env_lookup, warnings);
        expand_option_string(&mut self.macos_service, env_lookup, warnings);
        expand_option_string(&mut self.macos_account, env_lookup, warnings);
        expand_option_string(&mut self.linux_schema, env_lookup, warnings);
        expand_option_string(&mut self.linux_fallback_path, env_lookup, warnings);
        expand_string_map(&mut self.linux_attributes, env_lookup, warnings);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialStoreKind {
    File,
    Keychain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivationConfig {
    pub env_var: String,
    pub default_home: String,
    #[serde(default)]
    pub shared_paths: Vec<String>,
}

impl SessionActivationConfig {
    fn validate(&self) -> Result<(), ManifestValidationError> {
        require_non_empty("session_activation.env_var", &self.env_var)?;
        require_non_empty("session_activation.default_home", &self.default_home)?;

        for (index, path) in self.shared_paths.iter().enumerate() {
            require_non_empty(&format!("session_activation.shared_paths[{index}]"), path)?;
        }

        Ok(())
    }

    fn expand_env_placeholders<F>(&mut self, env_lookup: &F, warnings: &mut Vec<String>)
    where
        F: Fn(&str) -> Option<String>,
    {
        expand_string_in_place(&mut self.env_var, env_lookup, warnings);
        expand_string_in_place(&mut self.default_home, env_lookup, warnings);
        for path in &mut self.shared_paths {
            expand_string_in_place(path, env_lookup, warnings);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuxFile {
    pub path: String,
    pub required: bool,
    pub kind: AuxFileKind,
}

impl AuxFile {
    fn validate(&self, index: usize) -> Result<(), ManifestValidationError> {
        require_non_empty(&format!("aux_files[{index}].path"), &self.path)
    }

    fn expand_env_placeholders<F>(&mut self, env_lookup: &F, warnings: &mut Vec<String>)
    where
        F: Fn(&str) -> Option<String>,
    {
        expand_string_in_place(&mut self.path, env_lookup, warnings);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuxFileKind {
    File,
    Dir,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityExtract {
    pub field: String,
    pub source: IdentitySource,
    #[serde(default)]
    pub json_file_path: Option<String>,
    #[serde(default)]
    pub json_pointer: Option<String>,
    #[serde(default)]
    pub fallback_source: Option<IdentitySource>,
    #[serde(default)]
    pub fallback_json_file_path: Option<String>,
    #[serde(default)]
    pub fallback_json_pointer: Option<String>,
    #[serde(default)]
    pub jwt_from: Option<ValueSource>,
    #[serde(default)]
    pub jwt_json_pointer: Option<String>,
    #[serde(default)]
    pub claim_pointer: Option<String>,
    #[serde(default)]
    pub fallback_jwt_from: Option<ValueSource>,
    #[serde(default)]
    pub fallback_jwt_json_pointer: Option<String>,
    #[serde(default)]
    pub fallback_claim_pointer: Option<String>,
    #[serde(default)]
    pub json_top_keys_from: Option<ValueSource>,
    #[serde(default)]
    pub fallback_json_top_keys_from: Option<ValueSource>,
    #[serde(default)]
    pub literal: Option<String>,
    #[serde(default)]
    pub fallback_literal: Option<String>,
}

impl IdentityExtract {
    fn validate(&self, index: usize) -> Result<(), ManifestValidationError> {
        require_non_empty(&format!("identity_extract[{index}].field"), &self.field)?;
        validate_identity_source(&format!("identity_extract[{index}]"), self.source, self)?;

        if let Some(source) = self.fallback_source {
            validate_fallback_identity_source(&format!("identity_extract[{index}]"), source, self)?;
        }

        Ok(())
    }

    fn expand_env_placeholders<F>(&mut self, env_lookup: &F, warnings: &mut Vec<String>)
    where
        F: Fn(&str) -> Option<String>,
    {
        expand_option_string(&mut self.json_file_path, env_lookup, warnings);
        expand_option_string(&mut self.fallback_json_file_path, env_lookup, warnings);
        expand_option_string(&mut self.json_pointer, env_lookup, warnings);
        expand_option_string(&mut self.fallback_json_pointer, env_lookup, warnings);
        expand_option_string(&mut self.jwt_json_pointer, env_lookup, warnings);
        expand_option_string(&mut self.fallback_jwt_json_pointer, env_lookup, warnings);
        expand_option_string(&mut self.claim_pointer, env_lookup, warnings);
        expand_option_string(&mut self.fallback_claim_pointer, env_lookup, warnings);
        expand_option_string(&mut self.literal, env_lookup, warnings);
        expand_option_string(&mut self.fallback_literal, env_lookup, warnings);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentitySource {
    JsonFile,
    JsonValue,
    JwtClaim,
    JsonTopKeys,
    Literal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    JsonValue,
    JsonFile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginConfig {
    pub cmd: Vec<String>,
    pub ready_marker_kind: ReadyMarkerKind,
    pub ready_marker_timeout_s: u64,
    #[serde(default)]
    pub ready_marker_path: Option<String>,
}

impl LoginConfig {
    fn validate(&self) -> Result<(), ManifestValidationError> {
        if self.cmd.is_empty() {
            return Err(ManifestValidationError(
                "login.cmd must not be empty".into(),
            ));
        }

        if self.ready_marker_timeout_s == 0 {
            return Err(ManifestValidationError(
                "login.ready_marker_timeout_s must be greater than 0".into(),
            ));
        }

        if self.ready_marker_kind == ReadyMarkerKind::File {
            require_option("login.ready_marker_path", self.ready_marker_path.as_ref())?;
        }

        Ok(())
    }

    fn expand_env_placeholders<F>(&mut self, env_lookup: &F, warnings: &mut Vec<String>)
    where
        F: Fn(&str) -> Option<String>,
    {
        for segment in &mut self.cmd {
            expand_string_in_place(segment, env_lookup, warnings);
        }
        expand_option_string(&mut self.ready_marker_path, env_lookup, warnings);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadyMarkerKind {
    Keychain,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSource {
    pub kind: UsageSourceKind,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub response_metric_pointer: BTreeMap<String, String>,
    #[serde(default)]
    pub response_quota_pointer: BTreeMap<String, String>,
    #[serde(default)]
    pub cache_ttl_s: Option<u64>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub format: Option<UsageFormat>,
    #[serde(default)]
    pub timestamp_pointer: Option<String>,
    #[serde(default)]
    pub record_filter: Option<String>,
    #[serde(default)]
    pub metric_map: BTreeMap<String, String>,
    #[serde(default)]
    pub default_window: Option<String>,
}

impl UsageSource {
    fn validate(&self, index: usize) -> Result<(), ManifestValidationError> {
        match self.kind {
            UsageSourceKind::LocalLog => {
                require_option(&format!("usage_source[{index}].path"), self.path.as_ref())?;
                require_option(
                    &format!("usage_source[{index}].format"),
                    self.format.as_ref(),
                )?;
                require_option(
                    &format!("usage_source[{index}].timestamp_pointer"),
                    self.timestamp_pointer.as_ref(),
                )?;

                if self.metric_map.is_empty() {
                    return Err(ManifestValidationError(format!(
                        "usage_source[{index}].metric_map must not be empty"
                    )));
                }
            }
            UsageSourceKind::ProviderApi => {
                require_option(
                    &format!("usage_source[{index}].method"),
                    self.method.as_ref(),
                )?;
                require_option(&format!("usage_source[{index}].url"), self.url.as_ref())?;

                if self.response_metric_pointer.is_empty() && self.response_quota_pointer.is_empty()
                {
                    return Err(ManifestValidationError(format!(
                        "usage_source[{index}] requires response_metric_pointer or response_quota_pointer"
                    )));
                }
            }
        }

        Ok(())
    }

    fn expand_env_placeholders<F>(&mut self, env_lookup: &F, warnings: &mut Vec<String>)
    where
        F: Fn(&str) -> Option<String>,
    {
        expand_option_string(&mut self.method, env_lookup, warnings);
        expand_option_string(&mut self.url, env_lookup, warnings);
        expand_option_string(&mut self.path, env_lookup, warnings);
        expand_option_string(&mut self.timestamp_pointer, env_lookup, warnings);
        expand_option_string(&mut self.record_filter, env_lookup, warnings);
        expand_option_string(&mut self.default_window, env_lookup, warnings);
        expand_string_map(&mut self.headers, env_lookup, warnings);
        expand_string_map(&mut self.response_metric_pointer, env_lookup, warnings);
        expand_string_map(&mut self.response_quota_pointer, env_lookup, warnings);
        expand_string_map(&mut self.metric_map, env_lookup, warnings);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSourceKind {
    LocalLog,
    ProviderApi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageFormat {
    Jsonl,
    Json,
    RegexLines,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LoadedManifest {
    pub path: PathBuf,
    pub source: PluginSource,
    pub warnings: Vec<String>,
    pub manifest: Manifest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginSource {
    User,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PluginCatalog {
    pub plugins: Vec<LoadedManifest>,
    pub errors: Vec<PluginLoadFailure>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PluginLoadFailure {
    pub path: PathBuf,
    pub error: String,
}

pub fn load_manifest(path: &Path) -> Result<LoadedManifest, ManifestLoadError> {
    let source = fs::read_to_string(path).map_err(|source| ManifestLoadError::Read {
        path: path.to_path_buf(),
        source,
    })?;

    parse_manifest(&source, path, &|name| env::var(name).ok())
}

pub fn load_manifest_with_env_overrides(
    path: &Path,
    overrides: &BTreeMap<String, String>,
) -> Result<LoadedManifest, ManifestLoadError> {
    let source = fs::read_to_string(path).map_err(|source| ManifestLoadError::Read {
        path: path.to_path_buf(),
        source,
    })?;

    parse_manifest(&source, path, &|name| {
        overrides.get(name).cloned().or_else(|| env::var(name).ok())
    })
}

pub fn load_all(root: &Path) -> Result<PluginCatalog, PluginScanError> {
    if !root.exists() {
        return Ok(PluginCatalog::default());
    }

    let mut entries = fs::read_dir(root)
        .map_err(|source| PluginScanError::ReadDirectory {
            path: root.to_path_buf(),
            source,
        })?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();

    entries.sort_by_key(|entry| entry.file_name());

    let mut plugins_by_id = BTreeMap::new();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    for entry in entries {
        let manifest_path = entry.path().join("plugin.toml");
        if !manifest_path.is_file() {
            continue;
        }

        match load_manifest(&manifest_path) {
            Ok(loaded) => {
                if !loaded.manifest.supports_current_platform() {
                    continue;
                }

                if let Some(previous) =
                    plugins_by_id.insert(loaded.manifest.id.clone(), loaded.clone())
                {
                    warnings.push(format!(
                        "duplicate plugin id '{}' in {} overrides {}",
                        loaded.manifest.id,
                        loaded.path.display(),
                        previous.path.display()
                    ));
                }

                warnings.extend(loaded.warnings.iter().cloned());
            }
            Err(error) => errors.push(PluginLoadFailure {
                path: manifest_path,
                error: error.to_string(),
            }),
        }
    }

    Ok(PluginCatalog {
        plugins: plugins_by_id.into_values().collect(),
        errors,
        warnings,
    })
}

fn parse_manifest<F>(
    source: &str,
    path: &Path,
    env_lookup: &F,
) -> Result<LoadedManifest, ManifestLoadError>
where
    F: Fn(&str) -> Option<String>,
{
    let mut manifest: Manifest =
        toml::from_str(source).map_err(|source| ManifestLoadError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

    let warnings = manifest.expand_env_placeholders_with(env_lookup);
    manifest
        .validate()
        .map_err(|source| ManifestLoadError::Validation {
            path: path.to_path_buf(),
            source,
        })?;

    Ok(LoadedManifest {
        path: path.to_path_buf(),
        source: PluginSource::User,
        warnings,
        manifest,
    })
}

fn validate_identity_source(
    prefix: &str,
    source: IdentitySource,
    rule: &IdentityExtract,
) -> Result<(), ManifestValidationError> {
    match source {
        IdentitySource::JsonFile => {
            require_option(
                &format!("{prefix}.json_file_path"),
                rule.json_file_path.as_ref(),
            )?;
            require_option(
                &format!("{prefix}.json_pointer"),
                rule.json_pointer.as_ref(),
            )?;
        }
        IdentitySource::JsonValue => {
            require_option(
                &format!("{prefix}.json_pointer"),
                rule.json_pointer.as_ref(),
            )?;
        }
        IdentitySource::JwtClaim => {
            require_option(&format!("{prefix}.jwt_from"), rule.jwt_from.as_ref())?;
            require_option(
                &format!("{prefix}.jwt_json_pointer"),
                rule.jwt_json_pointer.as_ref(),
            )?;
            require_option(
                &format!("{prefix}.claim_pointer"),
                rule.claim_pointer.as_ref(),
            )?;
        }
        IdentitySource::JsonTopKeys => {
            require_option(
                &format!("{prefix}.json_top_keys_from"),
                rule.json_top_keys_from.as_ref(),
            )?;
        }
        IdentitySource::Literal => {
            require_option(&format!("{prefix}.literal"), rule.literal.as_ref())?;
        }
    }

    Ok(())
}

fn validate_fallback_identity_source(
    prefix: &str,
    source: IdentitySource,
    rule: &IdentityExtract,
) -> Result<(), ManifestValidationError> {
    match source {
        IdentitySource::JsonFile => {
            require_option(
                &format!("{prefix}.fallback_json_file_path"),
                rule.fallback_json_file_path.as_ref(),
            )?;
            require_option(
                &format!("{prefix}.fallback_json_pointer"),
                rule.fallback_json_pointer.as_ref(),
            )?;
        }
        IdentitySource::JsonValue => {
            require_option(
                &format!("{prefix}.fallback_json_pointer"),
                rule.fallback_json_pointer.as_ref(),
            )?;
        }
        IdentitySource::JwtClaim => {
            require_option(
                &format!("{prefix}.fallback_jwt_from"),
                rule.fallback_jwt_from.as_ref(),
            )?;
            require_option(
                &format!("{prefix}.fallback_jwt_json_pointer"),
                rule.fallback_jwt_json_pointer.as_ref(),
            )?;
            require_option(
                &format!("{prefix}.fallback_claim_pointer"),
                rule.fallback_claim_pointer.as_ref(),
            )?;
        }
        IdentitySource::JsonTopKeys => {
            require_option(
                &format!("{prefix}.fallback_json_top_keys_from"),
                rule.fallback_json_top_keys_from.as_ref(),
            )?;
        }
        IdentitySource::Literal => {
            require_option(
                &format!("{prefix}.fallback_literal"),
                rule.fallback_literal.as_ref(),
            )?;
        }
    }

    Ok(())
}

fn require_non_empty(field: &str, value: &str) -> Result<(), ManifestValidationError> {
    if value.trim().is_empty() {
        return Err(ManifestValidationError(format!(
            "{field} must not be empty"
        )));
    }

    Ok(())
}

fn require_option<T>(field: &str, value: Option<&T>) -> Result<(), ManifestValidationError> {
    if value.is_none() {
        return Err(ManifestValidationError(format!("{field} is required")));
    }

    Ok(())
}

fn expand_option_string<F>(value: &mut Option<String>, env_lookup: &F, warnings: &mut Vec<String>)
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(value) = value {
        expand_string_in_place(value, env_lookup, warnings);
    }
}

fn expand_string_map<F>(
    values: &mut BTreeMap<String, String>,
    env_lookup: &F,
    warnings: &mut Vec<String>,
) where
    F: Fn(&str) -> Option<String>,
{
    for value in values.values_mut() {
        expand_string_in_place(value, env_lookup, warnings);
    }
}

fn expand_string_in_place<F>(value: &mut String, env_lookup: &F, warnings: &mut Vec<String>)
where
    F: Fn(&str) -> Option<String>,
{
    *value = expand_env_placeholders(value, env_lookup, warnings);
}

fn expand_env_placeholders<F>(input: &str, env_lookup: &F, warnings: &mut Vec<String>) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let mut remaining = input;
    let mut output = String::new();

    while let Some(start) = remaining.find("${env:") {
        output.push_str(&remaining[..start]);
        let expression = &remaining[start + 6..];

        let Some(end) = expression.find('}') else {
            output.push_str(&remaining[start..]);
            return output;
        };

        output.push_str(&resolve_env_expression(
            &expression[..end],
            env_lookup,
            warnings,
        ));
        remaining = &expression[end + 1..];
    }

    output.push_str(remaining);
    output
}

fn resolve_env_expression<F>(expression: &str, env_lookup: &F, warnings: &mut Vec<String>) -> String
where
    F: Fn(&str) -> Option<String>,
{
    if let Some((name, default)) = expression.split_once(":-") {
        return env_lookup(name)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default.to_string());
    }

    match env_lookup(expression) {
        Some(value) => value,
        None => {
            warnings.push(format!(
                "environment variable '{}' is not set; substituting empty string",
                expression
            ));
            String::new()
        }
    }
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ManifestValidationError(pub String);

#[derive(Debug, Error)]
pub enum ManifestLoadError {
    #[error("failed to read plugin manifest {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse plugin manifest {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid plugin manifest {path}: {source}")]
    Validation {
        path: PathBuf,
        #[source]
        source: ManifestValidationError,
    },
}

#[derive(Debug, Error)]
pub enum PluginScanError {
    #[error("failed to read plugin directory {path}")]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{expand_env_placeholders, load_all, parse_manifest, Platform};

    #[test]
    fn env_expansion_supports_defaults_and_missing_values() {
        let mut warnings = Vec::new();
        let expanded = expand_env_placeholders(
            "${env:SET:-fallback}-${env:MISSING:-fallback}-${env:EMPTY}",
            &|name| match name {
                "SET" => Some("value".into()),
                "EMPTY" => None,
                _ => None,
            },
            &mut warnings,
        );

        assert_eq!(expanded, "value-fallback-");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn manifest_validation_rejects_missing_required_fields() {
        let source = r#"
id = "broken"
display_name = "Broken"
version = "1.0.0"
author = "test"
description = "broken"
platforms = ["macos"]

[credential_store]
kind = "file"

[login]
cmd = ["broken"]
ready_marker_kind = "file"
ready_marker_timeout_s = 10
ready_marker_path = "~/broken"
"#;

        let path = PathBuf::from("/tmp/broken/plugin.toml");
        let error = parse_manifest(source, &path, &|_| None).expect_err("manifest should fail");

        assert!(error
            .to_string()
            .contains("credential_store.path is required"));
    }

    #[test]
    fn load_all_skips_unsupported_platforms_and_overrides_duplicates() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let plugin_root = temp_dir.path();

        fs::create_dir_all(plugin_root.join("first")).expect("create first");
        fs::create_dir_all(plugin_root.join("second")).expect("create second");
        fs::create_dir_all(plugin_root.join("linux-only")).expect("create linux");

        let current_platform = match Platform::current().expect("platform") {
            Platform::Macos => "macos",
            Platform::Linux => "linux",
            Platform::Windows => "windows",
        };
        let other_platform = if current_platform == "macos" {
            "linux"
        } else {
            "macos"
        };

        fs::write(
            plugin_root.join("first/plugin.toml"),
            manifest_with("duplicate", "1.0.0", current_platform),
        )
        .expect("write first");
        fs::write(
            plugin_root.join("second/plugin.toml"),
            manifest_with("duplicate", "2.0.0", current_platform),
        )
        .expect("write second");
        fs::write(
            plugin_root.join("linux-only/plugin.toml"),
            manifest_with("linux-only", "1.0.0", other_platform),
        )
        .expect("write third");

        let catalog = load_all(plugin_root).expect("load all");

        assert_eq!(catalog.plugins.len(), 1);
        assert_eq!(catalog.plugins[0].manifest.version, "2.0.0");
        assert_eq!(catalog.errors.len(), 0);
        assert_eq!(catalog.warnings.len(), 1);
    }

    fn manifest_with(id: &str, version: &str, platform: &str) -> String {
        format!(
            r#"
id = "{id}"
display_name = "{id}"
version = "{version}"
author = "test"
description = "{id}"
platforms = ["{platform}"]

[credential_store]
kind = "file"
path = "~/.{id}/auth.json"
permissions = 384

[login]
cmd = ["{id}", "login"]
ready_marker_kind = "file"
ready_marker_timeout_s = 30
ready_marker_path = "~/.{id}/auth.json"
"#
        )
    }
}
