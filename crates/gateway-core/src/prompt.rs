use std::path::PathBuf;
use tracing::{debug, info};

/// Represents an assembled system prompt built from local operational doctrine,
/// soul, memory, user files, and base profile prompt.
#[derive(Debug, Clone)]
pub struct PromptAssembler {
    custom_hermes_dir: Option<PathBuf>,
    custom_cwd: Option<PathBuf>,
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
        }
    }

    /// Construct a PromptAssembler with custom directory overrides (useful for testing or non-standard paths).
    pub fn with_overrides(hermes_dir: Option<PathBuf>, cwd: Option<PathBuf>) -> Self {
        Self {
            custom_hermes_dir: hermes_dir,
            custom_cwd: cwd,
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
        PathBuf::from("/home/ubuntu/.hermes")
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
    ///
    /// Skips any missing files gracefully without error.
    pub async fn build_system_prompt(&self, base_profile_prompt: Option<&str>) -> String {
        let hermes_dir = self.resolve_hermes_dir();
        let cwd = self.resolve_cwd();

        let mut sections: Vec<String> = Vec::new();

        // 1. SOUL.md
        let soul_candidates = vec![
            hermes_dir.join("SOUL.md"),
            cwd.join("SOUL.md"),
            PathBuf::from("/home/ubuntu/.hermes/SOUL.md"),
        ];
        if let Some(soul) = Self::read_first_existing(&soul_candidates).await {
            sections.push(format!("# SOUL\n\n{}", soul));
        }

        // 2. AGENTS.md
        let agents_candidates = vec![
            cwd.join("AGENTS.md"),
            PathBuf::from("/home/ubuntu/AGENTS.md"),
            hermes_dir.join("AGENTS.md"),
        ];
        if let Some(agents) = Self::read_first_existing(&agents_candidates).await {
            sections.push(format!("# OPERATIONAL DOCTRINE (AGENTS.md)\n\n{}", agents));
        }

        // 3. MEMORY.md
        let memory_candidates = vec![
            hermes_dir.join("memories").join("MEMORY.md"),
            hermes_dir.join("MEMORY.md"),
            cwd.join("MEMORY.md"),
            PathBuf::from("/home/ubuntu/.hermes/memories/MEMORY.md"),
        ];
        if let Some(memory) = Self::read_first_existing(&memory_candidates).await {
            sections.push(format!("# MEMORY & ESTATE RULES\n\n{}", memory));
        }

        // 4. USER.md
        let user_candidates = vec![
            hermes_dir.join("memories").join("USER.md"),
            hermes_dir.join("USER.md"),
            cwd.join("USER.md"),
            PathBuf::from("/home/ubuntu/.hermes/memories/USER.md"),
        ];
        if let Some(user) = Self::read_first_existing(&user_candidates).await {
            sections.push(format!("# USER PROFILE\n\n{}", user));
        }

        // 5. system.md
        let system_candidates = vec![
            cwd.join("system.md"),
            hermes_dir.join("system.md"),
            PathBuf::from("/home/ubuntu/system.md"),
        ];
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

        let combined = sections.join("\n\n---\n\n");
        info!(
            "PromptAssembler built system prompt (sections: {}, total bytes: {})",
            sections.len(),
            combined.len()
        );
        combined
    }
}
