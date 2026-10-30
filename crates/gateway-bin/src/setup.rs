// hermes-air setup - Interactive setup wizard for Hermes Air CLI & Gateway
use anyhow::{Context, Result};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

/// Available setup wizard sections
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupSection {
    All,
    Model,
    Gateway,
    Tools,
}

impl SetupSection {
    pub fn from_str_relaxed(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" | "full" => Some(SetupSection::All),
            "model" | "provider" | "models" | "providers" | "agent" => Some(SetupSection::Model),
            "gateway" | "platform" | "platforms" | "messaging" => Some(SetupSection::Gateway),
            "tools" | "tool" | "browser" | "cdp" => Some(SetupSection::Tools),
            _ => None,
        }
    }

    pub fn valid_section_names() -> &'static [&'static str] {
        &["model", "gateway", "tools"]
    }
}

/// Model & Provider configuration state
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProviderState {
    pub provider_choice: String,
    pub provider_name: String,
    pub api_key: Option<String>,
    pub model_name: String,
    pub base_url: Option<String>,
}

impl Default for ModelProviderState {
    fn default() -> Self {
        Self {
            provider_choice: "1".to_string(),
            provider_name: "openai".to_string(),
            api_key: None,
            model_name: "gpt-4o".to_string(),
            base_url: Some("https://api.openai.com/v1".to_string()),
        }
    }
}

/// Messaging platforms configuration state
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatewayPlatformsState {
    pub enable_telegram: bool,
    pub telegram_bot_token: Option<String>,
    pub enable_discord: bool,
    pub discord_bot_token: Option<String>,
}

/// Tools & Browser configuration state
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserToolsState {
    pub enable_cdp: bool,
    pub cdp_url: String,
}

impl Default for BrowserToolsState {
    fn default() -> Self {
        Self {
            enable_cdp: true,
            cdp_url: "http://127.0.0.1:9222".to_string(),
        }
    }
}

/// Aggregate Wizard configuration state
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupWizardState {
    pub model: ModelProviderState,
    pub gateway: GatewayPlatformsState,
    pub browser: BrowserToolsState,
}

pub struct SetupIo<R, W> {
    pub reader: R,
    pub writer: W,
}

impl<R: BufRead, W: Write> SetupIo<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    pub fn prompt_line(&mut self, question: &str, default: Option<&str>) -> Result<String> {
        if let Some(def) = default {
            write!(self.writer, "{} [{}]: ", question, def)?;
        } else {
            write!(self.writer, "{}: ", question)?;
        }
        self.writer.flush()?;

        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            Ok(default.unwrap_or("").to_string())
        } else {
            Ok(trimmed.to_string())
        }
    }

    pub fn prompt_yes_no(&mut self, question: &str, default: bool) -> Result<bool> {
        let hint = if default { "Y/n" } else { "y/N" };
        let default_str = if default { "y" } else { "n" };
        loop {
            write!(self.writer, "{} [{}]: ", question, hint)?;
            self.writer.flush()?;

            let mut line = String::new();
            self.reader.read_line(&mut line)?;
            let trimmed = line.trim().to_ascii_lowercase();
            let ans = if trimmed.is_empty() {
                default_str
            } else {
                trimmed.as_str()
            };

            match ans {
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => {
                    writeln!(self.writer, "  Please enter 'y' or 'n'")?;
                }
            }
        }
    }

    pub fn prompt_masked(&mut self, question: &str, default: Option<&str>) -> Result<String> {
        self.prompt_line(question, default)
    }

    pub fn print_banner(&mut self) -> Result<()> {
        writeln!(self.writer)?;
        writeln!(self.writer, "====================================================")?;
        writeln!(self.writer, "🌿 Hermes Air 🍃 - Interactive Setup Wizard")?;
        writeln!(self.writer, "Configuration & Credentials Manager")?;
        writeln!(self.writer, "====================================================")?;
        writeln!(self.writer)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn print_header(&mut self, title: &str) -> Result<()> {
        writeln!(self.writer)?;
        writeln!(self.writer, "\x1b[1;36m◆ {}\x1b[0m", title)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn print_info(&mut self, text: &str) -> Result<()> {
        writeln!(self.writer, "  \x1b[34mℹ\x1b[0m {}", text)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn print_success(&mut self, text: &str) -> Result<()> {
        writeln!(self.writer, "  \x1b[32m✔\x1b[0m {}", text)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn print_warning(&mut self, text: &str) -> Result<()> {
        writeln!(self.writer, "  \x1b[33m▲\x1b[0m {}", text)?;
        self.writer.flush()?;
        Ok(())
    }
}

/// Step 1: LLM Provider Selection
pub fn run_model_provider_step<R: BufRead, W: Write>(
    io: &mut SetupIo<R, W>,
    state: &mut ModelProviderState,
) -> Result<()> {
    io.print_header("Step 1: LLM Provider Selection")?;
    io.print_info("Select inference provider:")?;
    writeln!(io.writer, "  1) OpenAI (api.openai.com)")?;
    writeln!(io.writer, "  2) OpenRouter (openrouter.ai)")?;
    writeln!(io.writer, "  3) Anthropic (api.anthropic.com)")?;
    writeln!(io.writer, "  4) Google Gemini (generativelanguage.googleapis.com)")?;
    writeln!(io.writer, "  5) Custom OpenAI-Compatible (e.g. 9Router, Local Ollama, vLLM)")?;

    let choice = io.prompt_line("Provider choice [1-5]", Some("1"))?;
    let (prov_name, default_model, default_base_url) = match choice.as_str() {
        "2" => (
            "openrouter",
            "anthropic/claude-sonnet-4.6",
            Some("https://openrouter.ai/api/v1"),
        ),
        "3" => (
            "anthropic",
            "claude-3-5-sonnet-20241022",
            Some("https://api.anthropic.com/v1"),
        ),
        "4" => (
            "gemini",
            "gemini-2.5-flash",
            Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        ),
        "5" => ("custom", "default-model", None),
        _ => ("openai", "gpt-4o", Some("https://api.openai.com/v1")),
    };

    state.provider_choice = choice;
    state.provider_name = prov_name.to_string();

    let key = io.prompt_masked(&format!("Enter API Key for {}", prov_name), None)?;
    if !key.is_empty() {
        state.api_key = Some(key);
    }

    let model = io.prompt_line("Default model name", Some(default_model))?;
    state.model_name = model;

    if prov_name == "custom" {
        let base = io.prompt_line("Custom Base URL", Some("http://127.0.0.1:8000/v1"))?;
        state.base_url = Some(base);
    } else {
        state.base_url = default_base_url.map(String::from);
    }

    io.print_success(&format!(
        "Configured provider '{}' with model '{}'",
        state.provider_name, state.model_name
    ))?;
    Ok(())
}

/// Step 2: Messaging Gateways (Optional)
pub fn run_messaging_gateway_step<R: BufRead, W: Write>(
    io: &mut SetupIo<R, W>,
    state: &mut GatewayPlatformsState,
) -> Result<()> {
    io.print_header("Step 2: Messaging Gateways (Optional)")?;

    // Enable Telegram Bot? (y/N) -> prompt for TELEGRAM_BOT_TOKEN
    let enable_tg = io.prompt_yes_no("Enable Telegram Bot?", false)?;
    state.enable_telegram = enable_tg;
    if enable_tg {
        let token = io.prompt_masked("TELEGRAM_BOT_TOKEN", None)?;
        if !token.is_empty() {
            state.telegram_bot_token = Some(token);
            io.print_success("Telegram Bot Token configured")?;
        } else {
            io.print_warning("No token provided, Telegram Bot disabled")?;
            state.enable_telegram = false;
        }
    }

    // Enable Discord Bot? (y/N) -> prompt for DISCORD_BOT_TOKEN
    let enable_discord = io.prompt_yes_no("Enable Discord Bot?", false)?;
    state.enable_discord = enable_discord;
    if enable_discord {
        let token = io.prompt_masked("DISCORD_BOT_TOKEN", None)?;
        if !token.is_empty() {
            state.discord_bot_token = Some(token);
            io.print_success("Discord Bot Token configured")?;
        } else {
            io.print_warning("No token provided, Discord Bot disabled")?;
            state.enable_discord = false;
        }
    }

    Ok(())
}

/// Step 3: Browser & Tools Settings
pub fn run_browser_tools_step<R: BufRead, W: Write>(
    io: &mut SetupIo<R, W>,
    state: &mut BrowserToolsState,
) -> Result<()> {
    io.print_header("Step 3: Browser & Tools Settings")?;

    // Enable Chrome DevTools Protocol (CDP) Browser Automation? (Y/n)
    let enable_cdp = io.prompt_yes_no(
        "Enable Chrome DevTools Protocol (CDP) Browser Automation?",
        true,
    )?;
    state.enable_cdp = enable_cdp;

    if enable_cdp {
        let default_url = if state.cdp_url.is_empty() {
            "http://127.0.0.1:9222"
        } else {
            &state.cdp_url
        };
        let cdp = io.prompt_line("CDP URL", Some(default_url))?;
        state.cdp_url = cdp;
        io.print_success(&format!("Browser CDP endpoint set to: {}", state.cdp_url))?;
    } else {
        io.print_info("CDP Browser Automation disabled")?;
    }

    Ok(())
}

/// Generates valid, structured TOML text representation of the complete configuration matching Hermes config schema
pub fn generate_config_toml(state: &SetupWizardState) -> String {
    let mut toml = String::new();

    toml.push_str("# Hermes Air - Configuration\n\n");

    // [gateway]
    toml.push_str("[gateway]\n");
    toml.push_str("name = \"hermes-air\"\n");
    toml.push_str("version = \"0.1.0\"\n");
    toml.push_str("max_concurrent_sessions = 100\n");
    toml.push_str("default_profile = \"default\"\n");
    toml.push_str("shutdown_timeout_secs = 30\n");
    toml.push_str("restart_on_config_change = false\n\n");

    // [profiles.default]
    toml.push_str("[profiles.default]\n");
    toml.push_str("name = \"Hermes\"\n");
    toml.push_str(&format!(
        "model_provider = \"{}\"\n",
        state.model.provider_name
    ));
    toml.push_str(&format!("model = \"{}\"\n", state.model.model_name));
    toml.push_str("temperature = 0.7\n");
    toml.push_str("max_tokens = 4096\n");
    toml.push_str("system_prompt = \"You are Hermes, a helpful, capable, and unfiltered personal AI assistant.\"\n");

    // Tools list
    let mut tools = vec![
        "terminal".to_string(),
        "read_file".to_string(),
        "write_file".to_string(),
        "patch".to_string(),
        "search_files".to_string(),
        "web_search".to_string(),
        "web_extract".to_string(),
        "execute_code".to_string(),
        "skills_list".to_string(),
        "skill_view".to_string(),
    ];
    if state.browser.enable_cdp {
        tools.push("browser_navigate".to_string());
        tools.push("browser_snapshot".to_string());
        tools.push("browser_click".to_string());
        tools.push("browser_type".to_string());
        tools.push("browser_exec".to_string());
    }
    let tools_formatted: Vec<String> = tools.iter().map(|t| format!("\"{}\"", t)).collect();
    toml.push_str(&format!("tools = [{}]\n", tools_formatted.join(", ")));
    toml.push_str("skills = []\n");
    toml.push_str("mcp_servers = []\n");
    toml.push_str("platform_overrides = {}\n\n");

    // [platforms.telegram]
    let tg_enabled = state.gateway.enable_telegram && state.gateway.telegram_bot_token.is_some();
    let tg_token = state.gateway.telegram_bot_token.as_deref().unwrap_or("");
    toml.push_str("[platforms.telegram]\n");
    toml.push_str(&format!("enabled = {}\n", tg_enabled));
    toml.push_str("adapter = \"telegram\"\n");
    toml.push_str(&format!(
        "config = {{ enabled = {}, bot_token = \"{}\", timeout_secs = 30, allowed_updates = [\"message\"], drop_pending_updates = false, custom = {{}} }}\n\n",
        tg_enabled, tg_token
    ));

    // [platforms.discord]
    let discord_enabled =
        state.gateway.enable_discord && state.gateway.discord_bot_token.is_some();
    let discord_token = state.gateway.discord_bot_token.as_deref().unwrap_or("");
    toml.push_str("[platforms.discord]\n");
    toml.push_str(&format!("enabled = {}\n", discord_enabled));
    toml.push_str("adapter = \"discord\"\n");
    toml.push_str(&format!(
        "config = {{ enabled = {}, bot_token = \"{}\", timeout_secs = 30, custom = {{}} }}\n\n",
        discord_enabled, discord_token
    ));

    // [plugins]
    toml.push_str("[plugins]\n");
    toml.push_str("enabled = false\n");
    toml.push_str("directory = \"plugins\"\n");
    toml.push_str("python_plugins = false\n");
    toml.push_str("allowed_plugins = []\n");
    toml.push_str("blocked_plugins = []\n\n");

    // [session_store]
    toml.push_str("[session_store]\n");
    toml.push_str("backend = \"memory\"\n");
    toml.push_str("local_cache_size = 1000\n");
    toml.push_str("default_ttl_secs = 86400\n\n");

    // [logging]
    toml.push_str("[logging]\n");
    toml.push_str("level = \"info\"\n");
    toml.push_str("format = \"text\"\n");
    toml.push_str("output = \"stdout\"\n");
    toml.push_str("file = \"hermes-gateway.log\"\n");
    toml.push_str("max_file_size_mb = 100\n");
    toml.push_str("max_files = 5\n\n");

    // [metrics]
    toml.push_str("[metrics]\n");
    toml.push_str("enabled = false\n");
    toml.push_str("port = 9090\n");
    toml.push_str("path = \"/metrics\"\n\n");

    // [dashboard]
    toml.push_str("[dashboard]\n");
    toml.push_str("enabled = false\n");
    toml.push_str("host = \"127.0.0.1\"\n");
    toml.push_str("port = 8080\n\n");

    // [tui]
    toml.push_str("[tui]\n");
    toml.push_str("enabled = false\n\n");

    // [acp]
    toml.push_str("[acp]\n");
    toml.push_str("enabled = false\n");
    toml.push_str("port = 9095\n");

    toml
}

/// Saves wizard configuration state to target file and environment
pub fn save_wizard_config(path: &Path, state: &SetupWizardState) -> Result<()> {
    let toml_str = generate_config_toml(state);

    // Validate TOML syntax via hermes_config loader parsing
    let _: hermes_config::GatewayConfig = toml::from_str(&toml_str)
        .context("Generated TOML configuration failed schema validation")?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, toml_str)?;

    // Persist environment variables if API key provided
    if let Some(ref key) = state.model.api_key {
        match state.model.provider_name.as_str() {
            "openai" => std::env::set_var("OPENAI_API_KEY", key),
            "openrouter" => std::env::set_var("OPENROUTER_API_KEY", key),
            "anthropic" => std::env::set_var("ANTHROPIC_API_KEY", key),
            "gemini" => std::env::set_var("GEMINI_API_KEY", key),
            _ => std::env::set_var("OPENAI_API_KEY", key),
        }
    }

    if let Some(ref base) = state.model.base_url {
        std::env::set_var("OPENAI_BASE_URL", base);
    }

    if state.browser.enable_cdp {
        std::env::set_var("BROWSER_CDP_URL", &state.browser.cdp_url);
    }

    if let Some(ref token) = state.gateway.telegram_bot_token {
        std::env::set_var("TELEGRAM_BOT_TOKEN", token);
    }

    if let Some(ref token) = state.gateway.discord_bot_token {
        std::env::set_var("DISCORD_BOT_TOKEN", token);
    }

    Ok(())
}

/// Runs the complete setup wizard or a specific section
pub fn run_setup_wizard(config_path_str: &str, section_arg: Option<&str>) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut io = SetupIo::new(stdin.lock(), stdout.lock());

    let target_section = match section_arg {
        Some(s) => match SetupSection::from_str_relaxed(s) {
            Some(sec) => sec,
            None => {
                io.print_warning(&format!(
                    "Unknown section '{}'. Available sections: {}",
                    s,
                    SetupSection::valid_section_names().join(", ")
                ))?;
                return Ok(());
            }
        },
        None => SetupSection::All,
    };

    let path = PathBuf::from(config_path_str);
    let mut state = SetupWizardState::default();

    // If an existing configuration file exists, load initial parameters
    if path.exists() {
        if let Ok(existing_cfg) = hermes_config::ConfigLoader::load(&path) {
            if let Some(profile) = existing_cfg.profiles.get("default") {
                state.model.provider_name = profile.model_provider.clone();
                state.model.model_name = profile.model.clone();
            }
            if let Some(tg) = existing_cfg.platforms.get("telegram") {
                if tg.enabled {
                    state.gateway.enable_telegram = true;
                    if let Some(token) = tg.config.get("bot_token").and_then(|v| v.as_str()) {
                        state.gateway.telegram_bot_token = Some(token.to_string());
                    }
                }
            }
            if let Some(discord) = existing_cfg.platforms.get("discord") {
                if discord.enabled {
                    state.gateway.enable_discord = true;
                    if let Some(token) = discord.config.get("bot_token").and_then(|v| v.as_str()) {
                        state.gateway.discord_bot_token = Some(token.to_string());
                    }
                }
            }
        }
    }

    io.print_banner()?;

    match target_section {
        SetupSection::All => {
            // Step 1: LLM Provider Selection
            run_model_provider_step(&mut io, &mut state.model)?;

            // Step 2: Messaging Gateways (Optional)
            run_messaging_gateway_step(&mut io, &mut state.gateway)?;

            // Step 3: Browser & Tools Settings
            run_browser_tools_step(&mut io, &mut state.browser)?;

            // Step 4: Write Configuration
            io.print_header("Step 4: Write Configuration")?;
            save_wizard_config(&path, &state)?;
            writeln!(
                io.writer,
                "✅ Configuration successfully saved to {}!",
                path.display()
            )?;
            writeln!(
                io.writer,
                "✨ You can now run 'hermes-air' to start chatting or 'hermes-air gateway' to start messaging services."
            )?;
            writeln!(io.writer)?;
            io.writer.flush()?;
        }
        SetupSection::Model => {
            run_model_provider_step(&mut io, &mut state.model)?;
            save_wizard_config(&path, &state)?;
            writeln!(
                io.writer,
                "✅ Configuration successfully saved to {}!",
                path.display()
            )?;
        }
        SetupSection::Gateway => {
            run_messaging_gateway_step(&mut io, &mut state.gateway)?;
            save_wizard_config(&path, &state)?;
            writeln!(
                io.writer,
                "✅ Configuration successfully saved to {}!",
                path.display()
            )?;
        }
        SetupSection::Tools => {
            run_browser_tools_step(&mut io, &mut state.browser)?;
            save_wizard_config(&path, &state)?;
            writeln!(
                io.writer,
                "✅ Configuration successfully saved to {}!",
                path.display()
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_section_parsing() {
        assert_eq!(
            SetupSection::from_str_relaxed("model"),
            Some(SetupSection::Model)
        );
        assert_eq!(
            SetupSection::from_str_relaxed("provider"),
            Some(SetupSection::Model)
        );
        assert_eq!(
            SetupSection::from_str_relaxed("gateway"),
            Some(SetupSection::Gateway)
        );
        assert_eq!(
            SetupSection::from_str_relaxed("platforms"),
            Some(SetupSection::Gateway)
        );
        assert_eq!(
            SetupSection::from_str_relaxed("tools"),
            Some(SetupSection::Tools)
        );
        assert_eq!(
            SetupSection::from_str_relaxed("cdp"),
            Some(SetupSection::Tools)
        );
        assert_eq!(
            SetupSection::from_str_relaxed("all"),
            Some(SetupSection::All)
        );
        assert_eq!(SetupSection::from_str_relaxed("unknown_foo"), None);
    }

    #[test]
    fn test_model_provider_step_openai() {
        let input = "1\nsk-test-openai-key\n\n";
        let mut reader = Cursor::new(input);
        let mut writer = Vec::new();
        let mut io = SetupIo::new(&mut reader, &mut writer);

        let mut state = ModelProviderState::default();
        run_model_provider_step(&mut io, &mut state).unwrap();

        assert_eq!(state.provider_name, "openai");
        assert_eq!(state.api_key.as_deref(), Some("sk-test-openai-key"));
        assert_eq!(state.model_name, "gpt-4o");
        assert_eq!(state.base_url.as_deref(), Some("https://api.openai.com/v1"));
    }

    #[test]
    fn test_model_provider_step_custom() {
        let input = "5\ncustom-key\nmy-custom-model\nhttp://127.0.0.1:20128/v1\n";
        let mut reader = Cursor::new(input);
        let mut writer = Vec::new();
        let mut io = SetupIo::new(&mut reader, &mut writer);

        let mut state = ModelProviderState::default();
        run_model_provider_step(&mut io, &mut state).unwrap();

        assert_eq!(state.provider_name, "custom");
        assert_eq!(state.api_key.as_deref(), Some("custom-key"));
        assert_eq!(state.model_name, "my-custom-model");
        assert_eq!(state.base_url.as_deref(), Some("http://127.0.0.1:20128/v1"));
    }

    #[test]
    fn test_messaging_gateway_step_prompting() {
        let input = "y\n123456:TG_TOKEN\ny\nDISCORD_TOKEN\n";
        let mut reader = Cursor::new(input);
        let mut writer = Vec::new();
        let mut io = SetupIo::new(&mut reader, &mut writer);

        let mut state = GatewayPlatformsState::default();
        run_messaging_gateway_step(&mut io, &mut state).unwrap();

        assert!(state.enable_telegram);
        assert_eq!(state.telegram_bot_token.as_deref(), Some("123456:TG_TOKEN"));
        assert!(state.enable_discord);
        assert_eq!(state.discord_bot_token.as_deref(), Some("DISCORD_TOKEN"));
    }

    #[test]
    fn test_browser_tools_step_prompting() {
        let input = "y\nhttp://127.0.0.1:9222\n";
        let mut reader = Cursor::new(input);
        let mut writer = Vec::new();
        let mut io = SetupIo::new(&mut reader, &mut writer);

        let mut state = BrowserToolsState::default();
        run_browser_tools_step(&mut io, &mut state).unwrap();

        assert!(state.enable_cdp);
        assert_eq!(state.cdp_url, "http://127.0.0.1:9222");
    }

    #[test]
    fn test_generate_config_toml_syntax_and_validity() {
        let state = SetupWizardState {
            model: ModelProviderState {
                provider_choice: "2".to_string(),
                provider_name: "openrouter".to_string(),
                api_key: Some("sk-or-test".to_string()),
                model_name: "anthropic/claude-sonnet-4.6".to_string(),
                base_url: Some("https://openrouter.ai/api/v1".to_string()),
            },
            gateway: GatewayPlatformsState {
                enable_telegram: true,
                telegram_bot_token: Some("123456:ABC-DEF1234ghIkl".to_string()),
                enable_discord: true,
                discord_bot_token: Some("discord-test-token".to_string()),
            },
            browser: BrowserToolsState {
                enable_cdp: true,
                cdp_url: "http://127.0.0.1:9222".to_string(),
            },
        };

        let toml_str = generate_config_toml(&state);
        assert!(toml_str.contains("model_provider = \"openrouter\""));
        assert!(toml_str.contains("model = \"anthropic/claude-sonnet-4.6\""));
        assert!(toml_str.contains("adapter = \"telegram\""));
        assert!(toml_str.contains("123456:ABC-DEF1234ghIkl"));
        assert!(toml_str.contains("adapter = \"discord\""));
        assert!(toml_str.contains("discord-test-token"));
        assert!(toml_str.contains("\"browser_navigate\""));

        let parsed: Result<hermes_config::GatewayConfig, _> = toml::from_str(&toml_str);
        assert!(
            parsed.is_ok(),
            "Generated TOML failed to parse: {:?}",
            parsed.err()
        );
    }

    #[test]
    fn test_save_wizard_config_to_temp_file() {
        let dir = std::env::temp_dir().join(format!("hermes-setup-test-{}", std::process::id()));
        let file_path = dir.join("test_config.toml");

        let state = SetupWizardState::default();
        let res = save_wizard_config(&file_path, &state);
        assert!(res.is_ok());
        assert!(file_path.exists());

        let loaded = hermes_config::ConfigLoader::load(&file_path);
        assert!(loaded.is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
