use std::time::Instant;
use hermes_platform_telegram::{InlineKeyboardButton, InlineKeyboardMarkup};

/// Active Slash Commands supported by Hermes native Gateway
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Start,
    Help,
    Status,
    Model(Option<String>),
    Clear,
}

/// Parse incoming prompt or message text for slash commands.
/// Matches commands like /start, /help, /status, /model [name], /clear, /reset.
/// Handles Telegram bot mentions (e.g. /status@HermesBot) and leading/trailing whitespace.
pub fn parse_command(text: &str) -> Option<SlashCommand> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let raw_cmd = parts.next()?;
    let args = parts.next().map(str::trim).filter(|s| !s.is_empty());

    // Strip bot mention if present, e.g. /status@HermesBot -> /status
    let cmd_name = raw_cmd.split('@').next()?.to_lowercase();

    match cmd_name.as_str() {
        "/start" => Some(SlashCommand::Start),
        "/help" | "/commands" => Some(SlashCommand::Help),
        "/status" => Some(SlashCommand::Status),
        "/model" => Some(SlashCommand::Model(args.map(|s| s.to_string()))),
        "/clear" | "/reset" | "/new" => Some(SlashCommand::Clear),
        _ => None,
    }
}

/// Global system start instant for uptime calculations
static START_INSTANT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

pub fn init_uptime() {
    START_INSTANT.get_or_init(Instant::now);
}

pub fn get_uptime_secs() -> u64 {
    START_INSTANT.get_or_init(Instant::now).elapsed().as_secs()
}

pub fn format_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if days > 0 {
        format!("{}d {}h {}m {}s", days, hours, minutes, seconds)
    } else if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}

/// Runtime Context provided to command execution
pub struct CommandContext<'a> {
    pub gateway_name: &'a str,
    pub gateway_version: &'a str,
    pub active_profile: &'a str,
    pub active_model: &'a str,
    pub telegram_connected: bool,
    pub memory_stats: Option<String>,
}

/// Format the Greeting for /start and /help
pub fn handle_start_or_help(ctx: &CommandContext) -> String {
    format!(
        "<b>Greetings!</b>\n\nI am your Hermes Gateway assistant. All systems are operational.\n\n<b>Active Model:</b> <code>{}</code>\n<b>Profile:</b> <code>{}</code>\n\n<b>Available Slash Commands:</b>\n• <code>/start</code> / <code>/help</code> — Show this greeting & command overview\n• <code>/status</code> — Display full system diagnostics, uptime & platform status\n• <code>/model</code> — View current active model & provider info\n• <code>/model &lt;name&gt;</code> — Dynamically switch active model (e.g. <code>/model ag/gemini-3.8-flash</code>)\n• <code>/clear</code> / <code>/reset</code> — Reset conversation turn memory & session",
        ctx.active_model, ctx.active_profile
    )
}

/// Format diagnostics overview for /status
pub fn handle_status(ctx: &CommandContext) -> String {
    let uptime_str = format_uptime(get_uptime_secs());
    let memory_info = ctx
        .memory_stats
        .clone()
        .unwrap_or_else(get_process_memory_info);

    let tg_status = if ctx.telegram_connected {
        "Connected"
    } else {
        "Disconnected"
    };

    format!(
"<b>{} Gateway Status</b>

<b>Directives (One-Tap Copy):</b>
<code>{}</code>
<code>{}</code>
<code>{}</code>
<code>{}</code>
<code>{}</code>
<code>{}</code>

<b>Diagnostic Overview:</b>
• <b>Gateway:</b> <code>{}</code>
• <b>Uptime:</b> <code>{}</code>
• <b>Profile:</b> <code>{}</code>
• <b>Model:</b> <code>{}</code>
• <b>Telegram:</b> <code>{}</code>
• <b>Memory:</b> <code>{}</code>

<i>Status: Operational</i>",
        ctx.gateway_name,
        format!("gateway: {} v{}", ctx.gateway_name, ctx.gateway_version),
        format!("uptime: {}", uptime_str),
        format!("profile: {}", ctx.active_profile),
        format!("model: {}", ctx.active_model),
        format!("telegram: {}", tg_status),
        format!("ram: {}", memory_info),
        format!("{} v{}", ctx.gateway_name, ctx.gateway_version),
        uptime_str,
        ctx.active_profile,
        ctx.active_model,
        tg_status,
        memory_info
    )
}

/// Handle /model command
pub fn handle_model(
    arg: &Option<String>,
    current_model: &str,
    on_switch: impl FnOnce(&str) -> Result<(), String>,
) -> String {
    match arg {
        None => {
            format!(
                "🤖 <b>Active Model Configuration</b>\n\n• <b>Current Model:</b> <code>{}</code>\n\nTo switch models dynamically for this gateway session, run:\n<code>/model &lt;model_name&gt;</code>\n\n<b>Examples:</b>\n• <code>/model ag/gemini-3.8-flash</code>\n• <code>/model freet/claude-3-7-sonnet</code>\n• <code>/model gpt-4o</code>",
                current_model
            )
        }
        Some(new_model) => {
            let target = new_model.trim();
            if target.is_empty() {
                return "❌ Model name cannot be empty.".to_string();
            }

            match on_switch(target) {
                Ok(_) => {
                    format!(
                        "✅ <b>Model Switched Successfully</b>\n\nActive model is now set to: <code>{}</code>\nSubsequent conversation turns will use this model.",
                        target
                    )
                }
                Err(e) => {
                    format!("❌ Failed to switch model: {}", e)
                }
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Interactive Model Picker UI & Keyboards (1-to-1 Blueprint Parity)
// -----------------------------------------------------------------------------

pub const MODEL_PAGE_SIZE: usize = 8;

pub struct ProviderItem {
    pub slug: String,
    pub name: String,
}

pub fn default_providers_list() -> Vec<ProviderItem> {
    vec![
        ProviderItem { slug: "9router".to_string(), name: "9Router Gateway".to_string() },
        ProviderItem { slug: "tokenharbor".to_string(), name: "TokenHarbor".to_string() },
        ProviderItem { slug: "openrouter".to_string(), name: "OpenRouter".to_string() },
        ProviderItem { slug: "anthropic".to_string(), name: "Anthropic".to_string() },
        ProviderItem { slug: "openai".to_string(), name: "OpenAI".to_string() },
    ]
}

/// Build the provider selection keyboard.
/// Displays active provider with ⭐, lays out 2 buttons per row,
/// and adds bottom navigation: [🔄 Refresh] (mrf:prov) and [✗ Cancel] (mx:cancel).
pub fn build_provider_keyboard(providers: &[ProviderItem], active_provider: &str) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut current_row: Vec<InlineKeyboardButton> = Vec::new();

    for p in providers {
        let is_current = p.slug.eq_ignore_ascii_case(active_provider)
            || (active_provider.is_empty() && p.slug == "9router");
        let label = if is_current {
            format!("⭐ {}", p.name)
        } else {
            p.name.clone()
        };

        current_row.push(InlineKeyboardButton::new(label, format!("mp:{}", p.slug)));
        if current_row.len() == 2 {
            rows.push(std::mem::take(&mut current_row));
        }
    }

    if !current_row.is_empty() {
        rows.push(current_row);
    }

    // Bottom action row: [🔄 Refresh], [✗ Cancel]
    rows.push(vec![
        InlineKeyboardButton::new("🔄 Refresh", "mrf:prov"),
        InlineKeyboardButton::new("✗ Cancel", "mx:cancel"),
    ]);

    InlineKeyboardMarkup::new(rows)
}

/// Header text for the Provider selection screen
pub fn provider_picker_header(active_model: &str, active_provider: &str) -> String {
    let prov = if active_provider.is_empty() {
        "9Router Gateway"
    } else {
        active_provider
    };

    format!(
        "⚡ <b>HERMES ROUTER // MODEL SELECTOR</b>\n\n<b>Active model:</b> <code>{}</code> ⭐\n<b>Provider:</b> <code>{}</code>\n\nSelect a provider:",
        active_model, prov
    )
}

/// Build the paginated model selection keyboard.
/// Displays active model with ⭐, pagination [◀ Prev], [page/total], [Next ▶],
/// and bottom row [⬅ Back] (mb:back), [🔄 Refresh] (mrf), [✗ Cancel] (mx:cancel).
pub fn build_model_keyboard(
    models: &[String],
    page: usize,
    active_model: &str,
    provider_slug: &str,
) -> (InlineKeyboardMarkup, usize, usize) {
    let total_models = models.len();
    let total_pages = if total_models == 0 {
        1
    } else {
        (total_models + MODEL_PAGE_SIZE - 1) / MODEL_PAGE_SIZE
    };
    let current_page = std::cmp::min(page, total_pages.saturating_sub(1));

    let start_idx = current_page * MODEL_PAGE_SIZE;
    let end_idx = std::cmp::min(start_idx + MODEL_PAGE_SIZE, total_models);
    let page_models = if start_idx < total_models {
        &models[start_idx..end_idx]
    } else {
        &[][..]
    };

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    // Model buttons: 1 or 2 per row
    for model_id in page_models {
        let short = model_id.split('/').last().unwrap_or(model_id);
        let is_active = model_id == active_model || short == active_model;
        let mut display_name = if is_active {
            format!("⭐ {}", short)
        } else {
            short.to_string()
        };

        if display_name.len() > 36 {
            display_name = format!("{}...", &display_name[..33]);
        }

        // Each model button callbacks with `mm:<model_id>`
        rows.push(vec![InlineKeyboardButton::new(display_name, format!("mm:{}", model_id))]);
    }

    // Pagination row if total_pages > 1
    if total_pages > 1 {
        let mut nav_row = Vec::new();
        if current_page > 0 {
            nav_row.push(InlineKeyboardButton::new(
                "◀ Prev",
                format!("mg:{}:{}", provider_slug, current_page - 1),
            ));
        }
        nav_row.push(InlineKeyboardButton::new(
            format!("[{}/{}]", current_page + 1, total_pages),
            "mx:noop",
        ));
        if current_page + 1 < total_pages {
            nav_row.push(InlineKeyboardButton::new(
                "Next ▶",
                format!("mg:{}:{}", provider_slug, current_page + 1),
            ));
        }
        rows.push(nav_row);
    }

    // Tail control row: [⬅ Back], [🔄 Refresh], [✗ Cancel]
    rows.push(vec![
        InlineKeyboardButton::new("⬅ Back", "mb:back"),
        InlineKeyboardButton::new("🔄 Refresh", format!("mrf:{}", provider_slug)),
        InlineKeyboardButton::new("✗ Cancel", "mx:cancel"),
    ]);

    (InlineKeyboardMarkup::new(rows), current_page + 1, total_pages)
}

/// Header card text for Model selection screen matching Python blueprint
pub fn model_picker_card(
    provider_name: &str,
    total_models: usize,
    current_page: usize,
    total_pages: usize,
    page_models: &[String],
    active_model: &str,
) -> String {
    let mut preview_lines = Vec::new();
    for mid in page_models {
        let is_active = mid == active_model || mid.split('/').last().unwrap_or(mid) == active_model;
        let marker = if is_active { "⭐ " } else { "  " };

        let mid_lower = mid.to_lowercase();
        let badge = if mid_lower.contains("flash")
            || mid_lower.contains("turbo")
            || mid_lower.contains("mini")
            || mid_lower.contains("3.8")
            || mid_lower.contains("3.7")
        {
            "🟢 ~0.1s"
        } else if mid_lower.contains("deepseek") || mid_lower.contains("gemma") || mid_lower.contains("free") {
            "🟢 ~0.3s"
        } else if mid_lower.contains("grok") || mid_lower.contains("luna") || mid_lower.contains("atria") {
            "🟡 ~0.8s"
        } else {
            "🟢 ~0.2s"
        };

        let mut display = mid.clone();
        if display.len() > 22 {
            display = format!("{}...", &display[..19]);
        }
        preview_lines.push(format!("{}{:<22} {}", marker, display, badge));
    }

    let preview_block = if preview_lines.is_empty() {
        "• none".to_string()
    } else {
        preview_lines.join("\n")
    };

    format!(
"⚡ <b>HERMES ROUTER // MODEL SELECTOR</b>
<pre>
PROVIDER : {}
GATEWAY  : 🟢 ~0.1s (9Router Oracle)
POOL     : {} models available [{}/{}]
──────────────────────────────────────
MODELS ON THIS PAGE & SPEED:
{}
</pre>
<b>Select a target model below:</b>",
        provider_name, total_models, current_page, total_pages, preview_block
    )
}

/// Handle /clear or /reset command
pub fn handle_clear() -> String {
    "🧹 <b>Session Reset</b>\n\nConversation turn memory has been cleared. Beginning a fresh session."
        .to_string()
}

/// Main dispatch entry point for slash commands
pub fn handle_command(
    cmd: SlashCommand,
    ctx: &CommandContext,
    on_switch_model: impl FnOnce(&str) -> Result<(), String>,
) -> String {
    match cmd {
        SlashCommand::Start | SlashCommand::Help => handle_start_or_help(ctx),
        SlashCommand::Status => handle_status(ctx),
        SlashCommand::Model(arg) => handle_model(&arg, ctx.active_model, on_switch_model),
        SlashCommand::Clear => handle_clear(),
    }
}

/// Helper to read process memory stats from /proc/self/statm (Linux) or fallback
fn get_process_memory_info() -> String {
    if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
        let parts: Vec<&str> = statm.split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(total_pages), Ok(resident_pages)) =
                (parts[0].parse::<u64>(), parts[1].parse::<u64>())
            {
                let page_size_kb = 4;
                let rss_mb = (resident_pages * page_size_kb) as f64 / 1024.0;
                let virt_mb = (total_pages * page_size_kb) as f64 / 1024.0;
                return format!("{:.1} MB RSS / {:.1} MB VIRT", rss_mb, virt_mb);
            }
        }
    }
    "N/A".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_commands() {
        assert_eq!(parse_command("/start"), Some(SlashCommand::Start));
        assert_eq!(parse_command("  /help  "), Some(SlashCommand::Help));
        assert_eq!(parse_command("/commands"), Some(SlashCommand::Help));
        assert_eq!(parse_command("/status"), Some(SlashCommand::Status));
        assert_eq!(parse_command("/status@HermesBot"), Some(SlashCommand::Status));
        assert_eq!(parse_command("/model"), Some(SlashCommand::Model(None)));
        assert_eq!(
            parse_command("/model ag/gemini-3.8-flash"),
            Some(SlashCommand::Model(Some("ag/gemini-3.8-flash".to_string())))
        );
        assert_eq!(
            parse_command("/model   freet/claude-5-sonnet  "),
            Some(SlashCommand::Model(Some("freet/claude-5-sonnet".to_string())))
        );
        assert_eq!(parse_command("/clear"), Some(SlashCommand::Clear));
        assert_eq!(parse_command("/reset"), Some(SlashCommand::Clear));
        assert_eq!(parse_command("/new"), Some(SlashCommand::Clear));
        assert_eq!(parse_command("hello world"), None);
        assert_eq!(parse_command("/unknown"), None);
    }

    #[test]
    fn test_format_uptime() {
        assert_eq!(format_uptime(45), "45s");
        assert_eq!(format_uptime(125), "2m 5s");
        assert_eq!(format_uptime(3665), "1h 1m 5s");
        assert_eq!(format_uptime(90061), "1d 1h 1m 1s");
    }

    #[test]
    fn test_handle_command_start_and_help() {
        let ctx = CommandContext {
            gateway_name: "hermes-gateway",
            gateway_version: "0.1.0",
            active_profile: "default",
            active_model: "ag/gemini-3.8-flash",
            telegram_connected: true,
            memory_stats: Some("42.0 MB RSS".to_string()),
        };

        let start_res = handle_command(SlashCommand::Start, &ctx, |_| Ok(()));
        assert!(start_res.contains("Greetings!"));
        assert!(start_res.contains("I am your Hermes Gateway assistant. All systems are operational."));
        assert!(start_res.contains("ag/gemini-3.8-flash"));

        let help_res = handle_command(SlashCommand::Help, &ctx, |_| Ok(()));
        assert!(help_res.contains("Greetings!"));
        assert!(help_res.contains("I am your Hermes Gateway assistant. All systems are operational."));
        assert!(help_res.contains("/status"));
    }

    #[test]
    fn test_handle_command_status() {
        let ctx = CommandContext {
            gateway_name: "Hermes",
            gateway_version: "0.1.0",
            active_profile: "default",
            active_model: "ag/gemini-3.8-flash",
            telegram_connected: true,
            memory_stats: Some("15.5 MB RSS / 120.0 MB VIRT".to_string()),
        };

        let res = handle_command(SlashCommand::Status, &ctx, |_| Ok(()));
        assert!(res.contains("Hermes Gateway Status"));
        assert!(res.contains("Hermes v0.1.0"));
        assert!(res.contains("default"));
        assert!(res.contains("Connected"));
        assert!(res.contains("15.5 MB RSS"));
    }

    #[test]
    fn test_handle_command_model_query_and_switch() {
        let ctx = CommandContext {
            gateway_name: "hermes-gateway",
            gateway_version: "0.1.0",
            active_profile: "default",
            active_model: "gemini-2.5-flash",
            telegram_connected: true,
            memory_stats: None,
        };

        // Query model
        let query_res = handle_command(SlashCommand::Model(None), &ctx, |_| Ok(()));
        assert!(query_res.contains("gemini-2.5-flash"));
        assert!(query_res.contains("/model &lt;model_name&gt;"));

        // Switch model success
        let mut switched = String::new();
        let switch_res = handle_command(
            SlashCommand::Model(Some("ag/gemini-3.8-flash".to_string())),
            &ctx,
            |target| {
                switched = target.to_string();
                Ok(())
            },
        );
        assert_eq!(switched, "ag/gemini-3.8-flash");
        assert!(switch_res.contains("Model Switched Successfully"));
        assert!(switch_res.contains("ag/gemini-3.8-flash"));

        // Switch model empty failure
        let fail_res = handle_command(
            SlashCommand::Model(Some("   ".to_string())),
            &ctx,
            |_| Ok(()),
        );
        assert!(fail_res.contains("Model name cannot be empty"));
    }

    #[test]
    fn test_handle_command_clear() {
        let ctx = CommandContext {
            gateway_name: "hermes-gateway",
            gateway_version: "0.1.0",
            active_profile: "default",
            active_model: "gemini-2.5-flash",
            telegram_connected: true,
            memory_stats: None,
        };

        let clear_res = handle_command(SlashCommand::Clear, &ctx, |_| Ok(()));
        assert!(clear_res.contains("Session Reset"));
        assert!(clear_res.contains("Conversation turn memory has been cleared."));
    }

    #[test]
    fn test_build_provider_keyboard() {
        let providers = default_providers_list();
        let keyboard = build_provider_keyboard(&providers, "9router");

        assert_eq!(keyboard.inline_keyboard.len(), 4);
        assert_eq!(keyboard.inline_keyboard[0][0].text, "⭐ 9Router Gateway");
        assert_eq!(keyboard.inline_keyboard[0][0].callback_data.as_deref(), Some("mp:9router"));
        assert_eq!(keyboard.inline_keyboard[0][1].text, "TokenHarbor");
        assert_eq!(keyboard.inline_keyboard[0][1].callback_data.as_deref(), Some("mp:tokenharbor"));

        // Action row
        let last_row = &keyboard.inline_keyboard[3];
        assert_eq!(last_row[0].text, "🔄 Refresh");
        assert_eq!(last_row[0].callback_data.as_deref(), Some("mrf:prov"));
        assert_eq!(last_row[1].text, "✗ Cancel");
        assert_eq!(last_row[1].callback_data.as_deref(), Some("mx:cancel"));
    }

    #[test]
    fn test_build_model_keyboard_and_pagination() {
        let models: Vec<String> = (1..=20).map(|i| format!("model-{}", i)).collect();
        let (keyboard, page, total) = build_model_keyboard(&models, 0, "model-1", "9router");

        assert_eq!(page, 1);
        assert_eq!(total, 3);
        // 8 model buttons + 1 nav row + 1 action row = 10 rows
        assert_eq!(keyboard.inline_keyboard.len(), 10);
        assert_eq!(keyboard.inline_keyboard[0][0].text, "⭐ model-1");
        assert_eq!(keyboard.inline_keyboard[0][0].callback_data.as_deref(), Some("mm:model-1"));

        // Nav row
        let nav_row = &keyboard.inline_keyboard[8];
        assert_eq!(nav_row.len(), 2); // [1/3], Next ▶
        assert_eq!(nav_row[0].text, "[1/3]");
        assert_eq!(nav_row[1].text, "Next ▶");
        assert_eq!(nav_row[1].callback_data.as_deref(), Some("mg:9router:1"));

        // Cancel / Back row
        let tail_row = &keyboard.inline_keyboard[9];
        assert_eq!(tail_row[0].text, "⬅ Back");
        assert_eq!(tail_row[0].callback_data.as_deref(), Some("mb:back"));
        assert_eq!(tail_row[1].text, "🔄 Refresh");
        assert_eq!(tail_row[2].text, "✗ Cancel");
    }
}
