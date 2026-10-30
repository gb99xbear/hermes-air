// hermes-cron - Cron scheduler with persistence
use async_trait::async_trait;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::RwLock;
use tokio_cron_scheduler::{Job, JobScheduler};
use uuid::Uuid;

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
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),
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
    AgentPrompt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub schedule: String,
    pub schedule_display: String,
    pub timezone: String,
    pub job_type: CronJobType,
    pub payload: serde_json::Value,
    pub enabled: bool,
    pub state: String,
    pub pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub deliver: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_deliver: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    #[serde(default)]
    pub no_agent: bool,
    pub repeat: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub run_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateJobParams {
    pub schedule: String,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub session_id: Option<String>,
    pub deliver: Option<String>,
    pub failure_deliver: Option<String>,
    pub skills: Option<Vec<String>>,
    pub paused: Option<bool>,
    pub paused_reason: Option<String>,
    pub pinned: Option<bool>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub script: Option<String>,
    pub monitor: Option<String>,
    pub no_agent: Option<bool>,
    pub repeat: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateJobParams {
    pub job_id: String,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub schedule: Option<String>,
    pub deliver: Option<String>,
    pub failure_deliver: Option<String>,
    pub skills: Option<Vec<String>>,
    pub paused: Option<bool>,
    pub pinned: Option<bool>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub script: Option<String>,
    pub monitor: Option<String>,
    pub no_agent: Option<bool>,
    pub repeat: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ParsedScheduleKind {
    Interval { seconds: u64 },
    Cron { expr: String },
    Once { run_at: OffsetDateTime },
}

#[derive(Debug, Clone)]
pub struct ParsedSchedule {
    pub kind: ParsedScheduleKind,
    pub display: String,
}

#[async_trait]
pub trait CronJobHandler: Send + Sync {
    async fn execute(&self, job: &CronJob) -> Result<(), CronError>;
}

pub struct CronScheduler {
    scheduler: Arc<RwLock<JobScheduler>>,
    scheduler_job_ids: Arc<RwLock<HashMap<String, Uuid>>>,
    jobs: Arc<RwLock<HashMap<String, CronJob>>>,
    handlers: Arc<RwLock<HashMap<CronJobType, Box<dyn CronJobHandler + Send + Sync>>>>,
    persistence_path: Option<std::path::PathBuf>,
}

fn parse_duration_seconds(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let pos = s.find(|c: char| !c.is_ascii_digit())?;
    let (num_part, unit) = s.split_at(pos);
    let val: u64 = num_part.parse().ok()?;
    match unit.trim().to_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => Some(val),
        "m" | "min" | "mins" | "minute" | "minutes" => Some(val * 60),
        "h" | "hr" | "hrs" | "hour" | "hours" => Some(val * 3600),
        "d" | "day" | "days" => Some(val * 86400),
        _ => None,
    }
}

pub fn parse_schedule(schedule: &str) -> Result<ParsedSchedule, CronError> {
    let raw = schedule.trim();
    if raw.is_empty() {
        return Err(CronError::ScheduleParse("Empty schedule string".to_string()));
    }
    let lower = raw.to_lowercase();

    // 1. One-shot delay: "in 30m", "in 2h"
    if let Some(rest) = lower.strip_prefix("in ") {
        if let Some(secs) = parse_duration_seconds(rest) {
            let run_at = OffsetDateTime::now_utc() + Duration::from_secs(secs);
            return Ok(ParsedSchedule {
                kind: ParsedScheduleKind::Once { run_at },
                display: format!("once in {}", rest.trim()),
            });
        }
    }

    // 2. Interval: "every 30m", "every hour"
    if let Some(rest) = lower.strip_prefix("every ") {
        let rest = rest.trim();
        if rest == "hour" {
            return Ok(ParsedSchedule {
                kind: ParsedScheduleKind::Interval { seconds: 3600 },
                display: "every hour".to_string(),
            });
        }
        if rest == "day" {
            return Ok(ParsedSchedule {
                kind: ParsedScheduleKind::Interval { seconds: 86400 },
                display: "every day".to_string(),
            });
        }
        if let Some(secs) = parse_duration_seconds(rest) {
            return Ok(ParsedSchedule {
                kind: ParsedScheduleKind::Interval { seconds: secs },
                display: format!("every {}", rest),
            });
        }
    }

    // 3. Bare duration: "30m", "2h", "10s"
    if let Some(secs) = parse_duration_seconds(&lower) {
        return Ok(ParsedSchedule {
            kind: ParsedScheduleKind::Interval { seconds: secs },
            display: format!("every {}", lower),
        });
    }

    // 4. ISO timestamp: "2026-10-30T17:00:00Z"
    if lower.contains('t') || (lower.len() >= 10 && lower.chars().nth(4) == Some('-')) {
        if let Ok(dt) = OffsetDateTime::parse(raw, &Rfc3339) {
            return Ok(ParsedSchedule {
                kind: ParsedScheduleKind::Once { run_at: dt },
                display: format!("once at {}", raw),
            });
        }
    }

    // 5. Cron syntax: 5 fields (standard) or 6 fields (extended)
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() == 5 || parts.len() == 6 {
        let cron_expr = if parts.len() == 5 {
            format!("0 {}", raw)
        } else {
            raw.to_string()
        };
        if Schedule::from_str(&cron_expr).is_ok() {
            return Ok(ParsedSchedule {
                kind: ParsedScheduleKind::Cron { expr: cron_expr },
                display: raw.to_string(),
            });
        }
    }

    Err(CronError::ScheduleParse(format!(
        "Invalid schedule '{}'. Expected interval (e.g. 30m, every 2h), delay (in 30m), cron (0 9 * * *), or ISO timestamp.",
        raw
    )))
}

fn compute_next_run(parsed: &ParsedSchedule) -> Option<String> {
    match &parsed.kind {
        ParsedScheduleKind::Once { run_at } => {
            run_at.format(&Rfc3339).ok()
        }
        ParsedScheduleKind::Interval { seconds } => {
            let next = OffsetDateTime::now_utc() + Duration::from_secs(*seconds);
            next.format(&Rfc3339).ok()
        }
        ParsedScheduleKind::Cron { expr } => {
            if let Ok(sched) = Schedule::from_str(expr) {
                sched.upcoming(chrono::Utc).next().map(|dt| {
                    if let Ok(odt) = OffsetDateTime::from_unix_timestamp(dt.timestamp()) {
                        odt.format(&Rfc3339).unwrap_or_default()
                    } else {
                        dt.to_rfc3339()
                    }
                })
            } else {
                None
            }
        }
    }
}

impl CronScheduler {
    pub async fn new(persistence_path: Option<std::path::PathBuf>) -> Result<Self, CronError> {
        let scheduler = JobScheduler::new()
            .await
            .map_err(|e| CronError::Scheduler(e.to_string()))?;

        let instance = Self {
            scheduler: Arc::new(RwLock::new(scheduler)),
            scheduler_job_ids: Arc::new(RwLock::new(HashMap::new())),
            jobs: Arc::new(RwLock::new(HashMap::new())),
            handlers: Arc::new(RwLock::new(HashMap::new())),
            persistence_path,
        };

        if let Some(ref path) = instance.persistence_path {
            let _ = instance.load_from_disk(path).await;
        }

        Ok(instance)
    }

    pub async fn create_job(&self, params: CreateJobParams) -> Result<CronJob, CronError> {
        let parsed = parse_schedule(&params.schedule)?;
        let id = Uuid::new_v4().to_string().replace("-", "")[..12].to_string();

        let prompt = params.prompt.unwrap_or_default();
        let name = match params.name {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => {
                if !prompt.is_empty() {
                    prompt.chars().take(50).collect()
                } else if let Some(ref sks) = params.skills {
                    sks.first().cloned().unwrap_or_else(|| format!("job-{}", &id[..6]))
                } else {
                    format!("job-{}", &id[..6])
                }
            }
        };

        let is_paused = params.paused.unwrap_or(false);
        let now = OffsetDateTime::now_utc();
        let next_run_at = if is_paused {
            None
        } else {
            compute_next_run(&parsed)
        };

        let skills = params.skills.unwrap_or_default();
        let skill = skills.first().cloned();

        let job = CronJob {
            id: id.clone(),
            name,
            prompt,
            schedule: params.schedule,
            schedule_display: parsed.display.clone(),
            timezone: "UTC".to_string(),
            job_type: CronJobType::AgentPrompt,
            payload: serde_json::Value::Null,
            enabled: !is_paused,
            state: if is_paused { "paused".to_string() } else { "scheduled".to_string() },
            pinned: params.pinned.unwrap_or(false),
            session_id: params.session_id,
            deliver: params.deliver.unwrap_or_else(|| "origin".to_string()),
            failure_deliver: params.failure_deliver,
            skills,
            skill,
            model: params.model,
            provider: params.provider,
            base_url: params.base_url,
            script: params.script,
            monitor: params.monitor,
            no_agent: params.no_agent.unwrap_or(false),
            repeat: params.repeat.unwrap_or_else(|| "forever".to_string()),
            created_at: now,
            updated_at: now,
            paused_at: if is_paused { Some(now.format(&Rfc3339).unwrap_or_default()) } else { None },
            paused_reason: params.paused_reason,
            last_run_at: None,
            next_run_at,
            last_status: None,
            last_error: None,
            run_count: 0,
            max_runs: None,
        };

        self.jobs.write().await.insert(id.clone(), job.clone());

        if job.enabled {
            self.schedule_job_in_engine(&job, &parsed).await?;
        }

        if let Some(path) = &self.persistence_path {
            let _ = self.save_to_disk(path).await;
        }

        Ok(job)
    }

    async fn schedule_job_in_engine(&self, job: &CronJob, parsed: &ParsedSchedule) -> Result<(), CronError> {
        let job_clone = job.clone();
        let jobs = self.jobs.clone();
        let handlers = self.handlers.clone();

        let tick_job = match &parsed.kind {
            ParsedScheduleKind::Interval { seconds } => {
                let dur = Duration::from_secs(*seconds);
                Job::new_repeated_async(dur, move |_uuid, _l| {
                    let job = job_clone.clone();
                    let handlers = handlers.clone();
                    let jobs = jobs.clone();
                    Box::pin(async move {
                        Self::execute_job_tick(job, handlers, jobs).await;
                    })
                }).map_err(|e| CronError::Scheduler(e.to_string()))?
            }
            ParsedScheduleKind::Cron { expr } => {
                Job::new_async(expr.as_str(), move |_uuid, _l| {
                    let job = job_clone.clone();
                    let handlers = handlers.clone();
                    let jobs = jobs.clone();
                    Box::pin(async move {
                        Self::execute_job_tick(job, handlers, jobs).await;
                    })
                }).map_err(|e| CronError::Scheduler(e.to_string()))?
            }
            ParsedScheduleKind::Once { run_at } => {
                let now = OffsetDateTime::now_utc();
                let diff = if *run_at > now {
                    (*run_at - now).unsigned_abs()
                } else {
                    Duration::from_millis(100)
                };
                Job::new_one_shot_async(diff, move |_uuid, _l| {
                    let job = job_clone.clone();
                    let handlers = handlers.clone();
                    let jobs = jobs.clone();
                    Box::pin(async move {
                        Self::execute_job_tick(job, handlers, jobs).await;
                    })
                }).map_err(|e| CronError::Scheduler(e.to_string()))?
            }
        };

        let sched_guard = self.scheduler.write().await;
        let job_uuid = sched_guard.add(tick_job).await.map_err(|e| CronError::Scheduler(e.to_string()))?;
        self.scheduler_job_ids.write().await.insert(job.id.clone(), job_uuid);
        Ok(())
    }

    async fn execute_job_tick(
        job: CronJob,
        handlers: Arc<RwLock<HashMap<CronJobType, Box<dyn CronJobHandler + Send + Sync>>>>,
        jobs: Arc<RwLock<HashMap<String, CronJob>>>,
    ) {
        let handler_opt = handlers.read().await.get(&job.job_type).is_some();
        let run_result = if handler_opt {
            let handlers_guard = handlers.read().await;
            if let Some(handler) = handlers_guard.get(&job.job_type) {
                handler.execute(&job).await
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };

        let mut jobs_guard = jobs.write().await;
        if let Some(j) = jobs_guard.get_mut(&job.id) {
            let now = OffsetDateTime::now_utc();
            j.last_run_at = Some(now.format(&Rfc3339).unwrap_or_default());
            j.run_count += 1;
            match run_result {
                Ok(_) => {
                    j.last_status = Some("ok".to_string());
                    j.last_error = None;
                }
                Err(e) => {
                    j.last_status = Some("error".to_string());
                    j.last_error = Some(e.to_string());
                }
            }
            if let Ok(parsed) = parse_schedule(&j.schedule) {
                j.next_run_at = compute_next_run(&parsed);
            }
        }
    }

    pub async fn list_jobs(&self) -> Vec<CronJob> {
        let mut list: Vec<CronJob> = self.jobs.read().await.values().cloned().collect();
        list.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        list
    }

    pub async fn get_job(&self, job_ref: &str) -> Option<CronJob> {
        let jobs = self.jobs.read().await;
        if let Some(j) = jobs.get(job_ref) {
            return Some(j.clone());
        }
        for j in jobs.values() {
            if j.name == job_ref {
                return Some(j.clone());
            }
        }
        None
    }

    pub async fn update_job(&self, params: UpdateJobParams) -> Result<CronJob, CronError> {
        let mut jobs = self.jobs.write().await;
        let job = jobs
            .values_mut()
            .find(|j| j.id == params.job_id || j.name == params.job_id)
            .ok_or_else(|| CronError::JobNotFound(params.job_id.clone()))?;

        if let Some(p) = params.prompt {
            job.prompt = p;
        }
        if let Some(n) = params.name {
            if !n.trim().is_empty() {
                job.name = n.trim().to_string();
            }
        }
        if let Some(s) = params.schedule {
            let parsed = parse_schedule(&s)?;
            job.schedule = s;
            job.schedule_display = parsed.display.clone();
            if job.enabled {
                job.next_run_at = compute_next_run(&parsed);
            }
        }
        if let Some(d) = params.deliver {
            job.deliver = d;
        }
        if let Some(fd) = params.failure_deliver {
            job.failure_deliver = if fd.is_empty() { None } else { Some(fd) };
        }
        if let Some(sks) = params.skills {
            job.skill = sks.first().cloned();
            job.skills = sks;
        }
        if let Some(p) = params.pinned {
            job.pinned = p;
        }
        if let Some(m) = params.model {
            job.model = if m.is_empty() { None } else { Some(m) };
        }
        if let Some(p) = params.provider {
            job.provider = if p.is_empty() { None } else { Some(p) };
        }
        if let Some(b) = params.base_url {
            job.base_url = if b.is_empty() { None } else { Some(b) };
        }
        if let Some(sc) = params.script {
            job.script = if sc.is_empty() { None } else { Some(sc) };
        }
        if let Some(mo) = params.monitor {
            job.monitor = if mo.is_empty() { None } else { Some(mo) };
        }
        if let Some(na) = params.no_agent {
            job.no_agent = na;
        }
        if let Some(r) = params.repeat {
            job.repeat = r;
        }

        job.updated_at = OffsetDateTime::now_utc();
        let updated = job.clone();
        drop(jobs);

        if let Some(path) = &self.persistence_path {
            let _ = self.save_to_disk(path).await;
        }

        Ok(updated)
    }

    pub async fn pause_job(&self, job_ref: &str, reason: Option<String>) -> Result<CronJob, CronError> {
        let mut jobs = self.jobs.write().await;
        let job = jobs
            .values_mut()
            .find(|j| j.id == job_ref || j.name == job_ref)
            .ok_or_else(|| CronError::JobNotFound(job_ref.to_string()))?;

        job.enabled = false;
        job.state = "paused".to_string();
        job.paused_at = Some(OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default());
        job.paused_reason = reason;
        job.updated_at = OffsetDateTime::now_utc();

        let job_id = job.id.clone();
        let updated = job.clone();
        drop(jobs);

        // Remove from scheduler if scheduled
        let mut ids_guard = self.scheduler_job_ids.write().await;
        if let Some(uuid) = ids_guard.remove(&job_id) {
            let sched_guard = self.scheduler.read().await;
            let _ = sched_guard.remove(&uuid).await;
        }

        if let Some(path) = &self.persistence_path {
            let _ = self.save_to_disk(path).await;
        }

        Ok(updated)
    }

    pub async fn resume_job(&self, job_ref: &str) -> Result<CronJob, CronError> {
        let mut jobs = self.jobs.write().await;
        let job = jobs
            .values_mut()
            .find(|j| j.id == job_ref || j.name == job_ref)
            .ok_or_else(|| CronError::JobNotFound(job_ref.to_string()))?;

        job.enabled = true;
        job.state = "scheduled".to_string();
        job.paused_at = None;
        job.paused_reason = None;
        job.updated_at = OffsetDateTime::now_utc();

        let parsed = parse_schedule(&job.schedule)?;
        job.next_run_at = compute_next_run(&parsed);

        let updated = job.clone();
        drop(jobs);

        self.schedule_job_in_engine(&updated, &parsed).await?;

        if let Some(path) = &self.persistence_path {
            let _ = self.save_to_disk(path).await;
        }

        Ok(updated)
    }

    pub async fn remove_job(&self, job_ref: &str) -> Result<CronJob, CronError> {
        let mut jobs = self.jobs.write().await;
        let job_key = jobs
            .iter()
            .find(|(_, j)| j.id == job_ref || j.name == job_ref)
            .map(|(k, _)| k.clone())
            .ok_or_else(|| CronError::JobNotFound(job_ref.to_string()))?;

        let removed = jobs.remove(&job_key).unwrap();
        drop(jobs);

        let mut ids_guard = self.scheduler_job_ids.write().await;
        if let Some(uuid) = ids_guard.remove(&removed.id) {
            let sched_guard = self.scheduler.read().await;
            let _ = sched_guard.remove(&uuid).await;
        }

        if let Some(path) = &self.persistence_path {
            let _ = self.save_to_disk(path).await;
        }

        Ok(removed)
    }

    pub async fn run_job_now(&self, job_ref: &str, extra_prompt: Option<String>) -> Result<CronJob, CronError> {
        let job = self.get_job(job_ref).await
            .ok_or_else(|| CronError::JobNotFound(job_ref.to_string()))?;

        if !job.enabled {
            return Err(CronError::InvalidArgument("Job is paused/disabled; resume it before running.".to_string()));
        }

        let mut fire_job = job.clone();
        if let Some(extra) = extra_prompt {
            if !extra.trim().is_empty() {
                if fire_job.prompt.is_empty() {
                    fire_job.prompt = extra;
                } else {
                    fire_job.prompt = format!("{}\n\nContext:\n{}", fire_job.prompt, extra);
                }
            }
        }

        let handlers = self.handlers.clone();
        let jobs = self.jobs.clone();

        tokio::spawn(async move {
            Self::execute_job_tick(fire_job, handlers, jobs).await;
        });

        // Return refreshed view
        let jobs_guard = self.jobs.read().await;
        Ok(jobs_guard.get(&job.id).cloned().unwrap_or(job))
    }

    pub async fn register_handler(&self, job_type: CronJobType, handler: Box<dyn CronJobHandler + Send + Sync>) {
        self.handlers.write().await.insert(job_type, handler);
    }

    pub async fn start(&self) -> Result<(), CronError> {
        let sched = self.scheduler.read().await;
        sched.start().await.map_err(|e| CronError::Scheduler(e.to_string()))?;
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<(), CronError> {
        let mut sched = self.scheduler.write().await;
        sched.shutdown().await.map_err(|e| CronError::Scheduler(e.to_string()))?;
        Ok(())
    }

    pub async fn load_from_disk(&self, path: &std::path::Path) -> Result<(), CronError> {
        if path.exists() {
            let content = tokio::fs::read_to_string(path).await.map_err(CronError::Io)?;
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                let jobs_val = if val.is_object() && val.get("jobs").is_some() {
                    val.get("jobs").unwrap().clone()
                } else {
                    val
                };

                if let Ok(jobs_map) = serde_json::from_value::<HashMap<String, CronJob>>(jobs_val.clone()) {
                    *self.jobs.write().await = jobs_map;
                } else if let Ok(jobs_list) = serde_json::from_value::<Vec<CronJob>>(jobs_val) {
                    let mut map = HashMap::new();
                    for j in jobs_list {
                        map.insert(j.id.clone(), j);
                    }
                    *self.jobs.write().await = map;
                }
            }
        }
        Ok(())
    }

    pub async fn save_to_disk(&self, path: &std::path::Path) -> Result<(), CronError> {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
        }
        let jobs = self.jobs.read().await;
        let job_list: Vec<&CronJob> = jobs.values().collect();
        let payload = serde_json::json!({
            "jobs": job_list,
            "updated_at": OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
        });
        let content = serde_json::to_string_pretty(&payload)?;
        tokio::fs::write(path, content).await.map_err(CronError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parse_schedule_forms() {
        assert!(parse_schedule("30m").is_ok());
        assert!(parse_schedule("every 2h").is_ok());
        assert!(parse_schedule("in 15m").is_ok());
        assert!(parse_schedule("0 9 * * *").is_ok());
        assert!(parse_schedule("").is_err());
    }

    #[tokio::test]
    async fn test_cron_scheduler_lifecycle() {
        let scheduler = CronScheduler::new(None).await.expect("create scheduler");

        // 1. Create
        let params = CreateJobParams {
            schedule: "every 30m".to_string(),
            prompt: Some("Test reminder".to_string()),
            name: Some("test-job".to_string()),
            session_id: Some("sess-1".to_string()),
            deliver: Some("origin".to_string()),
            failure_deliver: None,
            skills: Some(vec!["skill-a".to_string()]),
            paused: Some(false),
            paused_reason: None,
            pinned: Some(true),
            model: Some("model-x".to_string()),
            provider: Some("provider-y".to_string()),
            base_url: None,
            script: None,
            monitor: None,
            no_agent: Some(false),
            repeat: Some("forever".to_string()),
        };
        let job = scheduler.create_job(params).await.expect("create job");
        assert_eq!(job.name, "test-job");
        assert_eq!(job.state, "scheduled");
        assert!(job.enabled);

        // 2. List
        let jobs = scheduler.list_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);

        // 3. Pause
        let paused = scheduler.pause_job(&job.id, Some("maintenance".to_string())).await.expect("pause");
        assert_eq!(paused.state, "paused");
        assert!(!paused.enabled);

        // 4. Resume
        let resumed = scheduler.resume_job(&job.id).await.expect("resume");
        assert_eq!(resumed.state, "scheduled");
        assert!(resumed.enabled);

        // 5. Update
        let update_params = UpdateJobParams {
            job_id: job.id.clone(),
            prompt: Some("Updated prompt".to_string()),
            name: Some("renamed-job".to_string()),
            ..Default::default()
        };
        let updated = scheduler.update_job(update_params).await.expect("update");
        assert_eq!(updated.name, "renamed-job");
        assert_eq!(updated.prompt, "Updated prompt");

        // 6. Run now
        let ran = scheduler.run_job_now(&job.id, Some("extra info".to_string())).await.expect("run now");
        assert_eq!(ran.id, job.id);

        // 7. Remove
        let removed = scheduler.remove_job(&job.id).await.expect("remove");
        assert_eq!(removed.id, job.id);

        let jobs_after = scheduler.list_jobs().await;
        assert_eq!(jobs_after.len(), 0);
    }
}
