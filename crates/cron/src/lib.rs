// hermes-cron - Cron scheduler with persistence
use async_trait::async_trait;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::str::FromStr;
use tokio::sync::RwLock;
use tokio_cron_scheduler::{Job, JobScheduler};
use thiserror::Error;
use uuid::Uuid;
use time::OffsetDateTime;


#[derive(Debug, Error)]
pub enum CronError {
    #[error("Schedule parse error: {0}")]
    ScheduleParse(String),
    #[error("Job not found: {0}")]
    JobNotFound(String),
    #[error("Scheduler error: {0}")]
    Scheduler(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: Uuid,
    pub name: String,
    pub schedule: String,
    pub timezone: String,
    pub job_type: CronJobType,
    pub payload: serde_json::Value,
    pub enabled: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub last_run: Option<OffsetDateTime>,
    pub next_run: Option<OffsetDateTime>,
    pub run_count: u64,
    pub max_runs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronJobType {
    PlatformMessage,
    Webhook,
    Script,
    Plugin,
    Cleanup,
    Backup,
}

#[async_trait]
pub trait CronJobHandler: Send + Sync {
    async fn execute(&self, job: &CronJob) -> Result<(), CronError>;
}

pub struct CronScheduler {
    scheduler: JobScheduler,
    jobs: Arc<RwLock<HashMap<Uuid, CronJob>>>,
    handlers: Arc<RwLock<HashMap<CronJobType, Box<dyn CronJobHandler + Send + Sync>>>>,
    persistence_path: Option<std::path::PathBuf>,
}

impl CronScheduler {
    pub async fn new(persistence_path: Option<std::path::PathBuf>) -> Result<Self, CronError> {
        let scheduler = JobScheduler::new().await.map_err(|e| CronError::Scheduler(e.to_string()))?;
        
        let instance = Self {
            scheduler,
            jobs: Arc::new(RwLock::new(HashMap::new())),
            handlers: Arc::new(RwLock::new(HashMap::new())),
            persistence_path,
        };

        Ok(instance)
    }

    pub async fn add_job(&self, job: CronJob) -> Result<(), CronError> {

        let job_id = job.id;
        let job_clone = job.clone();
        let jobs = self.jobs.clone();
        let handlers = self.handlers.clone();

        let job_builder = Job::new_async(job.schedule.as_str(), move |_uuid, _l| {
            let job = job_clone.clone();
            let handlers = handlers.clone();
            let jobs = jobs.clone();
            Box::pin(async move {
                if let Some(handler) = handlers.read().await.get(&job.job_type) {
                    if let Err(e) = handler.execute(&job).await {
                        tracing::error!("Cron job {} failed: {}", job.name, e);
                    }
                }
                
                // Update job stats
                let mut jobs_guard = jobs.write().await;
                if let Some(j) = jobs_guard.get_mut(&job.id) {
                    j.last_run = Some(OffsetDateTime::now_utc());
                    j.run_count += 1;
                    if let Some(max) = j.max_runs {
                        if j.run_count >= max {
                            // Job completed max runs
                        }
                    }
                    j.next_run = Schedule::from_str(&j.schedule).ok().and_then(|s| s.upcoming(chrono::Utc).next()).map(|dt| OffsetDateTime::from_unix_timestamp(dt.timestamp()).ok()).flatten();
                }
            })
        }).map_err(|e| CronError::Scheduler(e.to_string()))?;

        self.scheduler.add(job_builder).await.map_err(|e| CronError::Scheduler(e.to_string()))?;
        self.jobs.write().await.insert(job_id, job);
        
        if let Some(path) = &self.persistence_path {
            self.save_to_disk(path).await?;
        }

        Ok(())
    }

    pub async fn remove_job(&self, id: Uuid) -> Result<(), CronError> {
        self.scheduler.remove(&id).await.map_err(|e| CronError::Scheduler(e.to_string()))?;
        self.jobs.write().await.remove(&id);
        
        if let Some(path) = &self.persistence_path {
            self.save_to_disk(path).await?;
        }

        Ok(())
    }

    pub async fn enable_job(&self, id: Uuid, enabled: bool) -> Result<(), CronError> {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(&id) {
            job.enabled = enabled;
            // Note: tokio-cron-scheduler 0.11 doesn't have resume/pause
            // We would need to remove and re-add the job to effectively pause/resume
        } else {
            return Err(CronError::JobNotFound(id.to_string()));
        }
        
        if let Some(path) = &self.persistence_path {
            self.save_to_disk(path).await?;
        }

        Ok(())
    }

    pub async fn get_job(&self, id: Uuid) -> Option<CronJob> {
        self.jobs.read().await.get(&id).cloned()
    }

    pub async fn list_jobs(&self) -> Vec<CronJob> {
        self.jobs.read().await.values().cloned().collect()
    }

    pub async fn register_handler(&self, job_type: CronJobType, handler: Box<dyn CronJobHandler + Send + Sync>) {
        self.handlers.write().await.insert(job_type, handler);
    }

    pub async fn start(&self) -> Result<(), CronError> {
        self.scheduler.start().await.map_err(|e| CronError::Scheduler(e.to_string()))?;
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<(), CronError> {
        self.scheduler.shutdown().await.map_err(|e| CronError::Scheduler(e.to_string()))?;
        Ok(())
    }

    #[allow(dead_code)]
    async fn load_from_disk(&self, path: &std::path::PathBuf) -> Result<(), CronError> {
        if path.exists() {
            let content = tokio::fs::read_to_string(path).await
                .map_err(|e| CronError::Io(e))?;
            let jobs: HashMap<Uuid, CronJob> = serde_json::from_str(&content)
                .map_err(|e| CronError::Serialization(e))?;
            let jobs_clone = jobs.clone();
            *self.jobs.write().await = jobs;
            
            // Re-register jobs with scheduler
            for job in jobs_clone.values() {
                if job.enabled {
                    self.add_job(job.clone()).await?;
                }
            }
        }
        Ok(())
    }

    async fn save_to_disk(&self, path: &std::path::PathBuf) -> Result<(), CronError> {
        let jobs = self.jobs.read().await;
        let content = serde_json::to_string(&*jobs)
            .map_err(|e| CronError::Serialization(e))?;
        tokio::fs::write(path, content).await
            .map_err(|e| CronError::Io(e))?;
        Ok(())
    }
}
