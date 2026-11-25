//! Builds providers from operator settings and resolves them by name.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use arustex_core::Secret;

use crate::http::HttpSettings;
use crate::{
    AnthropicProvider, LlmError, LlmResult, MockProvider, OpenAiChatProvider,
    OpenAiResponsesProvider, Provider,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
    Mock,
}

/// Everything needed to construct one provider. Produced by the application from
/// validated configuration; workflows never construct these.
#[derive(Debug, Clone)]
pub struct ProviderSettings {
    pub name: String,
    pub kind: ProviderKind,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<Secret>,
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub max_retries: u32,
    pub requests_per_minute: u32,
    pub context_window: u32,
}

pub fn build_provider(s: ProviderSettings) -> LlmResult<Arc<dyn Provider>> {
    let http = |default: &str| HttpSettings {
        base_url: s.base_url.clone().unwrap_or_else(|| default.to_owned()),
        timeout: s.timeout,
        connect_timeout: s.connect_timeout,
        max_retries: s.max_retries,
        requests_per_minute: s.requests_per_minute,
        ..HttpSettings::default()
    };
    if s.kind != ProviderKind::Mock && s.api_key.as_ref().is_none_or(Secret::is_empty) {
        let local = s.base_url.as_deref().is_some_and(|u| {
            u.starts_with("http://127.0.0.1") || u.starts_with("http://localhost")
        });
        if !local {
            return Err(LlmError::Config(format!(
                "provider {} has no API key",
                s.name
            )));
        }
    }
    Ok(match s.kind {
        ProviderKind::OpenAiChat => Arc::new(OpenAiChatProvider::new(
            &s.name,
            &s.model,
            s.api_key.clone(),
            http("https://api.openai.com/v1"),
            s.context_window,
        )?),
        ProviderKind::OpenAiResponses => Arc::new(OpenAiResponsesProvider::new(
            &s.name,
            &s.model,
            s.api_key.clone(),
            http("https://api.openai.com/v1"),
            s.context_window,
        )?),
        ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(
            &s.name,
            &s.model,
            s.api_key.clone(),
            http("https://api.anthropic.com"),
            s.context_window,
        )?),
        ProviderKind::Mock => Arc::new(MockProvider::new(&s.name, &s.model)),
    })
}

/// The set of configured providers.
#[derive(Debug, Clone, Default)]
pub struct ProviderRouter {
    providers: BTreeMap<String, Arc<dyn Provider>>,
    default: Option<String>,
}

impl ProviderRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, provider: Arc<dyn Provider>, default: bool) {
        let name = provider.name().to_owned();
        if default || self.default.is_none() {
            self.default = Some(name.clone());
        }
        self.providers.insert(name, provider);
    }

    /// Resolves a named provider, or the default when `name` is `None`.
    pub fn get(&self, name: Option<&str>) -> LlmResult<Arc<dyn Provider>> {
        let key = name
            .or(self.default.as_deref())
            .ok_or_else(|| LlmError::Config("no model provider configured".into()))?;
        self.providers
            .get(key)
            .cloned()
            .ok_or_else(|| LlmError::Config(format!("unknown model provider {key:?}")))
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }

    pub fn default_name(&self) -> Option<&str> {
        self.default.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}
