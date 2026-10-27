// hermes-session-store - Session persistence with Redis backend
use hermes_protocol::{Session, SessionState, Platform, Uuid};
use async_trait::async_trait;
use dashmap::DashMap;
use redis::{Client, aio::ConnectionManager, AsyncCommands, RedisError};

use std::sync::Arc;
use std::time::Duration;

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn get(&self, id: Uuid) -> Result<Option<Session>, SessionStoreError>;
    async fn set(&self, session: &Session) -> Result<(), SessionStoreError>;
    async fn delete(&self, id: Uuid) -> Result<(), SessionStoreError>;
    async fn list_by_profile(&self, profile: &str) -> Result<Vec<Session>, SessionStoreError>;
    async fn list_by_platform(&self, platform: Platform) -> Result<Vec<Session>, SessionStoreError>;
    async fn list_active(&self) -> Result<Vec<Session>, SessionStoreError>;
    async fn cleanup_expired(&self, ttl_secs: u64) -> Result<usize, SessionStoreError>;
    async fn health_check(&self) -> Result<(), SessionStoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    #[error("Redis error: {0}")]
    Redis(#[from] RedisError),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Session not found: {0}")]
    NotFound(Uuid),
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Internal error: {0}")]
    Internal(String),
}

#[derive(Clone)]
pub struct RedisSessionStore {
    conn: ConnectionManager,
    local_cache: Arc<DashMap<Uuid, Session>>,
    max_cache_size: usize,
    key_prefix: String,
    default_ttl: Duration,
}

impl RedisSessionStore {
    pub async fn new(
        redis_url: &str,
        _pool_size: u32,
        max_cache_size: usize,
        default_ttl_secs: u64,
        key_prefix: Option<String>,
    ) -> Result<Self, SessionStoreError> {
        let client = Client::open(redis_url)?;
        let conn = ConnectionManager::new(client).await?;
        
        Ok(Self {
            conn,
            local_cache: Arc::new(DashMap::new()),
            max_cache_size,
            key_prefix: key_prefix.unwrap_or_else(|| "hermes:session:".to_string()),
            default_ttl: Duration::from_secs(default_ttl_secs),
        })
    }

    fn key(&self, id: Uuid) -> String {
        format!("{}{}", self.key_prefix, id)
    }

    fn profile_index_key(&self, profile: &str) -> String {
        format!("{}profile:{}", self.key_prefix, profile)
    }

    fn platform_index_key(&self, platform: Platform) -> String {
        format!("{}platform:{}", self.key_prefix, platform.as_str())
    }

    async fn evict_if_needed(&self) {
        if self.local_cache.len() >= self.max_cache_size {
            // Simple LRU: remove oldest 10%
            let to_remove: Vec<_> = self.local_cache.iter()
                .take(self.max_cache_size / 10)
                .map(|entry| *entry.key())
                .collect();
            for key in to_remove {
                self.local_cache.remove(&key);
            }
        }
    }
}

#[async_trait]
impl SessionStore for RedisSessionStore {
    async fn get(&self, id: Uuid) -> Result<Option<Session>, SessionStoreError> {
        // Try local cache first
        if let Some(session) = self.local_cache.get(&id) {
            return Ok(Some(session.clone()));
        }

        // Fetch from Redis
        let mut conn = self.conn.clone();
        let key = self.key(id);
        let data: Option<String> = conn.get(&key).await?;
        
        match data {
            Some(json) => {
                let session: Session = serde_json::from_str(&json)?;
                self.evict_if_needed().await;
                self.local_cache.insert(id, session.clone());
                Ok(Some(session))
            }
            None => Ok(None),
        }
    }

    async fn set(&self, session: &Session) -> Result<(), SessionStoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(session.id);
        let json = serde_json::to_string(session)?;
        
        // Store in Redis with TTL
        conn.set_ex::<_, _, ()>(&key, json, self.default_ttl.as_secs()).await?;
        
        // Update indexes
        let profile_key = self.profile_index_key(&session.profile);
        let platform_key = self.platform_index_key(session.platform);
        
        let _: () = conn.sadd(&profile_key, session.id.to_string()).await?;
        let _: () = conn.sadd(&platform_key, session.id.to_string()).await?;
        let _: () = conn.expire(&profile_key, self.default_ttl.as_secs() as i64).await?;
        let _: () = conn.expire(&platform_key, self.default_ttl.as_secs() as i64).await?;
        
        // Update local cache
        self.evict_if_needed().await;
        self.local_cache.insert(session.id, session.clone());
        
        Ok(())
    }

    async fn delete(&self, id: Uuid) -> Result<(), SessionStoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(id);
        
        // Get session first to update indexes
        if let Some(session) = self.get(id).await? {
            let profile_key = self.profile_index_key(&session.profile);
            let platform_key = self.platform_index_key(session.platform);
            
            let _: () = conn.srem(&profile_key, id.to_string()).await?;
            let _: () = conn.srem(&platform_key, id.to_string()).await?;
        }
        
        conn.del::<_, ()>(&key).await?;
        self.local_cache.remove(&id);
        
        Ok(())
    }

    async fn list_by_profile(&self, profile: &str) -> Result<Vec<Session>, SessionStoreError> {
        let mut conn = self.conn.clone();
        let profile_key = self.profile_index_key(profile);
        let ids: Vec<String> = conn.smembers(&profile_key).await?;
        
        let mut sessions = Vec::new();
        for id_str in ids {
            if let Ok(id) = Uuid::parse_str(&id_str) {
                if let Some(session) = self.get(id).await? {
                    sessions.push(session);
                }
            }
        }
        Ok(sessions)
    }

    async fn list_by_platform(&self, platform: Platform) -> Result<Vec<Session>, SessionStoreError> {
        let mut conn = self.conn.clone();
        let platform_key = self.platform_index_key(platform);
        let ids: Vec<String> = conn.smembers(&platform_key).await?;
        
        let mut sessions = Vec::new();
        for id_str in ids {
            if let Ok(id) = Uuid::parse_str(&id_str) {
                if let Some(session) = self.get(id).await? {
                    sessions.push(session);
                }
            }
        }
        Ok(sessions)
    }

    async fn list_active(&self) -> Result<Vec<Session>, SessionStoreError> {
        let mut conn = self.conn.clone();
        let pattern = format!("{}*", self.key_prefix);
        let keys: Vec<String> = conn.keys(&pattern).await?;
        
        let mut sessions = Vec::new();
        for key in keys {
            if let Some(id_str) = key.strip_prefix(&self.key_prefix) {
                if let Ok(id) = Uuid::parse_str(id_str) {
                    if let Some(session) = self.get(id).await? {
                        if session.state == SessionState::Active {
                            sessions.push(session);
                        }
                    }
                }
            }
        }
        Ok(sessions)
    }

    async fn cleanup_expired(&self, _ttl_secs: u64) -> Result<usize, SessionStoreError> {
        // Redis handles TTL automatically, but we can clean up indexes
        let mut conn = self.conn.clone();
        let pattern = format!("{}profile:*", self.key_prefix);
        let keys: Vec<String> = conn.keys(&pattern).await?;
        
        let mut cleaned = 0;
        for key in keys {
            let _profile = key.strip_prefix(&format!("{}profile:", self.key_prefix)).unwrap_or("");
            let ids: Vec<String> = conn.smembers(&key).await?;
            
            for id_str in ids {
                if let Ok(id) = Uuid::parse_str(&id_str) {
                    let session_key = self.key(id);
                    let exists: bool = conn.exists(&session_key).await?;
                    if !exists {
                        let _: () = conn.srem(&key, &id_str).await?;
                        cleaned += 1;
                    }
                }
            }
        }
        
        // Also clean local cache
        let expired_keys: Vec<_> = self.local_cache.iter()
            .filter(|entry| entry.value().state == SessionState::Terminated)
            .map(|entry| *entry.key())
            .collect();
        for key in expired_keys {
            self.local_cache.remove(&key);
        }
        
        Ok(cleaned)
    }

    async fn health_check(&self) -> Result<(), SessionStoreError> {
        let mut conn = self.conn.clone();
        let _: String = redis::cmd("PING").query_async(&mut conn).await?;
        Ok(())
    }
}

/// In-memory session store (for testing or single-instance)
pub struct MemorySessionStore {
    sessions: Arc<DashMap<Uuid, Session>>,
    profile_index: Arc<DashMap<String, Vec<Uuid>>>,
    platform_index: Arc<DashMap<Platform, Vec<Uuid>>>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            profile_index: Arc::new(DashMap::new()),
            platform_index: Arc::new(DashMap::new()),
        }
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn get(&self, id: Uuid) -> Result<Option<Session>, SessionStoreError> {
        Ok(self.sessions.get(&id).map(|v| v.clone()))
    }

    async fn set(&self, session: &Session) -> Result<(), SessionStoreError> {
        self.sessions.insert(session.id, session.clone());
        self.profile_index.entry(session.profile.clone()).or_default().push(session.id);
        self.platform_index.entry(session.platform).or_default().push(session.id);
        Ok(())
    }

    async fn delete(&self, id: Uuid) -> Result<(), SessionStoreError> {
        if let Some((_, session)) = self.sessions.remove(&id) {
            if let Some(mut vec) = self.profile_index.get_mut(&session.profile) {
                vec.retain(|&x| x != id);
            }
            if let Some(mut vec) = self.platform_index.get_mut(&session.platform) {
                vec.retain(|&x| x != id);
            }
        }
        Ok(())
    }

    async fn list_by_profile(&self, profile: &str) -> Result<Vec<Session>, SessionStoreError> {
        let ids = self.profile_index.get(profile).map(|v| v.clone()).unwrap_or_default();
        let mut sessions = Vec::new();
        for id in ids {
            if let Some(session) = self.sessions.get(&id) {
                sessions.push(session.clone());
            }
        }
        Ok(sessions)
    }

    async fn list_by_platform(&self, platform: Platform) -> Result<Vec<Session>, SessionStoreError> {
        let ids = self.platform_index.get(&platform).map(|v| v.clone()).unwrap_or_default();
        let mut sessions = Vec::new();
        for id in ids {
            if let Some(session) = self.sessions.get(&id) {
                sessions.push(session.clone());
            }
        }
        Ok(sessions)
    }

    async fn list_active(&self) -> Result<Vec<Session>, SessionStoreError> {
        let sessions: Vec<_> = self.sessions.iter()
            .filter(|entry| entry.value().state == SessionState::Active)
            .map(|entry| entry.value().clone())
            .collect();
        Ok(sessions)
    }

    async fn cleanup_expired(&self, _ttl_secs: u64) -> Result<usize, SessionStoreError> {
        Ok(0) // No TTL in memory store
    }

    async fn health_check(&self) -> Result<(), SessionStoreError> {
        Ok(())
    }
}

impl Default for MemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Factory for creating session stores
pub struct SessionStoreFactory;

impl SessionStoreFactory {
    pub async fn create(
        backend: hermes_config::SessionStoreBackend,
        redis_config: Option<hermes_config::RedisConfig>,
        max_cache_size: usize,
        default_ttl_secs: u64,
    ) -> Result<Box<dyn SessionStore>, SessionStoreError> {
        match backend {
            hermes_config::SessionStoreBackend::Redis => {
                let redis = redis_config.ok_or_else(|| SessionStoreError::Config("Redis config required".into()))?;
                let store = RedisSessionStore::new(
                    &redis.url,
                    redis.pool_size,
                    max_cache_size,
                    default_ttl_secs,
                    None,
                ).await?;
                Ok(Box::new(store))
            }
            hermes_config::SessionStoreBackend::Memory => {
                Ok(Box::new(MemorySessionStore::new()))
            }
            hermes_config::SessionStoreBackend::Hybrid => {
                let redis = redis_config.ok_or_else(|| SessionStoreError::Config("Redis config required for hybrid".into()))?;
                let store = RedisSessionStore::new(
                    &redis.url,
                    redis.pool_size,
                    max_cache_size,
                    default_ttl_secs,
                    None,
                ).await?;
                Ok(Box::new(store))
            }
        }
    }
}
