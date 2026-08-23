use std::{
    collections::BTreeMap,
    env,
    fmt::{self, Display},
    fs::File,
    io::{self, Read},
    net::IpAddr,
    path::{Path, PathBuf},
    str::FromStr,
};

use jsonc_parser::{ParseOptions, parse_to_serde_value};
use reqwest::Url;
use serde::Deserialize;
use thiserror::Error;

const MAX_CONFIG_BYTES: usize = 256 * 1024;
const MAX_PROVIDERS: usize = 32;
const MAX_MODELS: usize = 256;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_HEADER_COUNT: usize = 32;
const MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;

pub const DEFAULT_MODEL_REFERENCE: &str = "openai/gpt-5.3-codex";
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModelRef {
    provider: String,
    model: String,
}

impl ModelRef {
    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

impl Display for ModelRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model)
    }
}

impl FromStr for ModelRef {
    type Err = ModelRefError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (provider, model) = value
            .split_once('/')
            .ok_or(ModelRefError::MissingSeparator)?;
        if provider.is_empty() || model.is_empty() {
            return Err(ModelRefError::EmptyPart);
        }
        if provider.len() > MAX_IDENTIFIER_BYTES || model.len() > MAX_IDENTIFIER_BYTES {
            return Err(ModelRefError::TooLong);
        }
        if !provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(ModelRefError::InvalidProvider);
        }
        if model.chars().any(char::is_control) {
            return Err(ModelRefError::InvalidModel);
        }
        Ok(Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
        })
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ModelRefError {
    #[error("expected provider/model")]
    MissingSeparator,
    #[error("provider and model must not be empty")]
    EmptyPart,
    #[error("provider or model identifier is too long")]
    TooLong,
    #[error("provider may contain only letters, digits, '-' and '_'")]
    InvalidProvider,
    #[error("model identifier contains control characters")]
    InvalidModel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) enum Protocol {
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "openai-chat")]
    OpenAiChat,
}

#[derive(Clone)]
pub struct ResolvedConfig {
    selected_model: ModelRef,
    providers: BTreeMap<String, ProviderConfig>,
}

impl ResolvedConfig {
    pub fn selected_model(&self) -> &ModelRef {
        &self.selected_model
    }

    pub(crate) fn providers(&self) -> &BTreeMap<String, ProviderConfig> {
        &self.providers
    }
}

#[derive(Clone)]
pub(crate) struct ProviderConfig {
    pub(crate) name: Option<String>,
    pub(crate) protocol: Protocol,
    pub(crate) base_url: Url,
    pub(crate) api_key_env: Option<String>,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) timeout_ms: Option<u64>,
    pub(crate) stream_idle_timeout_ms: Option<u64>,
    pub(crate) models: BTreeMap<String, ModelConfig>,
}

#[derive(Clone, Default)]
pub(crate) struct ModelConfig {
    pub(crate) name: Option<String>,
    pub(crate) options: ModelOptions,
}

#[derive(Clone, Default)]
pub(crate) struct ModelOptions {
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) text_verbosity: Option<TextVerbosity>,
    pub(crate) temperature: Option<f64>,
    pub(crate) max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningEffort {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TextVerbosity {
    Low,
    Medium,
    High,
}

impl TextVerbosity {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

pub struct Configuration;

impl Configuration {
    pub fn load(workspace: &Path) -> Result<ResolvedConfig, ConfigError> {
        let mut merged = RawConfig::defaults();
        let mut configured_model = false;

        if let Some(path) = global_config_path()
            && let Some(path) = find_config(&path, "config")
        {
            let config = load_file(&path)?;
            configured_model |= config.model.is_some();
            merged.merge(config);
        }

        if let Some(path) = find_config(workspace, "yap") {
            let config = load_file(&path)?;
            validate_project_config(&path, &config)?;
            configured_model |= config.model.is_some();
            merged.merge(config);
        }

        if let Some(path) = env::var_os("YAP_CONFIG") {
            let path = PathBuf::from(path);
            let config = load_file(&path)?;
            configured_model |= config.model.is_some();
            merged.merge(config);
        }

        if let Ok(base_url) = env::var("OPENAI_BASE_URL")
            && !base_url.trim().is_empty()
        {
            merged
                .provider
                .entry("openai".into())
                .or_default()
                .options
                .base_url = Some(base_url);
        }

        if let Ok(model) = env::var("YAP_MODEL") {
            if !model.trim().is_empty() {
                merged.model = Some(model);
            }
        } else if !configured_model
            && let Ok(model) = env::var("OPENAI_MODEL")
            && !model.trim().is_empty()
        {
            merged.model = Some(format!("openai/{model}"));
        }

        resolve(merged)
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("config {path} exceeds {MAX_CONFIG_BYTES} bytes")]
    TooLarge { path: PathBuf },
    #[error("invalid config {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("unsupported config version {version} in {path}; expected version 1")]
    UnsupportedVersion { path: PathBuf, version: u32 },
    #[error("project config {path} cannot configure provider connection field {field}")]
    ForbiddenProjectSetting { path: PathBuf, field: String },
    #[error("invalid model reference {value:?}: {source}")]
    InvalidModelReference {
        value: String,
        #[source]
        source: ModelRefError,
    },
    #[error("config contains more than {MAX_PROVIDERS} providers")]
    TooManyProviders,
    #[error("config contains more than {MAX_MODELS} models")]
    TooManyModels,
    #[error("invalid provider {provider}: {message}")]
    InvalidProvider { provider: String, message: String },
    #[error("invalid model {provider}/{model}: {message}")]
    InvalidModel {
        provider: String,
        model: String,
        message: String,
    },
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawConfig {
    #[serde(rename = "$schema")]
    schema: Option<String>,
    version: Option<u32>,
    model: Option<String>,
    #[serde(default)]
    provider: BTreeMap<String, RawProvider>,
}

impl RawConfig {
    fn defaults() -> Self {
        Self {
            schema: None,
            version: Some(1),
            model: Some(DEFAULT_MODEL_REFERENCE.into()),
            provider: BTreeMap::from([(
                "openai".into(),
                RawProvider {
                    name: Some("OpenAI".into()),
                    protocol: Some(Protocol::OpenAiResponses),
                    options: RawProviderOptions {
                        base_url: Some(DEFAULT_OPENAI_BASE_URL.into()),
                        api_key_env: Some("OPENAI_API_KEY".into()),
                        ..RawProviderOptions::default()
                    },
                    models: BTreeMap::new(),
                },
            )]),
        }
    }

    fn merge(&mut self, next: Self) {
        if next.schema.is_some() {
            self.schema = next.schema;
        }
        if next.version.is_some() {
            self.version = next.version;
        }
        if next.model.is_some() {
            self.model = next.model;
        }
        for (id, provider) in next.provider {
            self.provider.entry(id).or_default().merge(provider);
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawProvider {
    name: Option<String>,
    protocol: Option<Protocol>,
    #[serde(default)]
    options: RawProviderOptions,
    #[serde(default)]
    models: BTreeMap<String, RawModel>,
}

impl RawProvider {
    fn merge(&mut self, next: Self) {
        if next.name.is_some() {
            self.name = next.name;
        }
        if next.protocol.is_some() {
            self.protocol = next.protocol;
        }
        self.options.merge(next.options);
        for (id, model) in next.models {
            self.models.entry(id).or_default().merge(model);
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawProviderOptions {
    #[serde(rename = "baseURL")]
    base_url: Option<String>,
    api_key_env: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    timeout_ms: Option<u64>,
    stream_idle_timeout_ms: Option<u64>,
}

impl RawProviderOptions {
    fn merge(&mut self, next: Self) {
        if next.base_url.is_some() {
            self.base_url = next.base_url;
        }
        if next.api_key_env.is_some() {
            self.api_key_env = next.api_key_env;
        }
        self.headers.extend(next.headers);
        if next.timeout_ms.is_some() {
            self.timeout_ms = next.timeout_ms;
        }
        if next.stream_idle_timeout_ms.is_some() {
            self.stream_idle_timeout_ms = next.stream_idle_timeout_ms;
        }
    }

    fn has_connection_settings(&self) -> Option<&'static str> {
        if self.base_url.is_some() {
            Some("options.baseURL")
        } else if self.api_key_env.is_some() {
            Some("options.apiKeyEnv")
        } else if !self.headers.is_empty() {
            Some("options.headers")
        } else if self.timeout_ms.is_some() {
            Some("options.timeoutMs")
        } else if self.stream_idle_timeout_ms.is_some() {
            Some("options.streamIdleTimeoutMs")
        } else {
            None
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawModel {
    name: Option<String>,
    #[serde(default)]
    options: RawModelOptions,
}

impl RawModel {
    fn merge(&mut self, next: Self) {
        if next.name.is_some() {
            self.name = next.name;
        }
        self.options.merge(next.options);
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawModelOptions {
    reasoning_effort: Option<ReasoningEffort>,
    text_verbosity: Option<TextVerbosity>,
    temperature: Option<f64>,
    max_output_tokens: Option<u32>,
}

impl RawModelOptions {
    fn merge(&mut self, next: Self) {
        if next.reasoning_effort.is_some() {
            self.reasoning_effort = next.reasoning_effort;
        }
        if next.text_verbosity.is_some() {
            self.text_verbosity = next.text_verbosity;
        }
        if next.temperature.is_some() {
            self.temperature = next.temperature;
        }
        if next.max_output_tokens.is_some() {
            self.max_output_tokens = next.max_output_tokens;
        }
    }
}

fn global_config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("yap"))
    }
    #[cfg(not(windows))]
    {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|path| path.join("yap"))
    }
}

fn find_config(directory: &Path, stem: &str) -> Option<PathBuf> {
    ["jsonc", "json"]
        .into_iter()
        .map(|extension| directory.join(format!("{stem}.{extension}")))
        .find(|path| path.is_file())
}

fn load_file(path: &Path) -> Result<RawConfig, ConfigError> {
    let mut file = File::open(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            path: path.to_owned(),
        });
    }
    let text = String::from_utf8(bytes).map_err(|error| ConfigError::Parse {
        path: path.to_owned(),
        message: format!("config is not UTF-8: {error}"),
    })?;
    parse_config(path, &text)
}

fn parse_config(path: &Path, text: &str) -> Result<RawConfig, ConfigError> {
    let options = ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    };
    let config: RawConfig =
        parse_to_serde_value(text, &options).map_err(|error| ConfigError::Parse {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    if let Some(version) = config.version
        && version != 1
    {
        return Err(ConfigError::UnsupportedVersion {
            path: path.to_owned(),
            version,
        });
    }
    Ok(config)
}

fn validate_project_config(path: &Path, config: &RawConfig) -> Result<(), ConfigError> {
    for (provider_id, provider) in &config.provider {
        let field = if provider.protocol.is_some() {
            Some("protocol")
        } else {
            provider.options.has_connection_settings()
        };
        if let Some(field) = field {
            return Err(ConfigError::ForbiddenProjectSetting {
                path: path.to_owned(),
                field: format!("provider.{provider_id}.{field}"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn resolve_test_config(text: &str) -> Result<ResolvedConfig, ConfigError> {
    let mut raw = RawConfig::defaults();
    raw.merge(parse_config(Path::new("test.jsonc"), text)?);
    resolve(raw)
}

fn resolve(raw: RawConfig) -> Result<ResolvedConfig, ConfigError> {
    if raw.provider.len() > MAX_PROVIDERS {
        return Err(ConfigError::TooManyProviders);
    }
    let selected_value = raw
        .model
        .unwrap_or_else(|| DEFAULT_MODEL_REFERENCE.to_owned());
    let selected_model =
        selected_value
            .parse()
            .map_err(|source| ConfigError::InvalidModelReference {
                value: selected_value,
                source,
            })?;

    let model_count = raw
        .provider
        .values()
        .map(|provider| provider.models.len())
        .sum::<usize>();
    if model_count > MAX_MODELS {
        return Err(ConfigError::TooManyModels);
    }

    let mut providers = BTreeMap::new();
    for (provider_id, raw_provider) in raw.provider {
        validate_provider_id(&provider_id)?;
        let protocol = raw_provider.protocol.unwrap_or(Protocol::OpenAiResponses);
        let base_url_value =
            raw_provider
                .options
                .base_url
                .ok_or_else(|| ConfigError::InvalidProvider {
                    provider: provider_id.clone(),
                    message: "options.baseURL is required".into(),
                })?;
        let base_url = validate_base_url(&provider_id, &base_url_value)?;
        if let Some(api_key_env) = &raw_provider.options.api_key_env {
            validate_env_name(&provider_id, api_key_env)?;
        }
        validate_headers(&provider_id, &raw_provider.options.headers)?;
        validate_timeout(&provider_id, "timeoutMs", raw_provider.options.timeout_ms)?;
        validate_timeout(
            &provider_id,
            "streamIdleTimeoutMs",
            raw_provider.options.stream_idle_timeout_ms,
        )?;

        let mut models = BTreeMap::new();
        for (model_id, raw_model) in raw_provider.models {
            if model_id.is_empty() || model_id.len() > MAX_IDENTIFIER_BYTES {
                return Err(ConfigError::InvalidModel {
                    provider: provider_id.clone(),
                    model: model_id,
                    message: "model identifier must be between 1 and 256 bytes".into(),
                });
            }
            if raw_model
                .options
                .temperature
                .is_some_and(|value| !(0.0..=2.0).contains(&value))
            {
                return Err(ConfigError::InvalidModel {
                    provider: provider_id.clone(),
                    model: model_id,
                    message: "temperature must be between 0 and 2".into(),
                });
            }
            if raw_model.options.max_output_tokens == Some(0) {
                return Err(ConfigError::InvalidModel {
                    provider: provider_id.clone(),
                    model: model_id,
                    message: "maxOutputTokens must be greater than zero".into(),
                });
            }
            models.insert(
                model_id,
                ModelConfig {
                    name: raw_model.name,
                    options: ModelOptions {
                        reasoning_effort: raw_model.options.reasoning_effort,
                        text_verbosity: raw_model.options.text_verbosity,
                        temperature: raw_model.options.temperature,
                        max_output_tokens: raw_model.options.max_output_tokens,
                    },
                },
            );
        }

        providers.insert(
            provider_id,
            ProviderConfig {
                name: raw_provider.name,
                protocol,
                base_url,
                api_key_env: raw_provider.options.api_key_env,
                headers: raw_provider.options.headers,
                timeout_ms: raw_provider.options.timeout_ms,
                stream_idle_timeout_ms: raw_provider.options.stream_idle_timeout_ms,
                models,
            },
        );
    }

    Ok(ResolvedConfig {
        selected_model,
        providers,
    })
}

fn validate_provider_id(provider: &str) -> Result<(), ConfigError> {
    if provider.is_empty()
        || provider.len() > MAX_IDENTIFIER_BYTES
        || !provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ConfigError::InvalidProvider {
            provider: provider.to_owned(),
            message: "ID may contain only letters, digits, '-' and '_'".into(),
        });
    }
    Ok(())
}

fn validate_base_url(provider: &str, value: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(value).map_err(|error| ConfigError::InvalidProvider {
        provider: provider.to_owned(),
        message: format!("invalid options.baseURL: {error}"),
    })?;
    if url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidProvider {
            provider: provider.to_owned(),
            message: "options.baseURL cannot contain credentials, query, or fragment".into(),
        });
    }
    let is_loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback) {
        return Err(ConfigError::InvalidProvider {
            provider: provider.to_owned(),
            message: "options.baseURL must use HTTPS, except for loopback HTTP endpoints".into(),
        });
    }
    Ok(url)
}

fn validate_env_name(provider: &str, value: &str) -> Result<(), ConfigError> {
    let mut bytes = value.bytes();
    let valid = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid || value.len() > MAX_IDENTIFIER_BYTES {
        return Err(ConfigError::InvalidProvider {
            provider: provider.to_owned(),
            message: "options.apiKeyEnv is not a valid environment variable name".into(),
        });
    }
    Ok(())
}

fn validate_headers(provider: &str, headers: &BTreeMap<String, String>) -> Result<(), ConfigError> {
    if headers.len() > MAX_HEADER_COUNT {
        return Err(ConfigError::InvalidProvider {
            provider: provider.to_owned(),
            message: format!("options.headers exceeds {MAX_HEADER_COUNT} entries"),
        });
    }
    const RESERVED: &[&str] = &[
        "authorization",
        "content-length",
        "content-type",
        "host",
        "x-api-key",
    ];
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if RESERVED.contains(&lower.as_str()) {
            return Err(ConfigError::InvalidProvider {
                provider: provider.to_owned(),
                message: format!("options.headers cannot override {name}"),
            });
        }
        if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err()
            || reqwest::header::HeaderValue::from_str(value).is_err()
            || value.len() > MAX_HEADER_VALUE_BYTES
        {
            return Err(ConfigError::InvalidProvider {
                provider: provider.to_owned(),
                message: format!("invalid options.headers entry {name}"),
            });
        }
    }
    Ok(())
}

fn validate_timeout(provider: &str, name: &str, value: Option<u64>) -> Result<(), ConfigError> {
    if value.is_some_and(|value| !(1_000..=600_000).contains(&value)) {
        return Err(ConfigError::InvalidProvider {
            provider: provider.to_owned(),
            message: format!("options.{name} must be between 1000 and 600000 milliseconds"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn published_config_schema_is_valid_json() {
        let schema = include_str!("../schema/config.schema.json");
        let value: serde_json::Value = serde_json::from_str(schema).unwrap();

        assert_eq!(value["properties"]["version"]["const"], 1);
    }

    #[test]
    fn model_reference_splits_only_on_the_first_slash() {
        let reference: ModelRef = "openrouter/anthropic/claude-sonnet".parse().unwrap();

        assert_eq!(reference.provider(), "openrouter");
        assert_eq!(reference.model(), "anthropic/claude-sonnet");
        assert_eq!(reference.to_string(), "openrouter/anthropic/claude-sonnet");
    }

    #[test]
    fn jsonc_config_accepts_comments_and_trailing_commas() {
        let config = parse_config(
            Path::new("config.jsonc"),
            r#"{
                // model selection
                "version": 1,
                "model": "openai/gpt-test",
            }"#,
        )
        .unwrap();

        assert_eq!(config.model.as_deref(), Some("openai/gpt-test"));
    }

    #[test]
    fn config_rejects_unknown_fields() {
        let error = match parse_config(Path::new("config.jsonc"), r#"{"unknown": true}"#) {
            Ok(_) => panic!("unknown fields should fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn merge_keeps_global_connection_and_applies_project_model_settings() {
        let mut config = RawConfig::defaults();
        config.merge(
            parse_config(
                Path::new("global.jsonc"),
                r#"{
                    "provider": {
                        "openrouter": {
                            "protocol": "openai-chat",
                            "options": {
                                "baseURL": "https://openrouter.ai/api/v1",
                                "apiKeyEnv": "OPENROUTER_API_KEY"
                            }
                        }
                    }
                }"#,
            )
            .unwrap(),
        );
        let project = parse_config(
            Path::new("project.jsonc"),
            r#"{
                "model": "openrouter/anthropic/claude-sonnet",
                "provider": {
                    "openrouter": {
                        "models": {
                            "anthropic/claude-sonnet": {
                                "options": { "temperature": 0.2 }
                            }
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        validate_project_config(Path::new("project.jsonc"), &project).unwrap();
        config.merge(project);

        let resolved = resolve(config).unwrap();
        let provider = &resolved.providers()["openrouter"];
        assert_eq!(provider.base_url.as_str(), "https://openrouter.ai/api/v1");
        assert_eq!(resolved.selected_model().provider(), "openrouter");
        assert_eq!(
            provider.models["anthropic/claude-sonnet"]
                .options
                .temperature,
            Some(0.2)
        );
    }

    #[test]
    fn project_config_cannot_redirect_a_provider_connection() {
        let project = parse_config(
            Path::new("yap.jsonc"),
            r#"{
                "provider": {
                    "openai": {
                        "options": { "baseURL": "https://attacker.example/v1" }
                    }
                }
            }"#,
        )
        .unwrap();

        let error = validate_project_config(Path::new("yap.jsonc"), &project)
            .expect_err("project connection settings should fail");
        assert!(
            error
                .to_string()
                .contains("provider.openai.options.baseURL")
        );
    }

    #[test]
    fn non_loopback_plain_http_provider_is_rejected() {
        let error = validate_base_url("custom", "http://example.com/v1")
            .expect_err("plain remote HTTP should fail");

        assert!(error.to_string().contains("must use HTTPS"));
        assert!(validate_base_url("ollama", "http://127.0.0.1:11434/v1").is_ok());
    }

    #[test]
    fn config_file_read_is_bounded() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.jsonc");
        std::fs::write(&path, "x".repeat(MAX_CONFIG_BYTES + 1)).unwrap();

        assert!(matches!(
            load_file(&path),
            Err(ConfigError::TooLarge { .. })
        ));
    }
}
