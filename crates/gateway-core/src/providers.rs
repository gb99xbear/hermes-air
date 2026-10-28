// hermes-gateway-core - ProviderProfile and ProviderRegistry
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Provider profile base declaration - 1-to-1 exact parity with Python ProviderProfile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    // Identity
    pub name: String,
    #[serde(default = "default_api_mode")]
    pub api_mode: String,
    #[serde(default)]
    pub aliases: Vec<String>,

    // Human-readable metadata
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub signup_url: String,

    // Auth & endpoints
    #[serde(default)]
    pub env_vars: Vec<String>,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub models_url: String,
    #[serde(default = "default_auth_type")]
    pub auth_type: String,
    #[serde(default = "default_true")]
    pub supports_health_check: bool,
    #[serde(default = "default_true")]
    pub supports_model_listing: bool,

    // Vision support
    #[serde(default = "default_false")]
    pub supports_vision: bool,
    #[serde(default = "default_true")]
    pub supports_vision_tool_messages: bool,
    #[serde(default = "default_false")]
    pub supports_prompt_cache_key: bool,
    #[serde(default)]
    pub native_reasoning_details_type: Option<String>,

    // Model catalog & aliases
    #[serde(default)]
    pub fallback_models: Vec<String>,
    #[serde(default)]
    pub model_aliases: HashMap<String, String>,
    #[serde(default)]
    pub hostname: String,

    // Client & request quirks
    #[serde(default)]
    pub default_headers: HashMap<String, String>,
    #[serde(default)]
    pub default_max_tokens: Option<u32>,
    #[serde(default)]
    pub unsupported_response_formats: Vec<String>,
    #[serde(default)]
    pub default_aux_model: String,
}

fn default_api_mode() -> String {
    "chat_completions".to_string()
}

fn default_auth_type() -> String {
    "api_key".to_string()
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

impl Default for ProviderProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            api_mode: default_api_mode(),
            aliases: Vec::new(),
            display_name: String::new(),
            description: String::new(),
            signup_url: String::new(),
            env_vars: Vec::new(),
            base_url: String::new(),
            models_url: String::new(),
            auth_type: default_auth_type(),
            supports_health_check: true,
            supports_model_listing: true,
            supports_vision: false,
            supports_vision_tool_messages: true,
            supports_prompt_cache_key: false,
            native_reasoning_details_type: None,
            fallback_models: Vec::new(),
            model_aliases: HashMap::new(),
            hostname: String::new(),
            default_headers: HashMap::new(),
            default_max_tokens: None,
            unsupported_response_formats: Vec::new(),
            default_aux_model: String::new(),
        }
    }
}

impl ProviderProfile {
    /// Return the provider's base hostname for URL-based detection.
    /// Uses self.hostname if set explicitly, otherwise derives it from base_url.
    /// e.g. 'https://api.openai.com/v1' -> 'api.openai.com'
    /// 'http://100.75.163.92:20128/v1' -> '100.75.163.92'
    pub fn get_hostname(&self) -> String {
        if !self.hostname.trim().is_empty() {
            return self.hostname.trim().to_string();
        }
        if self.base_url.trim().is_empty() {
            return String::new();
        }

        let trimmed = self.base_url.trim();
        // Remove scheme if present
        let after_scheme = if let Some(idx) = trimmed.find("://") {
            &trimmed[idx + 3..]
        } else {
            trimmed
        };

        // Split by '/' or '?' or '#' to isolate host[:port]
        let host_port = after_scheme.split(&['/', '?', '#'][..]).next().unwrap_or("");

        // Split host from port (handle IPv6 in brackets if needed, or standard host:port)
        if host_port.starts_with('[') {
            if let Some(end_bracket) = host_port.find(']') {
                return host_port[1..end_bracket].to_string();
            }
        }

        let host = host_port.split(':').next().unwrap_or("");
        host.to_string()
    }

    /// Resolves an API endpoint path relative to .
    /// e.g. base_url 'https://api.openai.com/v1', endpoint 'chat/completions' -> 'https://api.openai.com/v1/chat/completions'
    pub fn resolve_api_url(&self, endpoint: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let ep = endpoint.trim_start_matches('/');
        if base.is_empty() {
            ep.to_string()
        } else if ep.is_empty() {
            base.to_string()
        } else {
            format!("{}/{}", base, ep)
        }
    }

    /// Resolves API key from the environment variables registered in .
    pub fn resolve_api_key(&self) -> Option<String> {
        for var in &self.env_vars {
            if let Ok(val) = std::env::var(var) {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
        None
    }
}

/// Thread-safe registry for ProviderProfiles.
#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    profiles: Arc<RwLock<HashMap<String, ProviderProfile>>>,
    aliases: Arc<RwLock<HashMap<String, String>>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    /// Creates a new ProviderRegistry populated with standard Hermes built-in profiles.
    pub fn new() -> Self {
        let registry = Self {
            profiles: Arc::new(RwLock::new(HashMap::new())),
            aliases: Arc::new(RwLock::new(HashMap::new())),
        };

        registry.register_defaults();
        registry
    }

    /// Register the standard Hermes built-in providers:
    /// 9router, openai, openrouter, anthropic, gemini, deepseek.
    fn register_defaults(&self) {
        // 1. 9router
        self.register_sync(ProviderProfile {
            name: "9router".to_string(),
            api_mode: "chat_completions".to_string(),
            aliases: vec![
                "ninerouter".to_string(),
                "nine-router".to_string(),
                "router9".to_string(),
                "router-9".to_string(),
                "9router-oracle".to_string(),
            ],
            display_name: "9Router Gateway".to_string(),
            description: "9Router AI Gateway - OpenAI-compatible inference with live model catalog".to_string(),
            signup_url: "http://100.75.163.92:20128/".to_string(),
            env_vars: vec![
                "HERMES_CUSTOM_100_75_163_92_20128_API_KEY".to_string(),
                "HERMES_CUSTOM_100_77_223_11_20128_API_KEY".to_string(),
                "NINEROUTER_API_KEY".to_string(),
                "NINE_ROUTER_API_KEY".to_string(),
                "ROUTER9_API_KEY".to_string(),
                "ROUTER_9_API_KEY".to_string(),
                "OPENAI_API_KEY".to_string(),
            ],
            base_url: "http://100.75.163.92:20128/v1".to_string(),
            models_url: "http://100.75.163.92:20128/v1/models".to_string(),
            auth_type: "api_key".to_string(),
            supports_health_check: true,
            supports_model_listing: true,
            supports_vision: true,
            supports_vision_tool_messages: true,
            supports_prompt_cache_key: false,
            native_reasoning_details_type: None,
            fallback_models: vec!["HermesCombo".to_string()],
            model_aliases: HashMap::new(),
            hostname: "100.75.163.92".to_string(),
            default_headers: HashMap::new(),
            default_max_tokens: None,
            unsupported_response_formats: Vec::new(),
            default_aux_model: "HermesCombo".to_string(),
        });

        // 2. openai
        self.register_sync(ProviderProfile {
            name: "openai".to_string(),
            api_mode: "chat_completions".to_string(),
            aliases: vec!["chatgpt".to_string()],
            display_name: "OpenAI".to_string(),
            description: "OpenAI direct API".to_string(),
            signup_url: "https://platform.openai.com/".to_string(),
            env_vars: vec!["OPENAI_API_KEY".to_string()],
            base_url: "https://api.openai.com/v1".to_string(),
            models_url: "https://api.openai.com/v1/models".to_string(),
            auth_type: "api_key".to_string(),
            supports_health_check: true,
            supports_model_listing: true,
            supports_vision: true,
            supports_vision_tool_messages: true,
            supports_prompt_cache_key: true,
            native_reasoning_details_type: None,
            fallback_models: vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()],
            model_aliases: HashMap::new(),
            hostname: "api.openai.com".to_string(),
            default_headers: HashMap::new(),
            default_max_tokens: None,
            unsupported_response_formats: Vec::new(),
            default_aux_model: "gpt-4o-mini".to_string(),
        });

        // 3. openrouter
        self.register_sync(ProviderProfile {
            name: "openrouter".to_string(),
            api_mode: "chat_completions".to_string(),
            aliases: vec!["or".to_string()],
            display_name: "OpenRouter".to_string(),
            description: "OpenRouter - unified API for 200+ models".to_string(),
            signup_url: "https://openrouter.ai/keys".to_string(),
            env_vars: vec!["OPENROUTER_API_KEY".to_string()],
            base_url: "https://openrouter.ai/api/v1".to_string(),
            models_url: "https://openrouter.ai/api/v1/models".to_string(),
            auth_type: "api_key".to_string(),
            supports_health_check: true,
            supports_model_listing: true,
            supports_vision: false,
            supports_vision_tool_messages: true,
            supports_prompt_cache_key: false,
            native_reasoning_details_type: None,
            fallback_models: vec![
                "anthropic/claude-sonnet-4.6".to_string(),
                "openai/gpt-5.4".to_string(),
                "deepseek/deepseek-chat".to_string(),
                "google/gemini-3.8-flash".to_string(),
                "google/gemini-3.7-flash".to_string(),
            ],
            model_aliases: HashMap::new(),
            hostname: "openrouter.ai".to_string(),
            default_headers: HashMap::new(),
            default_max_tokens: None,
            unsupported_response_formats: Vec::new(),
            default_aux_model: String::new(),
        });

        // 4. anthropic
        self.register_sync(ProviderProfile {
            name: "anthropic".to_string(),
            api_mode: "anthropic_messages".to_string(),
            aliases: vec![
                "claude".to_string(),
                "claude-oauth".to_string(),
                "claude-code".to_string(),
            ],
            display_name: "Anthropic".to_string(),
            description: "Anthropic Messages API".to_string(),
            signup_url: "https://console.anthropic.com/".to_string(),
            env_vars: vec![
                "ANTHROPIC_API_KEY".to_string(),
                "ANTHROPIC_TOKEN".to_string(),
                "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            ],
            base_url: "https://api.anthropic.com/v1".to_string(),
            models_url: "https://api.anthropic.com/v1/models".to_string(),
            auth_type: "api_key".to_string(),
            supports_health_check: true,
            supports_model_listing: true,
            supports_vision: false,
            supports_vision_tool_messages: true,
            supports_prompt_cache_key: false,
            native_reasoning_details_type: None,
            fallback_models: vec![
                "claude-3-5-sonnet-20241022".to_string(),
                "claude-3-5-haiku-20241022".to_string(),
            ],
            model_aliases: HashMap::new(),
            hostname: "api.anthropic.com".to_string(),
            default_headers: HashMap::new(),
            default_max_tokens: None,
            unsupported_response_formats: Vec::new(),
            default_aux_model: "claude-haiku-4-5-20251001".to_string(),
        });

        // 5. gemini
        self.register_sync(ProviderProfile {
            name: "gemini".to_string(),
            api_mode: "chat_completions".to_string(),
            aliases: vec![
                "google".to_string(),
                "google-gemini".to_string(),
                "google-ai-studio".to_string(),
            ],
            display_name: "Google Gemini".to_string(),
            description: "Google AI Studio Gemini API".to_string(),
            signup_url: "https://aistudio.google.com/".to_string(),
            env_vars: vec![
                "GEMINI_API_KEY".to_string(),
                "GOOGLE_API_KEY".to_string(),
            ],
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            models_url: "https://generativelanguage.googleapis.com/v1beta/openai/models".to_string(),
            auth_type: "api_key".to_string(),
            supports_health_check: true,
            supports_model_listing: true,
            supports_vision: false,
            supports_vision_tool_messages: true,
            supports_prompt_cache_key: false,
            native_reasoning_details_type: None,
            fallback_models: vec![
                "gemini-2.5-flash".to_string(),
                "gemini-2.5-pro".to_string(),
            ],
            model_aliases: HashMap::new(),
            hostname: "generativelanguage.googleapis.com".to_string(),
            default_headers: HashMap::new(),
            default_max_tokens: None,
            unsupported_response_formats: Vec::new(),
            default_aux_model: "gemini-3.6-flash".to_string(),
        });

        // 6. deepseek
        self.register_sync(ProviderProfile {
            name: "deepseek".to_string(),
            api_mode: "chat_completions".to_string(),
            aliases: vec!["deepseek-chat".to_string(), "deep-seek".to_string()],
            display_name: "DeepSeek".to_string(),
            description: "DeepSeek - native DeepSeek API".to_string(),
            signup_url: "https://platform.deepseek.com/".to_string(),
            env_vars: vec!["DEEPSEEK_API_KEY".to_string()],
            base_url: "https://api.deepseek.com/v1".to_string(),
            models_url: "https://api.deepseek.com/v1/models".to_string(),
            auth_type: "api_key".to_string(),
            supports_health_check: true,
            supports_model_listing: true,
            supports_vision: false,
            supports_vision_tool_messages: true,
            supports_prompt_cache_key: false,
            native_reasoning_details_type: None,
            fallback_models: vec!["deepseek-v4-pro".to_string(), "deepseek-flash".to_string()],
            model_aliases: HashMap::new(),
            hostname: "api.deepseek.com".to_string(),
            default_headers: HashMap::new(),
            default_max_tokens: None,
            unsupported_response_formats: vec!["json_schema".to_string()],
            default_aux_model: "deepseek-flash".to_string(),
        });
    }

    fn register_sync(&self, profile: ProviderProfile) {
        if let Ok(mut profiles) = self.profiles.try_write() {
            if let Ok(mut aliases) = self.aliases.try_write() {
                for alias in &profile.aliases {
                    aliases.insert(alias.to_lowercase(), profile.name.clone());
                }
                profiles.insert(profile.name.to_lowercase(), profile);
            }
        }
    }

    /// Registers or overrides a ProviderProfile asynchronously.
    pub async fn register(&self, profile: ProviderProfile) {
        let mut aliases = self.aliases.write().await;
        let mut profiles = self.profiles.write().await;

        for alias in &profile.aliases {
            aliases.insert(alias.to_lowercase(), profile.name.clone());
        }
        profiles.insert(profile.name.to_lowercase(), profile);
    }

    /// Looks up a provider profile by canonical name or alias.
    pub async fn get(&self, name_or_alias: &str) -> Option<ProviderProfile> {
        let key = name_or_alias.trim().to_lowercase();
        let aliases = self.aliases.read().await;
        let canonical = aliases.get(&key).cloned().unwrap_or_else(|| key.clone());
        drop(aliases);

        let profiles = self.profiles.read().await;
        profiles.get(&canonical).cloned()
    }

    /// Synchronous lookup (useful when outside async runtime or non-blocking caller).
    pub fn get_blocking(&self, name_or_alias: &str) -> Option<ProviderProfile> {
        let key = name_or_alias.trim().to_lowercase();
        let aliases = self.aliases.blocking_read();
        let canonical = aliases.get(&key).cloned().unwrap_or_else(|| key.clone());
        drop(aliases);

        let profiles = self.profiles.blocking_read();
        profiles.get(&canonical).cloned()
    }

    /// Lists all canonical provider profiles.
    pub async fn list(&self) -> Vec<ProviderProfile> {
        let profiles = self.profiles.read().await;
        profiles.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_hostname() {
        let mut profile = ProviderProfile::default();
        profile.base_url = "http://100.75.163.92:20128/v1".to_string();
        assert_eq!(profile.get_hostname(), "100.75.163.92");

        profile.base_url = "https://api.openai.com/v1".to_string();
        assert_eq!(profile.get_hostname(), "api.openai.com");

        profile.base_url = "http://localhost:8080/v1".to_string();
        assert_eq!(profile.get_hostname(), "localhost");

        // Explicit hostname takes precedence
        profile.hostname = "custom.hostname.io".to_string();
        assert_eq!(profile.get_hostname(), "custom.hostname.io");

        // Bracketed IPv6 address
        profile.hostname = String::new();
        profile.base_url = "http://[::1]:8080/v1".to_string();
        assert_eq!(profile.get_hostname(), "::1");
    }

    #[test]
    fn test_resolve_api_url() {
        let mut profile = ProviderProfile::default();
        profile.base_url = "https://api.openai.com/v1".to_string();
        assert_eq!(
            profile.resolve_api_url("chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            profile.resolve_api_url("/models"),
            "https://api.openai.com/v1/models"
        );
    }

    #[tokio::test]
    async fn test_provider_registration_and_alias_lookup() {
        let registry = ProviderRegistry::new();

        // Check pre-registered 9router
        let profile_9r = registry.get("9router").await.expect("9router must be registered");
        assert_eq!(profile_9r.name, "9router");
        assert_eq!(profile_9r.base_url, "http://100.75.163.92:20128/v1");

        // Alias lookup
        let by_alias = registry.get("router9").await.expect("alias router9 must resolve");
        assert_eq!(by_alias.name, "9router");

        // Case insensitivity
        let by_case = registry.get("9ROUTER-ORACLE").await.expect("case-insensitive alias lookup");
        assert_eq!(by_case.name, "9router");

        // Custom registration
        let custom = ProviderProfile {
            name: "custom-corp".to_string(),
            aliases: vec!["cc".to_string()],
            base_url: "https://api.custom.corp/v1".to_string(),
            ..Default::default()
        };
        registry.register(custom).await;

        let resolved_custom = registry.get("cc").await.expect("custom alias cc must resolve");
        assert_eq!(resolved_custom.name, "custom-corp");
        assert_eq!(resolved_custom.base_url, "https://api.custom.corp/v1");
    }

    #[tokio::test]
    async fn test_default_builtin_providers_catalog() {
        let registry = ProviderRegistry::new();
        let expected = vec!["9router", "openai", "openrouter", "anthropic", "gemini", "deepseek"];

        for name in expected {
            let p = registry.get(name).await.unwrap_or_else(|| panic!("Missing default provider {}", name));
            assert_eq!(p.name, name);
            assert!(!p.base_url.is_empty(), "Provider {} should have a base_url", name);
            assert!(!p.env_vars.is_empty(), "Provider {} should have env_vars", name);
        }
    }
}
