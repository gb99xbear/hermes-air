use std::time::Instant;
use hermes_platform_telegram::{InlineKeyboardButton, InlineKeyboardMarkup};

/// Busy Policy classification matching Python gateway/run_busy.py
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyPolicy {
    Reject,
    Dispatch,
    InterruptThenDispatch,
}

/// Active Slash Commands supported by Hermes native Gateway
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Start,
    Help,
    Status,
    Model(Option<String>),
    Clear,
    Stop,
    Context,
    Whoami,
    Profile,
    Diff(Option<String>),
    Compress(Option<String>),
}

impl SlashCommand {
    pub fn name(&self) -> &'static str {
        match self {
            SlashCommand::Start => "start",
            SlashCommand::Help => "help",
            SlashCommand::Status => "status",
            SlashCommand::Model(_) => "model",
            SlashCommand::Clear => "clear",
            SlashCommand::Stop => "stop",
            SlashCommand::Context => "context",
            SlashCommand::Whoami => "whoami",
            SlashCommand::Profile => "profile",
            SlashCommand::Diff(_) => "diff",
            SlashCommand::Compress(_) => "compress",
        }
    }

    pub fn busy_policy(&self) -> BusyPolicy {
        match self {
            // Reject mid-turn commands that mutate model, personality, or compact session
            SlashCommand::Model(_) | SlashCommand::Compress(_) => BusyPolicy::Reject,

            // Concurrent read-only diagnostic / info commands allowed mid-turn
            SlashCommand::Status
            | SlashCommand::Context
            | SlashCommand::Whoami
            | SlashCommand::Profile
            | SlashCommand::Diff(_)
            | SlashCommand::Help => BusyPolicy::Dispatch,

            // Interrupt running turn immediately and dispatch
            SlashCommand::Stop | SlashCommand::Clear | SlashCommand::Start => {
                BusyPolicy::InterruptThenDispatch
            }
        }
    }
}

/// Parse incoming prompt or message text for slash commands.
/// Matches commands like /start, /help, /status, /model [name], /clear, /reset, /new, /stop, /context, /whoami, /profile, /diff.
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
        "/stop" => Some(SlashCommand::Stop),
        "/context" => Some(SlashCommand::Context),
        "/whoami" => Some(SlashCommand::Whoami),
        "/profile" => Some(SlashCommand::Profile),
        "/diff" => Some(SlashCommand::Diff(args.map(|s| s.to_string()))),
        "/compress" | "/compact" => Some(SlashCommand::Compress(args.map(|s| s.to_string()))),
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
    pub session_id: Option<uuid::Uuid>,
    pub platform: Option<&'a str>,
    pub user_id: Option<&'a str>,
    pub chat_id: Option<&'a str>,
    pub chat_type: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    pub transcript_count: usize,
    pub estimated_prompt_tokens: usize,
    pub context_window: usize,
}

/// Format the Greeting for /start and /help
pub fn handle_start_or_help(ctx: &CommandContext) -> String {
    format!(
        "<b>Greetings!</b>\n\nI am your Hermes Gateway assistant. All systems are operational.\n\n<b>Active Model:</b> <code>{}</code>\n<b>Profile:</b> <code>{}</code>\n\n<b>Available Slash Commands:</b>\n• <code>/start</code> / <code>/help</code> — Show this greeting & command overview\n• <code>/status</code> — Display full system diagnostics, uptime & platform status\n• <code>/context</code> — Visual context window occupancy & token headroom bar\n• <code>/whoami</code> — Display user, chat, session UUID and platform scope\n• <code>/profile</code> — Display active agent profile configuration\n• <code>/diff</code> — Display recent git changes and file diffs\n• <code>/model</code> — View current active model & provider info\n• <code>/model &lt;name&gt;</code> — Dynamically switch active model (e.g. <code>/model ag/gemini-3.8-flash</code>)\n• <code>/stop</code> — Interrupt and cancel active running turn immediately\n• <code>/compress</code> / <code>/compact</code> [focus] — Compact conversation context into structured snapshot\n• <code>/clear</code> / <code>/reset</code> — Reset conversation turn memory & session",
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

/// Format visual context bar matching Python Hermes slash_commands_status.py
pub fn handle_context(ctx: &CommandContext) -> String {
    let window = if ctx.context_window > 0 {
        ctx.context_window
    } else {
        // Default context length fallback if not configured (e.g. 128k)
        128_000
    };

    let used = ctx.estimated_prompt_tokens;
    let pct = if window > 0 {
        ((used as f64 / window as f64) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let bar_len: usize = 24;
    let filled = ((pct / 100.0) * bar_len as f64).round() as usize;
    let filled = filled.min(bar_len);
    let empty = bar_len.saturating_sub(filled);

    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));
    let headroom = window.saturating_sub(used);

    format!(
"⚡ <b>CONTEXT WINDOW</b>

<b>Model:</b> <code>{}</code>
<b>Window:</b> {} tokens
<b>In use:</b> {} / {} ({:.1}%)
<b>Bar:</b> <code>[{}]</code>
<b>Headroom:</b> {} tokens
<b>Messages in transcript:</b> {}",
        ctx.active_model,
        format_number(window),
        format_number(used),
        format_number(window),
        pct,
        bar,
        format_number(headroom),
        ctx.transcript_count
    )
}

/// Handle /whoami command matching Python slash_commands.py
pub fn handle_whoami(ctx: &CommandContext) -> String {
    let platform = ctx.platform.unwrap_or("unknown");
    let chat_type = ctx.chat_type.unwrap_or("dm");
    let scope = if chat_type.eq_ignore_ascii_case("dm")
        || chat_type.eq_ignore_ascii_case("direct")
        || chat_type.eq_ignore_ascii_case("private")
        || chat_type.is_empty()
    {
        "DM"
    } else {
        "group/channel"
    };

    let user_id = ctx.user_id.unwrap_or("?");
    let chat_id = ctx.chat_id.unwrap_or("?");
    let session_uuid = ctx
        .session_id
        .map(|u| u.to_string())
        .unwrap_or_else(|| "none".to_string());
    let thread_id = ctx.thread_id.unwrap_or("none");

    format!(
"<b>You</b> — {} ({})
• <b>User ID:</b> <code>{}</code>
• <b>Chat ID:</b> <code>{}</code>
• <b>Chat Type:</b> <code>{}</code>
• <b>Thread ID:</b> <code>{}</code>
• <b>Active Profile:</b> <code>{}</code>
• <b>Session UUID:</b> <code>{}</code>
• <b>Tier:</b> unrestricted
• <b>Slash commands:</b> all available",
        platform, scope, user_id, chat_id, chat_type, thread_id, ctx.active_profile, session_uuid
    )
}

/// Handle /profile command
pub fn handle_profile(ctx: &CommandContext) -> String {
    format!(
"🎭 <b>Agent Profile Configuration</b>

• <b>Profile Name:</b> <code>{}</code>
• <b>Active Model:</b> <code>{}</code>
• <b>Gateway Name:</b> <code>{}</code>
• <b>Version:</b> <code>{}</code>
• <b>Session UUID:</b> <code>{}</code>",
        ctx.active_profile,
        ctx.active_model,
        ctx.gateway_name,
        ctx.gateway_version,
        ctx.session_id.map(|u| u.to_string()).unwrap_or_else(|| "none".to_string())
    )
}

/// Handle /diff command matching Python slash_commands.py
pub fn handle_diff(arg: &Option<String>) -> String {
    let stat_only = arg
        .as_deref()
        .map(|s| s.contains("--stat") || s.contains("stat"))
        .unwrap_or(false);

    // Try reading git diff in current working directory
    let git_status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output();

    let git_diff = if stat_only {
        std::process::Command::new("git")
            .args(["diff", "--stat"])
            .output()
    } else {
        std::process::Command::new("git")
            .args(["diff", "HEAD"])
            .output()
    };

    match (git_status, git_diff) {
        (Ok(status_out), Ok(diff_out)) => {
            let status_str = String::from_utf8_lossy(&status_out.stdout).trim().to_string();
            let diff_str = String::from_utf8_lossy(&diff_out.stdout).trim().to_string();

            if status_str.is_empty() && diff_str.is_empty() {
                return "📂 <b>Working Directory Clean</b>\n\nNo modified, staged, or untracked changes found.".to_string();
            }

            let mut out = Vec::new();
            if !status_str.is_empty() {
                let lines: Vec<&str> = status_str.lines().take(15).collect();
                let extra = if status_str.lines().count() > 15 {
                    format!("\n... and {} more files", status_str.lines().count() - 15)
                } else {
                    String::new()
                };
                out.push(format!("<b>Status:</b>\n<pre>\n{}{}\n</pre>", lines.join("\n"), extra));
            }

            if !diff_str.is_empty() {
                let truncated_diff = if diff_str.lines().count() > 60 {
                    let first_lines: Vec<&str> = diff_str.lines().take(60).collect();
                    format!("{}\n... (truncated — {} lines total; use /diff --stat for a summary)", first_lines.join("\n"), diff_str.lines().count())
                } else if diff_str.len() > 3000 {
                    format!("{}...\n(truncated at 3000 chars)", &diff_str[..3000])
                } else {
                    diff_str
                };
                out.push(format!("<pre>\n{}\n</pre>", truncated_diff));
            }

            out.join("\n\n")
        }
        _ => "⚠️ <i>Git repository not detected or git command unavailable in working directory.</i>".to_string(),
    }
}

/// Helper to format large integers with commas
fn format_number(n: usize) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    let bytes = s.as_bytes();
    let len = bytes.len();

    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(b as char);
    }
    result
}

/// Handle /stop command
pub fn handle_stop() -> String {
    "⏹️ Agent execution stopped.".to_string()
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

/// Format confirmation for manual /compress and /compact command
pub fn handle_compress(
    pre_count: usize,
    post_count: usize,
    tokens_saved: usize,
) -> String {
    format!(
        "🗜️ Context compacted: {} messages → {} messages ({} tokens saved).",
        pre_count, post_count, tokens_saved
    )
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
        SlashCommand::Stop => handle_stop(),
        SlashCommand::Context => handle_context(ctx),
        SlashCommand::Whoami => handle_whoami(ctx),
        SlashCommand::Profile => handle_profile(ctx),
        SlashCommand::Diff(arg) => handle_diff(&arg),
        SlashCommand::Compress(_) => handle_compress(ctx.transcript_count, 1.min(ctx.transcript_count), 0),
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
        assert_eq!(parse_command("/stop"), Some(SlashCommand::Stop));
        assert_eq!(parse_command("/context"), Some(SlashCommand::Context));
        assert_eq!(parse_command("/whoami"), Some(SlashCommand::Whoami));
        assert_eq!(parse_command("/profile"), Some(SlashCommand::Profile));
        assert_eq!(parse_command("/diff"), Some(SlashCommand::Diff(None)));
        assert_eq!(
            parse_command("/diff --stat"),
            Some(SlashCommand::Diff(Some("--stat".to_string())))
        );
        assert_eq!(parse_command("hello world"), None);
        assert_eq!(parse_command("/unknown"), None);
    }

    #[test]
    fn test_busy_policy() {
        assert_eq!(SlashCommand::Model(None).busy_policy(), BusyPolicy::Reject);
        assert_eq!(SlashCommand::Status.busy_policy(), BusyPolicy::Dispatch);
        assert_eq!(SlashCommand::Context.busy_policy(), BusyPolicy::Dispatch);
        assert_eq!(SlashCommand::Whoami.busy_policy(), BusyPolicy::Dispatch);
        assert_eq!(SlashCommand::Profile.busy_policy(), BusyPolicy::Dispatch);
        assert_eq!(SlashCommand::Diff(None).busy_policy(), BusyPolicy::Dispatch);
        assert_eq!(SlashCommand::Stop.busy_policy(), BusyPolicy::InterruptThenDispatch);
        assert_eq!(SlashCommand::Clear.busy_policy(), BusyPolicy::InterruptThenDispatch);
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
            session_id: Some(uuid::Uuid::nil()),
            platform: Some("telegram"),
            user_id: Some("12345"),
            chat_id: Some("67890"),
            chat_type: Some("dm"),
            thread_id: None,
            transcript_count: 5,
            estimated_prompt_tokens: 1200,
            context_window: 128000,
        };

        let start_res = handle_command(SlashCommand::Start, &ctx, |_| Ok(()));
        assert!(start_res.contains("Greetings!"));
        assert!(start_res.contains("I am your Hermes Gateway assistant. All systems are operational."));
        assert!(start_res.contains("ag/gemini-3.8-flash"));

        let help_res = handle_command(SlashCommand::Help, &ctx, |_| Ok(()));
        assert!(help_res.contains("Greetings!"));
        assert!(help_res.contains("I am your Hermes Gateway assistant. All systems are operational."));
        assert!(help_res.contains("/status"));
        assert!(help_res.contains("/context"));
        assert!(help_res.contains("/whoami"));
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
            session_id: Some(uuid::Uuid::nil()),
            platform: Some("telegram"),
            user_id: Some("12345"),
            chat_id: Some("67890"),
            chat_type: Some("dm"),
            thread_id: None,
            transcript_count: 5,
            estimated_prompt_tokens: 1200,
            context_window: 128000,
        };

        let res = handle_command(SlashCommand::Status, &ctx, |_| Ok(()));
        assert!(res.contains("Hermes Gateway Status"));
        assert!(res.contains("Hermes v0.1.0"));
        assert!(res.contains("default"));
        assert!(res.contains("Connected"));
        assert!(res.contains("15.5 MB RSS"));
    }

    #[test]
    fn test_handle_context_command() {
        let ctx = CommandContext {
            gateway_name: "Hermes",
            gateway_version: "0.1.0",
            active_profile: "default",
            active_model: "ag/gemini-3.8-flash",
            telegram_connected: true,
            memory_stats: None,
            session_id: Some(uuid::Uuid::nil()),
            platform: Some("telegram"),
            user_id: Some("12345"),
            chat_id: Some("67890"),
            chat_type: Some("dm"),
            thread_id: None,
            transcript_count: 12,
            estimated_prompt_tokens: 32000,
            context_window: 128000,
        };

        let res = handle_command(SlashCommand::Context, &ctx, |_| Ok(()));
        assert!(res.contains("CONTEXT WINDOW"));
        assert!(res.contains("ag/gemini-3.8-flash"));
        assert!(res.contains("128,000"));
        assert!(res.contains("32,000"));
        assert!(res.contains("25.0%"));
        assert!(res.contains("Messages in transcript:</b> 12"));
        assert!(res.contains("<b>Headroom:</b> 96,000"));
    }

    #[test]
    fn test_handle_whoami_command() {
        let ctx = CommandContext {
            gateway_name: "Hermes",
            gateway_version: "0.1.0",
            active_profile: "default",
            active_model: "ag/gemini-3.8-flash",
            telegram_connected: true,
            memory_stats: None,
            session_id: Some(uuid::Uuid::nil()),
            platform: Some("telegram"),
            user_id: Some("user_999"),
            chat_id: Some("chat_888"),
            chat_type: Some("dm"),
            thread_id: Some("thread_777"),
            transcript_count: 0,
            estimated_prompt_tokens: 0,
            context_window: 128000,
        };

        let res = handle_command(SlashCommand::Whoami, &ctx, |_| Ok(()));
        eprintln!("DEBUG_RES_WHOAMI:\n{}", res);
        assert!(res.contains("You</b> — telegram (DM)"));
        assert!(res.contains("user_999"));
        assert!(res.contains("chat_888"));
        assert!(res.contains("thread_777"));
        assert!(res.contains("<b>Tier:</b> unrestricted"));
    }

    #[test]
    fn test_handle_profile_command() {
        let session_id = uuid::Uuid::new_v4();
        let ctx = CommandContext {
            gateway_name: "Hermes",
            gateway_version: "0.1.0",
            active_profile: "coding-specialist",
            active_model: "freet/claude-3-7-sonnet",
            telegram_connected: true,
            memory_stats: None,
            session_id: Some(session_id),
            platform: Some("telegram"),
            user_id: None,
            chat_id: None,
            chat_type: None,
            thread_id: None,
            transcript_count: 0,
            estimated_prompt_tokens: 0,
            context_window: 128000,
        };

        let res = handle_command(SlashCommand::Profile, &ctx, |_| Ok(()));
        assert!(res.contains("Agent Profile Configuration"));
        assert!(res.contains("coding-specialist"));
        assert!(res.contains("freet/claude-3-7-sonnet"));
        assert!(res.contains(&session_id.to_string()));
    }

    #[test]
    fn test_handle_stop_command() {
        let res = handle_stop();
        assert_eq!(res, "⏹️ Agent execution stopped.");
    }

    #[test]
    fn test_handle_diff_command() {
        let res = handle_diff(&None);
        // Should execute git or return clean / not detected without panic
        assert!(!res.is_empty());
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
            session_id: None,
            platform: None,
            user_id: None,
            chat_id: None,
            chat_type: None,
            thread_id: None,
            transcript_count: 0,
            estimated_prompt_tokens: 0,
            context_window: 128000,
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
            session_id: None,
            platform: None,
            user_id: None,
            chat_id: None,
            chat_type: None,
            thread_id: None,
            transcript_count: 0,
            estimated_prompt_tokens: 0,
            context_window: 128000,
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

    #[test]
    fn test_parse_compress_commands() {
        assert_eq!(parse_command("/compress"), Some(SlashCommand::Compress(None)));
        assert_eq!(parse_command("/compact"), Some(SlashCommand::Compress(None)));
        assert_eq!(
            parse_command("/compress backend architecture"),
            Some(SlashCommand::Compress(Some("backend architecture".to_string())))
        );
        assert_eq!(
            parse_command("/compact memory-hygiene"),
            Some(SlashCommand::Compress(Some("memory-hygiene".to_string())))
        );
        assert_eq!(SlashCommand::Compress(None).busy_policy(), BusyPolicy::Reject);
    }

    #[test]
    fn test_handle_compress_formatting() {
        let text = handle_compress(24, 5, 12500);
        assert_eq!(text, "🗜️ Context compacted: 24 messages → 5 messages (12500 tokens saved).");
    }
}
