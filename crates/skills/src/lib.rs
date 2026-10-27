// hermes-skills - Skills system with curator
use hermes_protocol::Platform;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("Skill not found: {0}")]
    NotFound(String),
    #[error("Skill parse error: {0}")]
    Parse(String),
    #[error("Skill validation error: {0}")]
    Validation(String),
    #[error("Dependency error: {0}")]
    Dependency(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub version: String,
    pub description: String,
    pub category: String,
    pub tags: Vec<String>,
    pub author: String,
    pub license: String,
    pub platforms: Vec<Platform>,
    pub dependencies: Vec<String>,
    pub triggers: Vec<String>,
    pub when_to_use: String,
    pub content: String,
    pub references: Vec<String>,
    pub templates: HashMap<String, String>,
    pub scripts: HashMap<String, String>,
}

impl Skill {
    pub fn from_markdown(path: &PathBuf) -> Result<Self, SkillError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| SkillError::Io(e))?;
        
        // Parse frontmatter
        let (frontmatter, body) = Self::parse_frontmatter(&content)?;
        
        let mut skill: Skill = serde_yaml::from_str(&frontmatter)
            .map_err(|e| SkillError::Parse(e.to_string()))?;
        
        skill.content = body;
        Ok(skill)
    }

    fn parse_frontmatter(content: &str) -> Result<(String, String), SkillError> {
        if !content.starts_with("---") {
            return Err(SkillError::Parse("Missing frontmatter".into()));
        }
        
        let parts: Vec<&str> = content.splitn(3, "---").collect();
        if parts.len() < 3 {
            return Err(SkillError::Parse("Invalid frontmatter".into()));
        }
        
        Ok((parts[1].to_string(), parts[2].trim().to_string()))
    }
}

pub struct SkillRegistry {
    skills: Arc<RwLock<HashMap<String, Skill>>>,
    skill_dirs: Vec<PathBuf>,
}

impl SkillRegistry {
    pub fn new(skill_dirs: Vec<PathBuf>) -> Self {
        Self {
            skills: Arc::new(RwLock::new(HashMap::new())),
            skill_dirs,
        }
    }

    pub async fn load_all(&self) -> Result<(), SkillError> {
        for dir in &self.skill_dirs {
            self.load_from_dir(dir).await?;
        }
        Ok(())
    }

    async fn load_from_dir(&self, dir: &PathBuf) -> Result<(), SkillError> {
        if !dir.exists() {
            return Ok(());
        }

        let mut entries = tokio::fs::read_dir(dir).await
            .map_err(|e| SkillError::Io(e))?;

        while let Some(entry) = entries.next_entry().await
            .map_err(|e| SkillError::Io(e))? {
            
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "md") {
                match Skill::from_markdown(&path) {
                    Ok(skill) => {
                        self.skills.write().await.insert(skill.name.clone(), skill);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load skill from {:?}: {}", path, e);
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn get(&self, name: &str) -> Option<Skill> {
        self.skills.read().await.get(name).cloned()
    }

    pub async fn list(&self) -> Vec<Skill> {
        self.skills.read().await.values().cloned().collect()
    }

    pub async fn search(&self, query: &str) -> Vec<Skill> {
        let skills = self.skills.read().await;
        skills.values()
            .filter(|s| {
                s.name.contains(query) ||
                s.description.contains(query) ||
                s.tags.iter().any(|t| t.contains(query)) ||
                s.triggers.iter().any(|t| t.contains(query))
            })
            .cloned()
            .collect()
    }

    pub async fn get_by_category(&self, category: &str) -> Vec<Skill> {
        self.skills.read().await.values()
            .filter(|s| s.category == category)
            .cloned()
            .collect()
    }

    pub async fn get_by_platform(&self, platform: Platform) -> Vec<Skill> {
        self.skills.read().await.values()
            .filter(|s| s.platforms.contains(&platform))
            .cloned()
            .collect()
    }

    pub async fn resolve_dependencies(&self, skill_name: &str) -> Result<Vec<Skill>, SkillError> {
        let _skill = self.get(skill_name).await
            .ok_or_else(|| SkillError::NotFound(skill_name.to_string()))?;
        
        let mut resolved = Vec::new();
        let mut visited = std::collections::HashSet::new();
        
        fn resolve<'a>(
            registry: &'a SkillRegistry,
            name: &'a str,
            resolved: &'a mut Vec<Skill>,
            visited: &'a mut std::collections::HashSet<String>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SkillError>> + Send + 'a>> {
            Box::pin(async move {
                if visited.contains(name) {
                    return Err(SkillError::Dependency(format!("Circular dependency: {}", name)));
                }
                visited.insert(name.to_string());
                
                if let Some(skill) = registry.get(name).await {
                    let deps = skill.dependencies.clone();
                    for dep in &deps {
                        resolve(registry, dep, resolved, visited).await?;
                    }
                    if !resolved.iter().any(|s| s.name == name) {
                        resolved.push(skill);
                    }
                }
                Ok(())
            })
        }
        
        resolve(self, skill_name, &mut resolved, &mut visited).await?;
        Ok(resolved)
    }
}

pub struct Curator {
    _registry: SkillRegistry,
    llm_client: Option<Box<dyn LlmClient>>,
}

impl Curator {
    pub fn new(registry: SkillRegistry, llm_client: Option<Box<dyn LlmClient>>) -> Self {
        Self { _registry: registry, llm_client }
    }

    pub async fn generate_skill(&self, request: &str) -> Result<Skill, SkillError> {
        // Use LLM to generate skill from request
        if let Some(llm) = &self.llm_client {
            let prompt = format!(
                "Generate a skill for: {}. Follow the skill markdown format exactly.",
                request
            );
            let _response = llm.complete(&prompt).await?;
            // Parse response into Skill
            Ok(Skill::from_markdown(&PathBuf::from(""))?) // placeholder
        } else {
            Err(SkillError::Validation("No LLM client configured".into()))
        }
    }

    pub async fn merge_skills(&self, _skills: Vec<String>) -> Result<Skill, SkillError> {
        // Merge multiple skills into one
        Err(SkillError::Validation("Not implemented".into()))
    }
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, prompt: &str) -> Result<String, SkillError>;
}
