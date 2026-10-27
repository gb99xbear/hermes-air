// hermes-platform-adapter - Platform adapter trait and base implementations
use hermes_protocol::{
    Platform, PlatformEvent, PlatformError, DeliveryReceipt,
    IncomingMessage, OutgoingMessage,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use tokio::sync::mpsc;
use dashmap::DashMap;

/// Base configuration for all platform adapters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BasePlatformConfig {
    pub enabled: bool,
    pub rate_limit: Option<RateLimitConfig>,
    pub webhook: Option<WebhookConfig>,
    pub custom: serde_json::Value,
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

/// Platform adapter trait - all platform adapters must implement this
#[async_trait]
pub trait PlatformAdapter: Send + Sync + Debug {
    type Config: Send + Sync + 'static + Debug;
    type IncomingEvent: Send + Sync + 'static + Debug;
    type OutgoingMessage: Send + Sync + 'static + Debug;

    /// Returns the platform this adapter handles
    fn platform(&self) -> Platform;

    /// Initialize the adapter with configuration
    async fn initialize(&mut self, config: Self::Config) -> Result<(), PlatformError>;

    /// Start the adapter, connecting to platform APIs
    /// sender is used to send PlatformEvent::Incoming to the gateway core
    async fn start(&mut self, sender: mpsc::UnboundedSender<PlatformEvent>) -> Result<(), PlatformError>;

    /// Stop the adapter gracefully
    async fn stop(&mut self) -> Result<(), PlatformError>;

    /// Send an outgoing message to the platform
    async fn send(&self, msg: Self::OutgoingMessage) -> Result<DeliveryReceipt, PlatformError>;

    /// Handle an incoming webhook/event from the platform
    async fn handle_webhook(&self, payload: serde_json::Value) -> Result<Vec<IncomingMessage>, PlatformError>;

    /// Get adapter name
    fn name(&self) -> &'static str { self.platform().as_str() }

    /// Check if adapter is healthy
    async fn health_check(&self) -> Result<(), PlatformError> { Ok(()) }
}

/// Type-erased platform adapter for registry - using async_trait for dyn compatibility
#[async_trait]
pub trait DynPlatformAdapter: Send + Sync + Debug {
    fn platform(&self) -> Platform;
    async fn start(&mut self, sender: mpsc::UnboundedSender<PlatformEvent>) -> Result<(), PlatformError>;
    async fn stop(&mut self) -> Result<(), PlatformError>;
    async fn send(&self, msg: OutgoingMessage) -> Result<DeliveryReceipt, PlatformError>;
    async fn handle_webhook(&self, payload: serde_json::Value) -> Result<Vec<IncomingMessage>, PlatformError>;
    fn name(&self) -> &'static str;
    async fn health_check(&self) -> Result<(), PlatformError>;
}

/// Blanket implementation for any concrete adapter
#[async_trait]
impl<A> DynPlatformAdapter for A
where
    A: PlatformAdapter<Config = serde_json::Value, IncomingEvent = IncomingMessage, OutgoingMessage = OutgoingMessage> + 'static,
{
    fn platform(&self) -> Platform {
        PlatformAdapter::platform(self)
    }
    
    async fn start(&mut self, sender: mpsc::UnboundedSender<PlatformEvent>) -> Result<(), PlatformError> {
        PlatformAdapter::start(self, sender).await
    }
    
    async fn stop(&mut self) -> Result<(), PlatformError> {
        PlatformAdapter::stop(self).await
    }
    
    async fn send(&self, msg: OutgoingMessage) -> Result<DeliveryReceipt, PlatformError> {
        PlatformAdapter::send(self, msg).await
    }
    
    async fn handle_webhook(&self, payload: serde_json::Value) -> Result<Vec<IncomingMessage>, PlatformError> {
        PlatformAdapter::handle_webhook(self, payload).await
    }
    
    fn name(&self) -> &'static str {
        PlatformAdapter::name(self)
    }
    
    async fn health_check(&self) -> Result<(), PlatformError> {
        PlatformAdapter::health_check(self).await
    }
}

/// Webhook handler trait for HTTP-based platforms
#[async_trait]
pub trait WebhookHandler: Send + Sync {
    async fn handle(&self, path: &str, headers: &http::HeaderMap, body: bytes::Bytes) 
        -> Result<Vec<IncomingMessage>, PlatformError>;
}

/// Rate limiter for platform APIs
#[derive(Debug)]
pub struct RateLimiter {
    permits: std::sync::Arc<tokio::sync::Semaphore>,
    #[allow(dead_code)]
    refill_rate: u32,
    #[allow(dead_code)]
    refill_interval: std::time::Duration,
}

impl RateLimiter {
    pub fn new(requests_per_second: u32, burst: u32) -> Self {
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(burst as usize));
        let refill_rate = requests_per_second;
        let refill_interval = std::time::Duration::from_secs(1);
        
        let permits_clone = permits.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(refill_interval);
            loop {
                interval.tick().await;
                let current = permits_clone.available_permits();
                let to_add = (refill_rate as usize).saturating_sub(current);
                for _ in 0..to_add {
                    permits_clone.add_permits(1);
                }
            }
        });
        
        Self { permits, refill_rate, refill_interval }
    }

    pub async fn acquire(&self) -> Result<(), PlatformError> {
        let _ = self.permits.acquire().await
            .map_err(|_| PlatformError::Internal("Rate limiter closed".into()))?;
        Ok(())
    }

    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }
}

/// Base adapter implementation with common functionality
#[derive(Debug)]
pub struct BaseAdapter<C: Send + Sync + 'static + Debug> {
    pub platform: Platform,
    pub config: Option<C>,
    pub rate_limiter: Option<RateLimiter>,
    pub sender: Option<mpsc::UnboundedSender<PlatformEvent>>,
    pub started: bool,
}

impl<C: Send + Sync + 'static + Debug> BaseAdapter<C> {
    pub fn new(platform: Platform) -> Self {
        Self {
            platform,
            config: None,
            rate_limiter: None,
            sender: None,
            started: false,
        }
    }

    pub fn with_config(mut self, config: C) -> Self {
        self.config = Some(config);
        self
    }

    pub fn setup_rate_limit(&mut self, rate_limit: Option<RateLimitConfig>) {
        if let Some(rl) = rate_limit {
            self.rate_limiter = Some(RateLimiter::new(rl.requests_per_second, rl.burst));
        }
    }

    pub fn set_sender(&mut self, sender: mpsc::UnboundedSender<PlatformEvent>) {
        self.sender = Some(sender);
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub async fn emit_incoming(&self, msg: IncomingMessage) -> Result<(), PlatformError> {
        if let Some(sender) = &self.sender {
            sender.send(PlatformEvent::Incoming(msg))
                .map_err(|_| PlatformError::Internal("Event channel closed".into()))?;
        }
        Ok(())
    }

    pub async fn emit_error(&self, session_id: uuid::Uuid, error: PlatformError) -> Result<(), PlatformError> {
        if let Some(sender) = &self.sender {
            sender.send(PlatformEvent::Error { session_id, error })
                .map_err(|_| PlatformError::Internal("Event channel closed".into()))?;
        }
        Ok(())
    }
}

/// Adapter registry for managing multiple platform adapters
pub struct AdapterRegistry {
    adapters: DashMap<Platform, Box<dyn DynPlatformAdapter>>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self { adapters: DashMap::new() }
    }

    pub fn register<A>(&self, adapter: A) 
    where 
        A: PlatformAdapter<Config = serde_json::Value, IncomingEvent = IncomingMessage, OutgoingMessage = OutgoingMessage> + 'static,
    {
        let platform = adapter.platform();
        self.adapters.insert(platform, Box::new(adapter));
    }

    pub fn get(&self, platform: Platform) -> Option<dashmap::mapref::one::Ref<'_, Platform, Box<dyn DynPlatformAdapter>>> {
        self.adapters.get(&platform)
    }

    pub fn remove(&self, platform: Platform) -> bool {
        self.adapters.remove(&platform).is_some()
    }

    pub fn list(&self) -> Vec<Platform> {
        self.adapters.iter().map(|e| *e.key()).collect()
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}
