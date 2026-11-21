use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info};

use crate::memory::MemoryStore;
use hermes_skills::SkillRegistry;

/// Which skills index the assembler injects into the system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillsCatalogMode {
    /// No index at all (skills reachable only through the `skills_list` tool).
    Off,
    /// Names only, grouped by category.
    Names,
    /// Name + truncated description, grouped by category (Hermes' shape).
    Full,
}

impl SkillsCatalogMode {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" => Self::Off,
            "names" | "index" => Self::Names,
            _ => Self::Full,
        }
    }
}

/// Prompt composition settings, resolved from `PromptConfig`.
#[derive(Debug, Clone)]
pub struct PromptSettings {
    pub inject_memory: bool,
    pub skills_catalog: SkillsCatalogMode,
    pub skill_description_budget: usize,
}

impl Default for PromptSettings {
    fn default() -> Self {
        Self {
            inject_memory: true,
            skills_catalog: SkillsCatalogMode::Full,
            skill_description_budget: 1024,
        }
    }
}

/// Represents an assembled system prompt built from local operational doctrine,
/// soul, memory, user files, and base profile prompt.
#[derive(Clone)]
pub struct PromptAssembler {
    custom_hermes_dir: Option<PathBuf>,
    custom_cwd: Option<PathBuf>,
    memory_store: Option<Arc<MemoryStore>>,
    skill_registry: Option<Arc<SkillRegistry>>,
    settings: PromptSettings,
}

// Manual impl: MemoryStore and SkillRegistry do not implement Debug, so the
// derive cannot be used. Report presence only, never their internals.
impl std::fmt::Debug for PromptAssembler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptAssembler")
            .field("custom_hermes_dir", &self.custom_hermes_dir)
            .field("custom_cwd", &self.custom_cwd)
            .field("has_memory_store", &self.memory_store.is_some())
            .field("has_skill_registry", &self.skill_registry.is_some())
            .field("settings", &self.settings)
            .finish()
    }
}

impl Default for PromptAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptAssembler {
    pub fn new() -> Self {
        Self {
            custom_hermes_dir: None,
            custom_cwd: None,
            memory_store: None,
            skill_registry: None,
            settings: PromptSettings::default(),
        }
    }

    /// Attach the memory store so its frozen snapshot can be rendered into the prompt.
    pub fn with_memory_store(mut self, store: Arc<MemoryStore>) -> Self {
        self.memory_store = Some(store);
        self
    }

    /// Attach the skills registry so a compact index can be rendered into the prompt.
    pub fn with_skill_registry(mut self, registry: Arc<SkillRegistry>) -> Self {
        self.skill_registry = Some(registry);
        self
    }

    /// Override the composition settings (memory/skills injection).
    pub fn with_settings(mut self, settings: PromptSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Construct a PromptAssembler with custom directory overrides (useful for testing or non-standard paths).
    pub fn with_overrides(hermes_dir: Option<PathBuf>, cwd: Option<PathBuf>) -> Self {
        Self {
            custom_hermes_dir: hermes_dir,
            custom_cwd: cwd,
            memory_store: None,
            skill_registry: None,
            settings: PromptSettings::default(),
        }
    }

    fn resolve_hermes_dir(&self) -> PathBuf {
        if let Some(ref dir) = self.custom_hermes_dir {
            return dir.clone();
        }
        if let Ok(home) = std::env::var("HERMES_HOME") {
            return PathBuf::from(home);
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".hermes");
        }
        PathBuf::from(".hermes")
    }

    fn resolve_cwd(&self) -> PathBuf {
        if let Some(ref cwd) = self.custom_cwd {
            return cwd.clone();
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/home/ubuntu"))
    }

    async fn read_first_existing(candidates: &[PathBuf]) -> Option<String> {
        for candidate in candidates {
            if candidate.is_file() {
                match tokio::fs::read_to_string(candidate).await {
                    Ok(content) => {
                        let trimmed = content.trim();
                        if !trimmed.is_empty() {
                            debug!("Loaded prompt context file: {:?}", candidate);
                            return Some(trimmed.to_string());
                        }
                    }
                    Err(e) => {
                        debug!("Failed to read candidate file {:?}: {}", candidate, e);
                    }
                }
            }
        }
        None
    }

    /// Asynchronously builds the combined system prompt from:
    /// 1. `SOUL.md` (e.g. from ~/.hermes/SOUL.md or custom hermes dir)
    /// 2. `AGENTS.md` (from cwd or /home/ubuntu/AGENTS.md)
    /// 3. `MEMORY.md` (from ~/.hermes/memories/MEMORY.md or ~/.hermes/MEMORY.md)
    /// 4. `USER.md` (from ~/.hermes/memories/USER.md or ~/.hermes/USER.md)
    /// 5. `system.md` (from cwd or ~/.hermes/system.md)
    /// 6. Profile's configured base `system_prompt` from config.toml
    /// 7. The attached `MemoryStore` snapshot, when `inject_memory` is on
    /// 8. A compact skills index, when `skills_catalog` is not `off`
    ///
    /// Skips any missing files gracefully without error.
    pub async fn build_system_prompt(&self, base_profile_prompt: Option<&str>) -> String {
        self.build_system_prompt_with(base_profile_prompt, None)
            .await
    }

    /// Same as [`Self::build_system_prompt`], with an optional skills allow-list
    /// (`None` or an empty slice = every registered skill).
    pub async fn build_system_prompt_with(
        &self,
        base_profile_prompt: Option<&str>,
        skills_filter: Option<&[String]>,
    ) -> String {
        let hermes_dir = self.resolve_hermes_dir();
        let cwd = self.resolve_cwd();

        let mut sections: Vec<String> = Vec::new();

        // 1. SOUL.md
        let soul_candidates = vec![hermes_dir.join("SOUL.md"), cwd.join("SOUL.md")];
        if let Some(soul) = Self::read_first_existing(&soul_candidates).await {
            sections.push(format!("# SOUL\n\n{}", soul));
        }

        // 2. AGENTS.md - git root down to cwd, per directory the first non-empty
        // of AGENTS.override.md / AGENTS.md / agents.md (Hermes' precedence).
        if let Some(agents) = Self::read_agents_chain(&cwd).await {
            sections.push(format!("# OPERATIONAL DOCTRINE (AGENTS.md)\n\n{}", agents));
        }

        // 3-4. Memory + user profile. The same files have two renderings: the
        // MemoryStore's frozen snapshot (header + char usage, Hermes' shape) and a
        // raw file read. Use the store when attached, otherwise the files - never
        // both, or the same content lands in the prompt twice.
        let store_render = self.settings.inject_memory && self.memory_store.is_some();
        if !store_render {
            let mut memory_candidates = vec![
                hermes_dir.join("memories").join("MEMORY.md"),
                hermes_dir.join("MEMORY.md"),
                cwd.join("MEMORY.md"),
            ];
            if let Some(memory) = Self::read_first_existing(&memory_candidates).await {
                sections.push(format!("# MEMORY & ESTATE RULES\n\n{}", memory));
            }

            let mut user_candidates = vec![
                hermes_dir.join("memories").join("USER.md"),
                hermes_dir.join("USER.md"),
                cwd.join("USER.md"),
            ];
            if let Some(user) = Self::read_first_existing(&user_candidates).await {
                sections.push(format!("# USER PROFILE\n\n{}", user));
            }
        }

        // 5. system.md
        let system_candidates = vec![cwd.join("system.md"), hermes_dir.join("system.md")];
        if let Some(sys_md) = Self::read_first_existing(&system_candidates).await {
            sections.push(format!("# SYSTEM INSTRUCTIONS\n\n{}", sys_md));
        }

        // 6. Base profile system prompt
        if let Some(base) = base_profile_prompt {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                sections.push(format!("# PROFILE INSTRUCTIONS\n\n{}", trimmed));
            }
        }

        // 7. Memory-store snapshot (frozen at load time, so the prefix stays byte-stable).
        if let Some(store) = self
            .memory_store
            .as_ref()
            .filter(|_| self.settings.inject_memory)
        {
            if let Some(memory) = store.format_for_system_prompt("memory") {
                sections.push(format!("# MEMORY (your personal notes)\n\n{}", memory));
            }
            if let Some(user) = store.format_for_system_prompt("user") {
                sections.push(format!("# USER PROFILE (who the user is)\n\n{}", user));
            }
        }

        // 8. Compact skills index (Hermes' tier-1 shape; bodies stay pull-only).
        if let Some(registry) = self.skill_registry.as_ref() {
            if let Some(index) = self.build_skills_index(registry, skills_filter).await {
                sections.push(index);
            }
        }

        let combined = sections.join("\n\n---\n\n");
        info!(
            "PromptAssembler built system prompt (sections: {}, total bytes: {})",
            sections.len(),
            combined.len()
        );
        combined
    }
}

/// Cap for every loaded context file (Hermes: `CONTEXT_FILE_MAX_CHARS`).
pub const CONTEXT_FILE_MAX_CHARS: usize = 20_000;

/// Truncate to `cap` characters, keeping a visible marker so a trimmed file is
/// never mistaken for a complete one.
fn cap_context_file(text: String, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text;
    }
    let head: String = text.chars().take(cap).collect();
    format!("{}\n\n[... truncated at {} characters ...]", head, cap)
}

impl PromptAssembler {
    /// Directories to check for AGENTS.md: the git root first, `cwd` last
    /// (deeper wins). `cwd` alone when no git root is found - walking further up
    /// could pick up a file planted in /tmp or a home directory.
    fn agents_md_dirs(cwd: &Path) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut cur: Option<PathBuf> = Some(cwd.to_path_buf());
        while let Some(dir) = cur {
            dirs.push(dir.clone());
            if dir.join(".git").exists() {
                break;
            }
            cur = dir.parent().map(|p| p.to_path_buf());
        }
        dirs.reverse();
        dirs
    }

    /// Merged AGENTS.md chain for `cwd`, capped at [`CONTEXT_FILE_MAX_CHARS`].
    async fn read_agents_chain(cwd: &Path) -> Option<String> {
        const NAMES: [&str; 3] = ["AGENTS.override.md", "AGENTS.md", "agents.md"];
        let mut parts: Vec<String> = Vec::new();
        for dir in Self::agents_md_dirs(cwd) {
            for name in NAMES {
                let candidate = dir.join(name);
                if let Some(text) = Self::read_first_existing(&[candidate]).await {
                    parts.push(text);
                    break;
                }
            }
        }
        if parts.is_empty() {
            return None;
        }
        Some(cap_context_file(parts.join("\n\n"), CONTEXT_FILE_MAX_CHARS))
    }

    /// Compact skills index for the system prompt, in Hermes' shape: a `## Skills`
    /// header with load guidance, then one `### category` block per category with
    /// `- name: description` lines. Skill bodies are never inlined - the model
    /// pulls them with `skill_view`.
    async fn build_skills_index(
        &self,
        registry: &SkillRegistry,
        skills_filter: Option<&[String]>,
    ) -> Option<String> {
        if self.settings.skills_catalog == SkillsCatalogMode::Off {
            return None;
        }
        if let Err(e) = registry.load_all().await {
            debug!("skills index: registry load failed: {}", e);
        }
        let mut skills = registry.list().await;
        if let Some(filter) = skills_filter.filter(|f| !f.is_empty()) {
            skills.retain(|s| filter.iter().any(|want| want == &s.name));
        }
        if skills.is_empty() {
            return None;
        }
        skills.sort_by(|a, b| {
            (a.category.as_str(), a.name.as_str()).cmp(&(b.category.as_str(), b.name.as_str()))
        });

        let budget = self.settings.skill_description_budget;
        let truncate = |raw: &str| -> String {
            let desc = raw.trim();
            if desc.chars().count() <= budget {
                return desc.to_string();
            }
            let keep: String = desc.chars().take(budget.saturating_sub(3)).collect();
            format!("{}...", keep)
        };

        let mut out = String::from(
            "## Skills\nBefore replying, scan the skills below. If a skill matches, or is even \
             partially relevant to your task, you MUST load it with skill_view(name) and follow \
             its instructions. The list below is an index only - bodies load on demand.\n",
        );

        let mut current = String::new();
        for skill in &skills {
            let category = match skill.category.trim() {
                "" => "general",
                c => c,
            };
            if category != current {
                out.push_str(&format!("\n### {}\n", category));
                current = category.to_string();
            }
            match self.settings.skills_catalog {
                SkillsCatalogMode::Names => out.push_str(&format!("- {}\n", skill.name)),
                _ => {
                    let desc = truncate(&skill.description);
                    if desc.is_empty() {
                        out.push_str(&format!("- {}\n", skill.name));
                    } else {
                        out.push_str(&format!("- {}: {}\n", skill.name, desc));
                    }
                }
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod prompt_context_tests {
    use super::*;
    use std::fs;

    fn write_file(path: &std::path::Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hermes-pa-{}-{}", tag, uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn agents_md_chain_merges_git_root_down_to_cwd() {
        let tmp = temp_dir("agents");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::create_dir_all(tmp.join("sub")).unwrap();
        write_file(&tmp.join("AGENTS.md"), "ROOT RULE");
        write_file(&tmp.join("sub").join("AGENTS.md"), "SUB RULE");
        let assembler = PromptAssembler::with_overrides(Some(tmp.clone()), Some(tmp.join("sub")));
        let prompt = assembler.build_system_prompt(None).await;
        assert!(prompt.contains("ROOT RULE"), "git-root AGENTS.md missing");
        assert!(prompt.contains("SUB RULE"), "cwd AGENTS.md missing");
        assert!(
            prompt.find("ROOT RULE").unwrap() < prompt.find("SUB RULE").unwrap(),
            "git root must precede cwd"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn agents_override_wins_over_agents_md() {
        let tmp = temp_dir("override");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        write_file(&tmp.join("AGENTS.md"), "PLAIN RULE");
        write_file(&tmp.join("AGENTS.override.md"), "OVERRIDE RULE");
        let assembler = PromptAssembler::with_overrides(Some(tmp.clone()), Some(tmp.clone()));
        let prompt = assembler.build_system_prompt(None).await;
        assert!(prompt.contains("OVERRIDE RULE"), "override must win");
        assert!(
            !prompt.contains("PLAIN RULE"),
            "shadowed file must not load"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn oversized_context_file_is_capped() {
        let tmp = temp_dir("cap");
        fs::create_dir_all(tmp.join(".git")).unwrap();
        write_file(
            &tmp.join("AGENTS.md"),
            &"x".repeat(CONTEXT_FILE_MAX_CHARS + 5_000),
        );
        let assembler = PromptAssembler::with_overrides(Some(tmp.clone()), Some(tmp.clone()));
        let prompt = assembler.build_system_prompt(None).await;
        assert!(prompt.contains("truncated at"), "cap marker missing");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn memory_store_snapshot_is_injected_once() {
        let tmp = temp_dir("mem");
        let memories = tmp.join("memories");
        write_file(&memories.join("MEMORY.md"), "- remember the milk");
        write_file(&memories.join("USER.md"), "- user is Bear");
        let store = Arc::new(MemoryStore::with_base_dir(memories.clone()));
        let assembler = PromptAssembler::with_overrides(Some(tmp.clone()), Some(tmp.clone()))
            .with_memory_store(store);
        let prompt = assembler.build_system_prompt(Some("base")).await;
        assert!(
            prompt.contains("remember the milk"),
            "memory snapshot missing"
        );
        assert!(prompt.contains("user is Bear"), "user snapshot missing");
        assert_eq!(
            prompt.matches("remember the milk").count(),
            1,
            "memory content must not be injected twice"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn skills_index_carries_name_description_and_category() {
        let tmp = temp_dir("skills");
        write_file(
            &tmp.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo-skill\ndescription: Does demo things\n---\n\n# Demo\n",
        );
        let registry = Arc::new(SkillRegistry::new(vec![tmp.join("skills")]));
        let assembler = PromptAssembler::with_overrides(Some(tmp.clone()), Some(tmp.clone()))
            .with_skill_registry(registry);
        let prompt = assembler.build_system_prompt(None).await;
        assert!(prompt.contains("## Skills"), "skills header missing");
        assert!(prompt.contains("demo-skill"), "skill name missing");
        assert!(
            prompt.contains("Does demo things"),
            "skill description missing"
        );
    }

    #[tokio::test]
    async fn skills_catalog_off_omits_the_index() {
        let tmp = temp_dir("off");
        write_file(
            &tmp.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo-skill\ndescription: Does demo things\n---\n\n# Demo\n",
        );
        let registry = Arc::new(SkillRegistry::new(vec![tmp.join("skills")]));
        let settings = PromptSettings {
            skills_catalog: SkillsCatalogMode::Off,
            ..Default::default()
        };
        let assembler = PromptAssembler::with_overrides(Some(tmp.clone()), Some(tmp.clone()))
            .with_skill_registry(registry)
            .with_settings(settings);
        let prompt = assembler.build_system_prompt(None).await;
        assert!(!prompt.contains("## Skills"), "index should be off");
    }
}
