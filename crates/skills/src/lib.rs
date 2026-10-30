// hermes-skills - Skills system with discovery, skills_list, and skill_view
use async_trait::async_trait;
use hermes_protocol::Platform;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use walkdir::WalkDir;

const MAX_DESCRIPTION_LENGTH: usize = 250;
const MAX_NAME_LENGTH: usize = 64;

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillMetadata {
    #[serde(default)]
    pub hermes: Option<HermesMetadata>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HermesMetadata {
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub related_skills: Vec<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub platforms: Vec<Platform>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub when_to_use: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub templates: HashMap<String, String>,
    #[serde(default)]
    pub scripts: HashMap<String, String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub skill_dir: Option<PathBuf>,
    #[serde(default)]
    pub skill_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillListItem {
    pub name: String,
    pub description: String,
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsListResponse {
    pub success: bool,
    pub skills: Vec<SkillListItem>,
    pub categories: Vec<String>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillViewResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_binary: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_provenance: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_files: Option<BTreeMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_hint: Option<String>,
    #[serde(default)]
    pub required_environment_variables: Vec<serde_json::Value>,
    #[serde(default)]
    pub required_commands: Vec<serde_json::Value>,
    #[serde(default)]
    pub missing_required_environment_variables: Vec<String>,
    #[serde(default)]
    pub missing_credential_files: Vec<String>,
    #[serde(default)]
    pub missing_required_commands: Vec<String>,
    #[serde(default)]
    pub setup_needed: bool,
    #[serde(default)]
    pub setup_skipped: bool,
    #[serde(default)]
    pub readiness_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_files: Option<BTreeMap<String, Vec<String>>>,
}

impl Skill {
    pub fn from_markdown(path: &PathBuf) -> Result<Self, SkillError> {
        let content = std::fs::read_to_string(path).map_err(SkillError::Io)?;
        let (frontmatter_str, body) = Self::parse_frontmatter(&content)?;
        
        let yaml_val: serde_yaml::Value = serde_yaml::from_str(&frontmatter_str)
            .map_err(|e| SkillError::Parse(e.to_string()))?;

        let mut skill: Skill = serde_yaml::from_value(yaml_val.clone())
            .unwrap_or_else(|_| Skill {
                name: path.parent().and_then(|p| p.file_name()).map(|f| f.to_string_lossy().to_string()).unwrap_or_default(),
                version: String::new(),
                description: String::new(),
                category: String::new(),
                tags: Vec::new(),
                author: String::new(),
                license: String::new(),
                platforms: Vec::new(),
                dependencies: Vec::new(),
                triggers: Vec::new(),
                when_to_use: String::new(),
                content: String::new(),
                references: Vec::new(),
                templates: HashMap::new(),
                scripts: HashMap::new(),
                metadata: None,
                skill_dir: None,
                skill_file: None,
            });

        if let Ok(json_meta) = serde_json::to_value(&yaml_val) {
            if let Some(meta) = json_meta.get("metadata") {
                skill.metadata = Some(meta.clone());
            }
        }

        if skill.name.is_empty() {
            skill.name = path.parent()
                .and_then(|p| p.file_name())
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
        }

        if skill.description.is_empty() {
            for line in body.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    skill.description = trimmed.to_string();
                    break;
                }
            }
        }

        skill.content = content;
        skill.skill_file = Some(path.clone());
        if path.file_name().map_or(false, |f| f == "SKILL.md") {
            skill.skill_dir = path.parent().map(|p| p.to_path_buf());
        }

        Ok(skill)
    }

    pub fn parse_frontmatter(content: &str) -> Result<(String, String), SkillError> {
        let trimmed = content.trim_start();
        if !trimmed.starts_with("---") {
            return Ok((String::new(), content.to_string()));
        }
        
        let after_first = &trimmed[3..];
        if let Some(end_idx) = after_first.find("---") {
            let frontmatter = &after_first[..end_idx];
            let body = &after_first[end_idx + 3..];
            Ok((frontmatter.trim().to_string(), body.trim().to_string()))
        } else {
            Err(SkillError::Parse("Invalid frontmatter delimiter".into()))
        }
    }
}

pub struct SkillRegistry {
    skills: Arc<RwLock<HashMap<String, Skill>>>,
    skill_dirs: Vec<PathBuf>,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new(Self::default_skill_dirs())
    }
}

impl SkillRegistry {
    pub fn new(skill_dirs: Vec<PathBuf>) -> Self {
        Self {
            skills: Arc::new(RwLock::new(HashMap::new())),
            skill_dirs,
        }
    }

    pub fn default_skill_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(home) = std::env::var("HOME") {
            let hermes_skills = PathBuf::from(&home).join(".hermes").join("skills");
            if hermes_skills.exists() {
                dirs.push(hermes_skills);
            }
            let plugins_skills = PathBuf::from(&home).join(".hermes").join("plugins");
            if plugins_skills.exists() {
                dirs.push(plugins_skills);
            }
        }
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let local_skills = current_dir.join("skills");
        if local_skills.exists() {
            dirs.push(local_skills);
        }
        dirs
    }

    pub async fn load_all(&self) -> Result<(), SkillError> {
        for dir in &self.skill_dirs {
            self.load_from_dir(dir).await?;
        }
        Ok(())
    }

    async fn load_from_dir(&self, dir: &Path) -> Result<(), SkillError> {
        if !dir.exists() {
            return Ok(());
        }

        let dir_clone = dir.to_path_buf();
        let loaded = tokio::task::spawn_blocking(move || {
            let mut result = Vec::new();
            let excluded_dirs: HashSet<&'static str> = [
                ".git", ".github", ".hub", ".archive", ".curator_backups", ".locks",
                ".venv", "venv", "node_modules", "site-packages", "__pycache__",
                ".tox", ".nox", ".pytest_cache", ".mypy_cache", ".ruff_cache",
            ].into_iter().collect();

            let support_dirs: HashSet<&'static str> = [
                "references", "templates", "assets", "scripts"
            ].into_iter().collect();

            let mut it = WalkDir::new(&dir_clone).follow_links(true).into_iter();
            while let Some(entry_res) = it.next() {
                let entry = match entry_res {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                let path = entry.path();
                let file_name = entry.file_name().to_string_lossy();

                if entry.file_type().is_dir() {
                    if excluded_dirs.contains(file_name.as_ref()) {
                        it.skip_current_dir();
                        continue;
                    }
                    // If this dir is a support dir inside a skill dir (parent has SKILL.md), skip it
                    if support_dirs.contains(file_name.as_ref()) {
                        if let Some(parent) = path.parent() {
                            if parent.join("SKILL.md").exists() {
                                it.skip_current_dir();
                                continue;
                            }
                        }
                    }
                } else if entry.file_type().is_file() {
                    let is_skill_md = file_name == "SKILL.md";
                    let is_plain_md = path.extension().map_or(false, |ext| ext == "md");
                    if is_skill_md || is_plain_md {
                        if let Ok(skill) = Skill::from_markdown(&path.to_path_buf()) {
                            result.push(skill);
                        }
                    }
                }
            }
            result
        }).await.map_err(|e| SkillError::Validation(format!("Join error: {}", e)))?;

        let mut skills_lock = self.skills.write().await;
        for skill in loaded {
            skills_lock.insert(skill.name.clone(), skill);
        }

        Ok(())
    }

    pub async fn register(&self, skill: Skill) {
        let mut skills_lock = self.skills.write().await;
        skills_lock.insert(skill.name.clone(), skill);
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
        let mut visited = HashSet::new();
        
        fn resolve<'a>(
            registry: &'a SkillRegistry,
            name: &'a str,
            resolved: &'a mut Vec<Skill>,
            visited: &'a mut HashSet<String>,
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

    pub async fn skills_list(&self, category: Option<&str>) -> SkillsListResponse {
        // Ensure skills are loaded if empty
        if self.skills.read().await.is_empty() {
            let _ = self.load_all().await;
        }

        let skills_guard = self.skills.read().await;
        let mut items = Vec::new();
        let mut categories_set = std::collections::BTreeSet::new();

        for skill in skills_guard.values() {
            let cat = if skill.category.is_empty() {
                Self::extract_category_from_skill(skill, &self.skill_dirs)
            } else {
                Some(skill.category.clone())
            };

            if let Some(c) = &cat {
                if !c.is_empty() {
                    categories_set.insert(c.clone());
                }
            }

            if let Some(filter_cat) = category {
                if cat.as_deref() != Some(filter_cat) {
                    continue;
                }
            }

            let truncated_desc = Self::truncate_description(&skill.description);
            let display_name = if skill.name.len() > MAX_NAME_LENGTH {
                skill.name[..MAX_NAME_LENGTH].to_string()
            } else {
                skill.name.clone()
            };

            items.push(SkillListItem {
                name: display_name,
                description: truncated_desc,
                category: cat,
            });
        }

        items.sort_by(|a, b| {
            let cat_a = a.category.as_deref().unwrap_or("");
            let cat_b = b.category.as_deref().unwrap_or("");
            cat_a.cmp(cat_b).then_with(|| a.name.cmp(&b.name))
        });

        let count = items.len();
        if count == 0 {
            return SkillsListResponse {
                success: true,
                skills: Vec::new(),
                categories: Vec::new(),
                count: 0,
                hint: None,
                message: Some("No skills found in skills/ directory.".to_string()),
            };
        }

        SkillsListResponse {
            success: true,
            skills: items,
            categories: categories_set.into_iter().collect(),
            count,
            hint: Some("Use skill_view(name) to see full content, tags, and linked files".to_string()),
            message: None,
        }
    }

    pub async fn skill_view(&self, name: &str, file_path: Option<&str>) -> SkillViewResponse {
        if name.contains("..") {
            return SkillViewResponse {
                success: false,
                error: Some("Skill name cannot contain '..' path traversal components.".to_string()),
                hint: Some("Use a skill name or relative path within the skills directory.".to_string()),
                ..Default::default()
            };
        }

        // If file_path is specified and has traversal
        if let Some(fp) = file_path {
            if fp.contains("..") {
                return SkillViewResponse {
                    success: false,
                    error: Some("Path traversal ('..') is not allowed.".to_string()),
                    hint: Some("Use a relative path within the skill directory".to_string()),
                    ..Default::default()
                };
            }
        }

        if self.skills.read().await.is_empty() {
            let _ = self.load_all().await;
        }

        let skill_opt = self.find_skill_candidate(name).await;
        let skill = match skill_opt {
            Some(s) => s,
            None => {
                let available = self.get_top_skill_names(20).await;
                return SkillViewResponse {
                    success: false,
                    error: Some(format!("Skill '{}' not found.", name)),
                    available_skills: Some(available),
                    hint: Some("Use skills_list to see all available skills".to_string()),
                    ..Default::default()
                };
            }
        };

        if let Some(fp) = file_path {
            return self.serve_skill_file(&skill, fp);
        }

        // Parse tags and related_skills from frontmatter / metadata
        let mut tags = skill.tags.clone();
        let mut related_skills = Vec::new();

        if let Some(meta) = &skill.metadata {
            if let Some(hermes) = meta.get("hermes") {
                if let Some(t_arr) = hermes.get("tags").and_then(|v| v.as_array()) {
                    let mut meta_tags = Vec::new();
                    for t in t_arr {
                        if let Some(s) = t.as_str() {
                            meta_tags.push(s.to_string());
                        }
                    }
                    if !meta_tags.is_empty() {
                        tags = meta_tags;
                    }
                }
                if let Some(rel_arr) = hermes.get("related_skills").and_then(|v| v.as_array()) {
                    for r in rel_arr {
                        if let Some(s) = r.as_str() {
                            related_skills.push(s.to_string());
                        }
                    }
                }
            }
        }

        let linked_files = skill.skill_dir.as_ref().map(|dir| Self::collect_linked_files(dir));
        let has_linked = linked_files.as_ref().map_or(false, |m| !m.is_empty());

        let rel_path = skill.skill_file.as_ref().map(|sf| {
            for base in &self.skill_dirs {
                if let Ok(rel) = sf.strip_prefix(base) {
                    return rel.to_string_lossy().to_string();
                }
            }
            sf.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default()
        });

        SkillViewResponse {
            success: true,
            name: Some(skill.name.clone()),
            description: Some(skill.description.clone()),
            tags: Some(tags),
            related_skills: Some(related_skills),
            content: Some(skill.content.clone()),
            path: rel_path,
            skill_dir: skill.skill_dir.as_ref().map(|d| d.to_string_lossy().to_string()),
            file: None,
            is_binary: None,
            org_provenance: None,
            linked_files: if has_linked { linked_files } else { None },
            usage_hint: if has_linked {
                Some("To view linked files, call skill_view(name, file_path) where file_path is e.g. 'references/api.md' or 'assets/config.yaml'".to_string())
            } else {
                None
            },
            required_environment_variables: Vec::new(),
            required_commands: Vec::new(),
            missing_required_environment_variables: Vec::new(),
            missing_credential_files: Vec::new(),
            missing_required_commands: Vec::new(),
            setup_needed: false,
            setup_skipped: false,
            readiness_status: "available".to_string(),
            _source_path: skill.skill_file.as_ref().map(|f| f.to_string_lossy().to_string()),
            metadata: skill.metadata.clone(),
            error: None,
            hint: None,
            available_skills: None,
            available_files: None,
        }
    }

    fn serve_skill_file(&self, skill: &Skill, file_path: &str) -> SkillViewResponse {
        let skill_dir = match &skill.skill_dir {
            Some(d) => d,
            None => {
                return SkillViewResponse {
                    success: false,
                    error: Some(format!("Skill '{}' has no directory.", skill.name)),
                    ..Default::default()
                };
            }
        };

        let target = skill_dir.join(file_path);
        // Path containment check
        let canonical_dir = match std::fs::canonicalize(skill_dir) {
            Ok(p) => p,
            Err(_) => skill_dir.clone(),
        };
        let canonical_target = match std::fs::canonicalize(&target) {
            Ok(p) => p,
            Err(_) => {
                let available = Self::collect_available_files(skill_dir);
                return SkillViewResponse {
                    success: false,
                    error: Some(format!("File '{}' not found in skill '{}'.", file_path, skill.name)),
                    available_files: Some(available),
                    hint: Some("Use one of the available file paths listed above".to_string()),
                    ..Default::default()
                };
            }
        };

        if !canonical_target.starts_with(&canonical_dir) {
            return SkillViewResponse {
                success: false,
                error: Some("Path traversal ('..') is not allowed.".to_string()),
                hint: Some("Use a relative path within the skill directory".to_string()),
                ..Default::default()
            };
        }

        if !canonical_target.is_file() {
            let available = Self::collect_available_files(skill_dir);
            return SkillViewResponse {
                success: false,
                error: Some(format!("File '{}' not found in skill '{}'.", file_path, skill.name)),
                available_files: Some(available),
                hint: Some("Use one of the available file paths listed above".to_string()),
                ..Default::default()
            };
        }

        match std::fs::read(&canonical_target) {
            Ok(bytes) => {
                match String::from_utf8(bytes) {
                    Ok(text) => SkillViewResponse {
                        success: true,
                        name: Some(skill.name.clone()),
                        file: Some(file_path.to_string()),
                        content: Some(text),
                        is_binary: Some(false),
                        ..Default::default()
                    },
                    Err(_) => {
                        let size = std::fs::metadata(&canonical_target).map(|m| m.len()).unwrap_or(0);
                        let fname = canonical_target.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                        SkillViewResponse {
                            success: true,
                            name: Some(skill.name.clone()),
                            file: Some(file_path.to_string()),
                            is_binary: Some(true),
                            content: Some(format!("[Binary file: {}, size: {} bytes]", fname, size)),
                            ..Default::default()
                        }
                    }
                }
            }
            Err(e) => SkillViewResponse {
                success: false,
                error: Some(format!("Failed to read '{}': {}", file_path, e)),
                ..Default::default()
            },
        }
    }

    async fn find_skill_candidate(&self, name: &str) -> Option<Skill> {
        let skills = self.skills.read().await;
        // Direct lookup
        if let Some(s) = skills.get(name) {
            return Some(s.clone());
        }

        // Check if name is path-like, e.g. "Security/hermes-agent" or "hermes-agent"
        for s in skills.values() {
            if s.name == name {
                return Some(s.clone());
            }
            if let Some(sd) = &s.skill_dir {
                if sd.file_name().map_or(false, |f| f == name) {
                    return Some(s.clone());
                }
            }
            if let Some(sf) = &s.skill_file {
                if sf.file_stem().map_or(false, |s| s == name) {
                    return Some(s.clone());
                }
            }
        }

        None
    }

    async fn get_top_skill_names(&self, limit: usize) -> Vec<String> {
        let skills = self.skills.read().await;
        let mut names: Vec<String> = skills.keys().cloned().collect();
        names.sort();
        names.into_iter().take(limit).collect()
    }

    fn truncate_description(desc: &str) -> String {
        let single_line = desc.replace('\n', " ");
        let trimmed = single_line.trim();
        if trimmed.len() <= MAX_DESCRIPTION_LENGTH {
            trimmed.to_string()
        } else {
            format!("{}...", &trimmed[..MAX_DESCRIPTION_LENGTH - 3])
        }
    }

    fn extract_category_from_skill(skill: &Skill, search_roots: &[PathBuf]) -> Option<String> {
        let path = skill.skill_file.as_ref()?;
        for root in search_roots {
            if let Ok(rel) = path.strip_prefix(root) {
                let components: Vec<_> = rel.iter().map(|c| c.to_string_lossy().to_string()).collect();
                if components.len() >= 3 {
                    return Some(components[0].clone());
                }
            }
        }
        None
    }

    fn collect_linked_files(skill_dir: &Path) -> BTreeMap<String, Vec<String>> {
        let mut groups = BTreeMap::new();
        let subdirs = ["references", "templates", "assets", "scripts"];
        for sub in &subdirs {
            let sub_path = skill_dir.join(sub);
            if sub_path.is_dir() {
                let mut files = Vec::new();
                for entry in WalkDir::new(&sub_path).into_iter().filter_map(|e| e.ok()) {
                    if entry.file_type().is_file() {
                        if let Ok(rel) = entry.path().strip_prefix(skill_dir) {
                            files.push(rel.to_string_lossy().to_string());
                        }
                    }
                }
                if !files.is_empty() {
                    files.sort();
                    groups.insert(sub.to_string(), files);
                }
            }
        }
        groups
    }

    fn collect_available_files(skill_dir: &Path) -> BTreeMap<String, Vec<String>> {
        let mut groups = BTreeMap::new();
        let support_dirs = ["references", "templates", "assets", "scripts"];
        for entry in WalkDir::new(skill_dir).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                let path = entry.path();
                if path.file_name().map_or(false, |f| f == "SKILL.md") {
                    continue;
                }
                if let Ok(rel) = path.strip_prefix(skill_dir) {
                    let rel_str = rel.to_string_lossy().to_string();
                    let top = rel_str.split('/').next().unwrap_or("");
                    let group_key = if support_dirs.contains(&top) {
                        top.to_string()
                    } else {
                        "other".to_string()
                    };
                    groups.entry(group_key).or_insert_with(Vec::new).push(rel_str);
                }
            }
        }
        for (_, files) in groups.iter_mut() {
            files.sort();
        }
        groups
    }
}

impl Default for SkillViewResponse {
    fn default() -> Self {
        Self {
            success: false,
            name: None,
            description: None,
            tags: None,
            related_skills: None,
            content: None,
            path: None,
            skill_dir: None,
            file: None,
            is_binary: None,
            org_provenance: None,
            linked_files: None,
            usage_hint: None,
            required_environment_variables: Vec::new(),
            required_commands: Vec::new(),
            missing_required_environment_variables: Vec::new(),
            missing_credential_files: Vec::new(),
            missing_required_commands: Vec::new(),
            setup_needed: false,
            setup_skipped: false,
            readiness_status: "available".to_string(),
            _source_path: None,
            metadata: None,
            error: None,
            hint: None,
            available_skills: None,
            available_files: None,
        }
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
        if let Some(llm) = &self.llm_client {
            let prompt = format!(
                "Generate a skill for: {}. Follow the skill markdown format exactly with YAML frontmatter ---",
                request
            );
            let response = llm.complete(&prompt).await?;
            let (frontmatter, body) = Skill::parse_frontmatter(&response)?;
            let mut skill: Skill = serde_yaml::from_str(&frontmatter)
                .map_err(|e| SkillError::Parse(e.to_string()))?;
            skill.content = body;
            Ok(skill)
        } else {
            Err(SkillError::Validation("No LLM client configured".into()))
        }
    }

    pub async fn merge_skills(&self, skills_content: Vec<String>) -> Result<Skill, SkillError> {
        if let Some(llm) = &self.llm_client {
            if skills_content.is_empty() {
                return Err(SkillError::Validation("No skills provided to merge".into()));
            }
            let mut prompt = String::from("Merge the following skills into a single, unified Skill markdown file with YAML frontmatter ---

");
            for (idx, sc) in skills_content.iter().enumerate() {
                prompt.push_str(&format!("=== Skill {} ===
{}

", idx + 1, sc));
            }
            let response = llm.complete(&prompt).await?;
            let (frontmatter, body) = Skill::parse_frontmatter(&response)?;
            let mut skill: Skill = serde_yaml::from_str(&frontmatter)
                .map_err(|e| SkillError::Parse(e.to_string()))?;
            skill.content = body;
            Ok(skill)
        } else {
            Err(SkillError::Validation("No LLM client configured for skill merging".into()))
        }
    }
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, prompt: &str) -> Result<String, SkillError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[tokio::test]
    async fn test_skills_list_and_view() {
        let temp_dir = std::env::temp_dir().join(format!("hermes_skills_test_{}", uuid::Uuid::new_v4()));
        let skill_dir = temp_dir.join("test-skill");
        let refs_dir = skill_dir.join("references");
        std::fs::create_dir_all(&refs_dir).unwrap();

        let skill_md_content = r#"---
name: test-skill
description: A test skill for verification
tags:
  - test
  - rust
metadata:
  hermes:
    tags:
      - test-tag-hermes
    related_skills:
      - another-skill
---
# Test Skill

This is the instructions body.
"#;
        let mut skill_file = File::create(skill_dir.join("SKILL.md")).unwrap();
        skill_file.write_all(skill_md_content.as_bytes()).unwrap();

        let mut ref_file = File::create(refs_dir.join("guide.md")).unwrap();
        ref_file.write_all(b"# Guide content\nDetails here.").unwrap();

        let registry = SkillRegistry::new(vec![temp_dir.clone()]);
        registry.load_all().await.unwrap();

        // 1. Test skills_list
        let list_resp = registry.skills_list(None).await;
        assert!(list_resp.success);
        assert_eq!(list_resp.count, 1);
        assert_eq!(list_resp.skills[0].name, "test-skill");
        assert_eq!(list_resp.skills[0].description, "A test skill for verification");

        // 2. Test skill_view main
        let view_resp = registry.skill_view("test-skill", None).await;
        assert!(view_resp.success);
        assert_eq!(view_resp.name.as_deref(), Some("test-skill"));
        assert!(view_resp.content.as_ref().unwrap().contains("# Test Skill"));
        assert_eq!(view_resp.tags.as_ref().unwrap(), &vec!["test-tag-hermes".to_string()]);
        assert_eq!(view_resp.related_skills.as_ref().unwrap(), &vec!["another-skill".to_string()]);
        assert!(view_resp.linked_files.is_some());
        let linked = view_resp.linked_files.unwrap();
        assert_eq!(linked.get("references").unwrap(), &vec!["references/guide.md".to_string()]);

        // 3. Test skill_view linked file
        let file_resp = registry.skill_view("test-skill", Some("references/guide.md")).await;
        assert!(file_resp.success);
        assert_eq!(file_resp.file.as_deref(), Some("references/guide.md"));
        assert!(file_resp.content.as_ref().unwrap().contains("# Guide content"));

        // Clean up
        let _ = std::fs::remove_dir_all(temp_dir);
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    #[tokio::test]
    async fn test_generate_skill_parses_real_llm_markdown() {
        let fake_llm_output = r#"---
name: test-skill
description: A verified test skill generated by LLM
tags:
  - test
---

# Test Skill
This content is preserved and not discarded.
"#;
        let (frontmatter, body) = Skill::parse_frontmatter(fake_llm_output).expect("Must parse frontmatter");
        let skill: Skill = serde_yaml::from_str(&frontmatter).expect("Must deserialize frontmatter");
        assert_eq!(skill.name, "test-skill");
        assert_eq!(skill.description, "A verified test skill generated by LLM");
        assert!(body.contains("This content is preserved and not discarded"));
    }
}
