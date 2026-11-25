//! TOML configuration.
//!
//! Precedence is command line > environment > configuration file > defaults.
//! Secrets are never written in the file: each one names an environment variable
//! (`*_env`) or a protected file (`*_file`, which must not be readable by group or
//! others). Unknown keys are rejected so a misspelled or removed option (for example a
//! plaintext `api_key`) fails validation instead of being silently ignored.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use arustex_core::Secret;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid configuration in {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("secret {name}: {message}")]
    Secret { name: String, message: String },
}

pub type ConfigResult<T> = Result<T, ConfigError>;

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(message.into())
}

/// Where a secret comes from. Exactly one of the two fields must be set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
}

/// Resolves secrets from the process environment and the filesystem. Tests provide
/// their own environment map.
pub trait Environment {
    fn var(&self, name: &str) -> Option<String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl Environment for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }
}

impl Environment for std::collections::BTreeMap<String, String> {
    fn var(&self, name: &str) -> Option<String> {
        self.get(name).cloned().filter(|v| !v.is_empty())
    }
}

/// Reads a secret file after checking that only its owner can read it.
pub fn read_protected_file(path: &Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is accessible by group or others (mode {:o}); run chmod 600",
            path.display(),
            mode & 0o777
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value = text.trim_end_matches(['\n', '\r']).to_owned();
    if value.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(value)
}

impl SecretRef {
    pub fn is_set(&self) -> bool {
        self.env.is_some() || self.file.is_some()
    }

    pub fn resolve(&self, name: &str, env: &dyn Environment) -> ConfigResult<Secret> {
        let err = |message: String| ConfigError::Secret {
            name: name.to_owned(),
            message,
        };
        match (&self.env, &self.file) {
            (Some(_), Some(_)) => Err(err("set either env or file, not both".into())),
            (Some(var), None) => env
                .var(var)
                .map(Secret::new)
                .ok_or_else(|| err(format!("environment variable {var} is not set"))),
            (None, Some(path)) => read_protected_file(path).map(Secret::new).map_err(err),
            (None, None) => Err(err("no env or file source configured".into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DatabaseConfig {
    /// Connection URL without a password (e.g. `postgres://arustex@/arustex?host=/run/postgresql`).
    pub url: Option<String>,
    /// Full connection URL from an environment variable or protected file.
    pub url_secret: SecretRef,
    pub max_connections: u32,
    pub acquire_timeout_secs: u64,
    pub statement_timeout_secs: u64,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: None,
            url_secret: SecretRef {
                env: Some("ARUSTEX_DATABASE_URL".into()),
                file: None,
            },
            max_connections: 16,
            acquire_timeout_secs: 10,
            statement_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonConfig {
    /// Unix socket path; defaults to `$XDG_RUNTIME_DIR/arustex/arustex.sock`.
    pub socket: Option<PathBuf>,
    pub max_clients: usize,
    /// Outbound frames buffered per client before it is disconnected as too slow.
    pub client_queue: usize,
    pub event_buffer: usize,
    pub request_timeout_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket: None,
            max_clients: 32,
            client_queue: 512,
            event_buffer: 1024,
            request_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiChat,
    OpenaiResponses,
    Anthropic,
    /// Deterministic local provider for tests and demonstrations; never networked.
    Mock,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub name: String,
    pub kind: ProviderKind,
    pub model: String,
    /// Endpoint base URL. Required for compatible endpoints; defaults per kind.
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: SecretRef,
    #[serde(default)]
    pub default: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// Client-side request rate limit; zero disables it.
    #[serde(default)]
    pub requests_per_minute: u32,
    #[serde(default = "default_max_output")]
    pub max_output_tokens: u32,
    #[serde(default = "default_context")]
    pub context_window: u32,
}

fn default_timeout() -> u64 {
    120
}
fn default_connect_timeout() -> u64 {
    10
}
fn default_retries() -> u32 {
    3
}
fn default_max_output() -> u32 {
    4096
}
fn default_context() -> u32 {
    128_000
}

impl ProviderConfig {
    pub fn effective_base_url(&self) -> Option<String> {
        self.base_url.clone().or_else(|| match self.kind {
            ProviderKind::OpenaiChat | ProviderKind::OpenaiResponses => {
                Some("https://api.openai.com/v1".into())
            }
            ProviderKind::Anthropic => Some("https://api.anthropic.com".into()),
            ProviderKind::Mock => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOverride {
    pub key: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub model_profile: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub token_budget: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SchedulerConfig {
    pub enabled: bool,
    pub poll_interval_ms: u64,
    pub lease_secs: u64,
    pub max_concurrent_sessions: usize,
    pub max_attempts: u32,
    pub planner_enabled: bool,
    pub reviewer_enabled: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_ms: 1000,
            lease_secs: 120,
            max_concurrent_sessions: 4,
            max_attempts: 3,
            planner_enabled: true,
            reviewer_enabled: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsConfig {
    pub max_record_line_bytes: usize,
    pub max_record_body_bytes: usize,
    pub max_records_per_import: u64,
    pub attachment_max_bytes: u64,
    pub operation_timeout_secs: u64,
    pub max_operation_output_bytes: usize,
    pub record_retention_days: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_record_line_bytes: 1024 * 1024,
            max_record_body_bytes: 512 * 1024,
            max_records_per_import: 1_000_000,
            attachment_max_bytes: 64 * 1024 * 1024,
            operation_timeout_secs: 30,
            max_operation_output_bytes: 64 * 1024,
            record_retention_days: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyEvent {
    TaskCompleted,
    TaskFailed,
    TaskNeedsReview,
    ApprovalRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TelegramConfig {
    /// Delivery is disabled unless the operator enables it.
    pub enabled: bool,
    pub bot_token: SecretRef,
    /// The single destination chat; workflow agents cannot change it.
    pub chat_id: Option<i64>,
    pub api_base: String,
    pub events: BTreeSet<NotifyEvent>,
    pub min_interval_secs: u64,
    pub max_attempts: u32,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: SecretRef::default(),
            chat_id: None,
            api_base: "https://api.telegram.org".into(),
            events: [
                NotifyEvent::TaskCompleted,
                NotifyEvent::TaskFailed,
                NotifyEvent::ApprovalRequested,
            ]
            .into_iter()
            .collect(),
            min_interval_secs: 3,
            max_attempts: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LoggingConfig {
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub database: DatabaseConfig,
    pub daemon: DaemonConfig,
    pub providers: Vec<ProviderConfig>,
    pub agents: Vec<AgentOverride>,
    pub scheduler: SchedulerConfig,
    pub limits: LimitsConfig,
    pub telegram: TelegramConfig,
    pub logging: LoggingConfig,
}

/// Values given on the command line; they override everything else.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub config_path: Option<PathBuf>,
    pub database_url: Option<String>,
    pub socket: Option<PathBuf>,
    pub log_level: Option<String>,
}

/// Configuration plus resolved secrets, ready for bootstrap.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub source: Option<PathBuf>,
    pub database_url: Secret,
    pub socket: PathBuf,
    pub provider_keys: Vec<(String, Option<Secret>)>,
    pub telegram_token: Option<Secret>,
}

fn is_loopback_http(url: &str) -> bool {
    let rest = url.strip_prefix("http://").unwrap_or("");
    let host = rest.split(['/', ':']).next().unwrap_or("");
    matches!(host, "127.0.0.1" | "localhost" | "[") || rest.starts_with("[::1]")
}

fn check_endpoint(what: &str, url: &str) -> ConfigResult<()> {
    if url.starts_with("https://") {
        let host = &url[8..];
        if host.is_empty() || host.starts_with('/') || host.contains('@') {
            return Err(invalid(format!("{what}: invalid URL {url}")));
        }
        return Ok(());
    }
    if is_loopback_http(url) {
        return Ok(());
    }
    Err(invalid(format!(
        "{what}: endpoint must use https (plain http is allowed only on loopback): {url}"
    )))
}

fn url_has_password(url: &str) -> bool {
    let Some(rest) = url.split_once("://").map(|(_, r)| r) else {
        return false;
    };
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    authority
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| userinfo.contains(':'))
        || url.contains("password=")
}

impl Config {
    pub fn parse(text: &str, path: &Path) -> ConfigResult<Self> {
        toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: path.to_owned(),
            message: e.to_string(),
        })
    }

    /// Structural validation that does not touch secrets.
    pub fn validate(&self) -> ConfigResult<()> {
        if let Some(url) = &self.database.url {
            if url_has_password(url) {
                return Err(invalid(
                    "database.url must not contain a password; use database.url_secret (env or file)",
                ));
            }
            if !url.starts_with("postgres://") && !url.starts_with("postgresql://") {
                return Err(invalid("database.url must be a postgres:// URL"));
            }
        }
        if self.database.max_connections == 0 || self.database.max_connections > 256 {
            return Err(invalid("database.max_connections must be 1..=256"));
        }
        if self.daemon.max_clients == 0
            || self.daemon.client_queue < 16
            || self.daemon.event_buffer < 16
        {
            return Err(invalid("daemon queue sizes are too small"));
        }
        let mut names = BTreeSet::new();
        let mut defaults = 0;
        for p in &self.providers {
            if p.name.is_empty()
                || p.name.len() > 64
                || !p
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(invalid(format!(
                    "provider name {:?} must be 1..=64 of [A-Za-z0-9_-]",
                    p.name
                )));
            }
            if !names.insert(p.name.as_str()) {
                return Err(invalid(format!("duplicate provider {}", p.name)));
            }
            if p.model.trim().is_empty() {
                return Err(invalid(format!("provider {}: model is required", p.name)));
            }
            if p.default {
                defaults += 1;
            }
            if p.kind == ProviderKind::Mock {
                if p.base_url.is_some() || p.api_key.is_set() {
                    return Err(invalid(format!(
                        "provider {}: the mock provider takes no endpoint or key",
                        p.name
                    )));
                }
            } else {
                if let Some(url) = p.effective_base_url() {
                    check_endpoint(&format!("provider {}", p.name), &url)?;
                }
                if !p.api_key.is_set() {
                    return Err(invalid(format!(
                        "provider {}: api_key.env or api_key.file is required",
                        p.name
                    )));
                }
            }
            if p.timeout_secs == 0 || p.timeout_secs > 3600 || p.max_retries > 10 {
                return Err(invalid(format!(
                    "provider {}: timeout or retry limits out of range",
                    p.name
                )));
            }
        }
        if defaults > 1 {
            return Err(invalid("at most one provider may be the default"));
        }
        for a in &self.agents {
            if !matches!(
                a.key.as_str(),
                "planner" | "worker" | "assistant" | "reviewer"
            ) {
                return Err(invalid(format!(
                    "unknown agent {}; agents are planner, worker, assistant, reviewer",
                    a.key
                )));
            }
            if let Some(profile) = &a.model_profile
                && !names.contains(profile.as_str())
            {
                return Err(invalid(format!(
                    "agent {}: unknown model profile {profile}",
                    a.key
                )));
            }
            if a.max_turns == Some(0) || a.max_turns.is_some_and(|t| t > 200) {
                return Err(invalid(format!(
                    "agent {}: max_turns must be 1..=200",
                    a.key
                )));
            }
        }
        if self.scheduler.lease_secs < 10
            || self.scheduler.max_concurrent_sessions == 0
            || self.scheduler.max_attempts == 0
        {
            return Err(invalid("scheduler limits out of range"));
        }
        if self.limits.max_record_body_bytes > self.limits.max_record_line_bytes {
            return Err(invalid(
                "limits.max_record_body_bytes cannot exceed max_record_line_bytes",
            ));
        }
        let t = &self.telegram;
        check_endpoint("telegram.api_base", &t.api_base)?;
        if t.enabled {
            if !t.bot_token.is_set() {
                return Err(invalid(
                    "telegram.bot_token (env or file) is required when enabled",
                ));
            }
            if t.chat_id.is_none() {
                return Err(invalid("telegram.chat_id is required when enabled"));
            }
            if t.max_attempts == 0 || t.max_attempts > 20 {
                return Err(invalid("telegram.max_attempts must be 1..=20"));
            }
        }
        Ok(())
    }

    pub fn default_provider(&self) -> Option<&ProviderConfig> {
        self.providers
            .iter()
            .find(|p| p.default)
            .or_else(|| self.providers.first())
    }

    pub fn statement_timeout(&self) -> Duration {
        Duration::from_secs(self.database.statement_timeout_secs)
    }

    /// A redacted, flattened view of the effective settings for display.
    pub fn redacted_entries(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let value = serde_json::to_value(self).unwrap_or_default();
        flatten("", &value, &mut out);
        out
    }
}

fn flatten(prefix: &str, value: &serde_json::Value, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&key, v, out);
            }
        }
        serde_json::Value::Array(items) if items.iter().any(|v| v.is_object()) => {
            for (i, v) in items.iter().enumerate() {
                flatten(&format!("{prefix}[{i}]"), v, out);
            }
        }
        other => out.push((
            prefix.to_owned(),
            match other {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => "-".into(),
                v => v.to_string(),
            },
        )),
    }
}

/// Default config file search order.
pub fn default_config_paths(env: &dyn Environment) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = env.var("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(dir).join("arustex/config.toml"));
    } else if let Some(home) = env.var("HOME") {
        paths.push(PathBuf::from(home).join(".config/arustex/config.toml"));
    }
    paths.push(PathBuf::from("/etc/arustex/config.toml"));
    paths
}

/// Default socket path: `$XDG_RUNTIME_DIR/arustex/arustex.sock`, else
/// `/tmp/arustex-<uid>/arustex.sock` (the directory is created with mode 0700).
pub fn default_socket(env: &dyn Environment) -> PathBuf {
    match env.var("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("arustex/arustex.sock"),
        None => {
            use std::os::unix::fs::MetadataExt;
            let uid = std::fs::metadata("/proc/self")
                .map(|m| m.uid())
                .unwrap_or(0);
            PathBuf::from(format!("/tmp/arustex-{uid}/arustex.sock"))
        }
    }
}

/// Loads the configuration file (if any), applies environment and command-line
/// overrides, validates, and resolves secrets.
pub fn load(
    cli: &CliOverrides,
    env: &dyn Environment,
    resolve_providers: bool,
) -> ConfigResult<Loaded> {
    let explicit = cli
        .config_path
        .clone()
        .or_else(|| env.var("ARUSTEX_CONFIG").map(PathBuf::from));
    let (mut config, source) = match explicit {
        Some(path) => {
            let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                path: path.clone(),
                source,
            })?;
            (Config::parse(&text, &path)?, Some(path))
        }
        None => match default_config_paths(env).into_iter().find(|p| p.is_file()) {
            Some(path) => {
                let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                    path: path.clone(),
                    source,
                })?;
                (Config::parse(&text, &path)?, Some(path))
            }
            None => (Config::default(), None),
        },
    };
    if let Some(level) = cli.log_level.clone().or_else(|| env.var("ARUSTEX_LOG")) {
        config.logging.level = level;
    }
    if let Some(socket) = cli
        .socket
        .clone()
        .or_else(|| env.var("ARUSTEX_SOCKET").map(PathBuf::from))
    {
        config.daemon.socket = Some(socket);
    }
    config.validate()?;

    let database_url = match (&cli.database_url, &config.database.url) {
        (Some(url), _) => Secret::new(url.clone()),
        (None, file_url) => match config
            .database
            .url_secret
            .resolve("database.url_secret", env)
        {
            Ok(secret) => secret,
            Err(err) => match file_url {
                Some(url) => Secret::new(url.clone()),
                None => return Err(err),
            },
        },
    };
    let socket = config
        .daemon
        .socket
        .clone()
        .unwrap_or_else(|| default_socket(env));
    let mut provider_keys = Vec::new();
    for p in &config.providers {
        let key = if p.kind == ProviderKind::Mock || !resolve_providers {
            None
        } else {
            Some(
                p.api_key
                    .resolve(&format!("providers.{}.api_key", p.name), env)?,
            )
        };
        provider_keys.push((p.name.clone(), key));
    }
    let telegram_token = if config.telegram.enabled {
        Some(
            config
                .telegram
                .bot_token
                .resolve("telegram.bot_token", env)?,
        )
    } else {
        None
    };
    Ok(Loaded {
        config,
        source,
        database_url,
        socket,
        provider_keys,
        telegram_token,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn example_config_is_valid() {
        let text = include_str!("../../../config.example.toml");
        let cfg = Config::parse(text, Path::new("config.example.toml")).unwrap();
        cfg.validate().unwrap();
        assert!(!cfg.telegram.enabled, "telegram is disabled by default");
    }

    #[test]
    fn rejects_unknown_keys_and_plaintext_secrets() {
        let err = Config::parse(
            "[[providers]]\nname='a'\nkind='anthropic'\nmodel='m'\napi_key='sk-1'\n",
            Path::new("x"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("api_key"), "{err}");
        assert!(Config::parse("[database]\npasword='x'\n", Path::new("x")).is_err());
        let cfg = Config::parse(
            "[database]\nurl='postgres://u:secret@h/db'\n",
            Path::new("x"),
        )
        .unwrap();
        assert!(cfg.validate().unwrap_err().to_string().contains("password"));
    }

    #[test]
    fn enforces_https_endpoints() {
        let base = "[[providers]]\nname='a'\nkind='openai_chat'\nmodel='m'\napi_key={env='K'}\n";
        let ok = Config::parse(
            &format!("{base}base_url='https://llm.example.com/v1'\n"),
            Path::new("x"),
        )
        .unwrap();
        ok.validate().unwrap();
        let local = Config::parse(
            &format!("{base}base_url='http://127.0.0.1:8080/v1'\n"),
            Path::new("x"),
        )
        .unwrap();
        local.validate().unwrap();
        let bad = Config::parse(
            &format!("{base}base_url='http://llm.example.com/v1'\n"),
            Path::new("x"),
        )
        .unwrap();
        assert!(bad.validate().is_err());
        let creds = Config::parse(
            &format!("{base}base_url='https://user:pw@llm.example.com'\n"),
            Path::new("x"),
        )
        .unwrap();
        assert!(creds.validate().is_err());
    }

    #[test]
    fn precedence_cli_over_env_over_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "c.toml",
            "[daemon]\nsocket='/file.sock'\n[logging]\nlevel='warn'\n",
        );
        let e = env(&[
            ("ARUSTEX_DATABASE_URL", "postgres://env/db"),
            ("ARUSTEX_SOCKET", "/env.sock"),
        ]);
        let cli = CliOverrides {
            config_path: Some(path.clone()),
            ..Default::default()
        };
        let loaded = load(&cli, &e, true).unwrap();
        assert_eq!(loaded.socket, PathBuf::from("/env.sock"));
        assert_eq!(loaded.database_url.expose(), "postgres://env/db");
        assert_eq!(loaded.config.logging.level, "warn");
        let cli = CliOverrides {
            config_path: Some(path),
            socket: Some("/cli.sock".into()),
            database_url: Some("postgres://cli/db".into()),
            log_level: Some("debug".into()),
        };
        let loaded = load(&cli, &e, true).unwrap();
        assert_eq!(loaded.socket, PathBuf::from("/cli.sock"));
        assert_eq!(loaded.database_url.expose(), "postgres://cli/db");
        assert_eq!(loaded.config.logging.level, "debug");
    }

    #[test]
    fn secret_files_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let key = write(dir.path(), "key", "sk-file\n");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let r = SecretRef {
            env: None,
            file: Some(key.clone()),
        };
        assert!(
            r.resolve("k", &env(&[]))
                .unwrap_err()
                .to_string()
                .contains("chmod 600")
        );
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(r.resolve("k", &env(&[])).unwrap().expose(), "sk-file");
        let both = SecretRef {
            env: Some("X".into()),
            file: Some(key),
        };
        assert!(both.resolve("k", &env(&[("X", "y")])).is_err());
    }

    #[test]
    fn telegram_requires_token_and_chat_when_enabled() {
        let cfg = Config::parse("[telegram]\nenabled=true\n", Path::new("x")).unwrap();
        assert!(cfg.validate().is_err());
        let cfg = Config::parse(
            "[telegram]\nenabled=true\nchat_id=42\nbot_token={env='TG'}\n",
            Path::new("x"),
        )
        .unwrap();
        cfg.validate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "c.toml",
            "[telegram]\nenabled=true\nchat_id=42\nbot_token={env='TG'}\n",
        );
        let cli = CliOverrides {
            config_path: Some(path),
            ..Default::default()
        };
        let loaded = load(
            &cli,
            &env(&[
                ("ARUSTEX_DATABASE_URL", "postgres://h/d"),
                ("TG", "123:abc"),
            ]),
            true,
        )
        .unwrap();
        assert_eq!(loaded.telegram_token.unwrap().expose(), "123:abc");
    }

    #[test]
    fn redacted_view_hides_secret_values() {
        let cfg = Config::parse(
            "[[providers]]\nname='a'\nkind='anthropic'\nmodel='m'\napi_key={env='ANTHROPIC_KEY'}\n",
            Path::new("x"),
        )
        .unwrap();
        let entries = cfg.redacted_entries();
        assert!(
            entries
                .iter()
                .any(|(k, v)| k == "providers[0].api_key.env" && v == "ANTHROPIC_KEY")
        );
        assert!(entries.iter().all(|(_, v)| !v.starts_with("sk-")));
    }
}
