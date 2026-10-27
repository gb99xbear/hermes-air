// hermes-config - Configuration management with hot-reload
use serde::{Deserialize, Serialize};
use serde_json::Map;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use notify::{RecommendedWatcher, Watcher, RecursiveMode, Event};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub gateway: GatewaySection,
    pub profiles: HashMap<String, ProfileConfig>,
    pub platforms: HashMap<String, PlatformConfig>,
    pub plugins: PluginsConfig,
    pub session_store: SessionStoreConfig,
    pub logging: LoggingConfig,
    pub metrics: MetricsConfig,
    pub dashboard: DashboardConfig,
    pub tui: TuiConfig,
    pub acp: AcpConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySection {
    pub name: String,
    pub version: String,
    pub max_concurrent_sessions: usize,
    pub default_profile: String,
    pub shutdown_timeout_secs: u64,
    pub restart_on_config_change: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub name: String,
    pub model_provider: String,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub system_prompt: Option<String>,
    pub tools: Vec<String>,
    pub skills: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub platform_overrides: HashMap<String, Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformConfig {
    pub enabled: bool,
    pub adapter: String,
    pub config: Map<String, serde_json::Value>,
    pub rate_limit: Option<RateLimitConfig>,
    pub webhook: Option<WebhookConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub requests_per_second: u32,
    pub burst: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    pub path: String,
    pub secret: Option<String>,
    pub verify_signature: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginsConfig {
    pub enabled: bool,
    pub directory: PathBuf,
    pub wasm_directory: Option<PathBuf>,
    pub python_plugins: bool,
    pub allowed_plugins: Vec<String>,
    pub blocked_plugins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStoreConfig {
    pub backend: SessionStoreBackend,
    pub redis: Option<RedisConfig>,
    pub local_cache_size: usize,
    pub default_ttl_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStoreBackend {
    Redis,
    Memory,
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisConfig {
    pub url: String,
    pub pool_size: u32,
    pub connection_timeout_secs: u64,
    pub command_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
    pub output: LogOutput,
    pub file: Option<PathBuf>,
    pub max_file_size_mb: u64,
    pub max_files: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    Json,
    Text,
    Compact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogOutput {
    Stdout,
    Stderr,
    File,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub port: u16,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub auth: Option<DashboardAuthConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardAuthConfig {
    pub r#type: String,
    pub config: Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    pub enabled: bool,
    pub socket_path: Option<PathBuf>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpConfig {
    pub enabled: bool,
    pub port: u16,
}

/// Configuration loader with hot-reload support
#[allow(dead_code)]
pub struct ConfigLoader {
    config: Arc<RwLock<GatewayConfig>>,
    watcher: Option<RecommendedWatcher>,
    config_path: PathBuf,
}

impl ConfigLoader {
    pub fn load<P: AsRef<std::path::Path>>(path: P) -> anyhow::Result<GatewayConfig> {
        let content = std::fs::read_to_string(&path)?;
        let config: GatewayConfig = if path.as_ref().extension().and_then(|s| s.to_str()) == Some("yaml") || 
            path.as_ref().extension().and_then(|s| s.to_str()) == Some("yml") {
            serde_yaml::from_str(&content)?
        } else {
            toml::from_str(&content)?
        };
        Ok(config)
    }

    pub async fn load_with_watch<P: AsRef<std::path::Path>>(path: P) -> anyhow::Result<Self> {
        let config = Self::load(&path)?;
        let config_path = path.as_ref().to_path_buf();
        
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = tx.try_send(event);
                }
            },
            notify::Config::default(),
        )?;
        
        watcher.watch(&config_path, RecursiveMode::NonRecursive)?;
        
        let config_arc = Arc::new(RwLock::new(config));
        let config_clone = config_arc.clone();
        let path_clone = config_path.clone();
        
        tokio::spawn(async move {
            while let Some(_event) = rx.recv().await {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                if let Ok(new_config) = Self::load(&path_clone) {
                    let mut guard = config_clone.write().await;
                    *guard = new_config;
                    tracing::info!("Configuration reloaded from {}", path_clone.display());
                }
            }
        });
        
        Ok(Self {
            config: config_arc,
            watcher: Some(watcher),
            config_path,
        })
    }

    pub async fn get(&self) -> Arc<RwLock<GatewayConfig>> {
        self.config.clone()
    }

    pub async fn get_snapshot(&self) -> GatewayConfig {
        self.config.read().await.clone()
    }
}

impl GatewayConfig {
    pub fn get_profile(&self, name: &str) -> Option<&ProfileConfig> {
        self.profiles.get(name)
    }

    pub fn get_platform(&self, name: &str) -> Option<&PlatformConfig> {
        self.platforms.get(name)
    }

    pub fn enabled_platforms(&self) -> Vec<&PlatformConfig> {
        self.platforms.values().filter(|p| p.enabled).collect()
    }
}
