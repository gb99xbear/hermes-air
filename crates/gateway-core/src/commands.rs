use std::time::Instant;

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
/// Handles Telegram bot mentions (e.g. /status@my_bot) and leading/trailing whitespace.
pub fn parse_command(text: &str) -> Option<SlashCommand> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let raw_cmd = parts.next()?;
    let args = parts.next().map(str::trim).filter(|s| !s.is_empty());

    // Strip bot mention if present, e.g. /status@AlfredBot -> /status
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

/// Format the Butler Greeting for /start and /help
pub fn handle_start_or_help(ctx: &CommandContext) -> String {
    format!(
        "🎩 <b>Greetings, Bear.</b>\n\n        I am your loyal butler and personal gateway interface. All systems are operational.\n\n        <b>Active Model:</b> <code>{}</code>\n        <b>Profile:</b> <code>{}</code>\n\n        <b>Available Slash Commands:</b>\n        • <code>/start</code> / <code>/help</code> — Show this butler greeting & command overview\n        • <code>/status</code> — Display full system diagnostics, uptime & platform status\n        • <code>/model</code> — View current active model & provider info\n        • <code>/model &lt;name&gt;</code> — Dynamically switch active model (e.g. <code>/model ag/gemini-3.8-flash</code>)\n        • <code>/clear</code> / <code>/reset</code> — Reset conversation turn memory & session\n\n        <i>Execute any request directly — I stand ready to serve.</i>",
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
"🎩 *ALFRED GATEWAY STATUS*

| Metric | Details |
| :--- | :--- |
| **Gateway** | `{}` |
| **Uptime** | `{}` |
| **Profile** | `{}` |
| **Model** | `{}` |
| **Telegram** | `{}` |
| **RAM** | `{}` |

*Estate Directive:*
`profile`: `{}`
`model`: `{}`
`ram`: `{}`

_Status: Operational | 500% Obedience Standing Active._",
        format!("{} v{}", ctx.gateway_name, ctx.gateway_version),
        uptime_str,
        ctx.active_profile,
        ctx.active_model,
        tg_status,
        memory_info,
        ctx.active_profile,
        ctx.active_model,
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
                "🤖 <b>Active Model Configuration</b>\n\n                • <b>Current Model:</b> <code>{}</code>\n\n                To switch models dynamically for this gateway session, run:\n                <code>/model &lt;model_name&gt;</code>\n\n                <b>Examples:</b>\n                • <code>/model ag/gemini-3.8-flash</code>\n                • <code>/model freet/claude-3-7-sonnet</code>\n                • <code>/model gpt-4o</code>",
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
                        "✅ <b>Model Switched Successfully</b>\n\n                        Active model is now set to: <code>{}</code>\n                        Subsequent conversation turns will use this model.",
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

/// Handle /clear or /reset command
pub fn handle_clear() -> String {
    "🧹 <b>Session Reset</b>\n\n    Conversation turn memory has been cleared. Beginning a fresh session."
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
        assert_eq!(parse_command("/status@AlfredBot"), Some(SlashCommand::Status));
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
            active_profile: "alfred",
            active_model: "ag/gemini-3.8-flash",
            telegram_connected: true,
            memory_stats: Some("42.0 MB RSS".to_string()),
        };

        let start_res = handle_command(SlashCommand::Start, &ctx, |_| Ok(()));
        assert!(start_res.contains("Greetings, Bear."));
        assert!(start_res.contains("ag/gemini-3.8-flash"));

        let help_res = handle_command(SlashCommand::Help, &ctx, |_| Ok(()));
        assert!(help_res.contains("Greetings, Bear."));
        assert!(help_res.contains("/status"));
    }

    #[test]
    fn test_handle_command_status() {
        let ctx = CommandContext {
            gateway_name: "hermes-gateway",
            gateway_version: "0.1.0",
            active_profile: "alfred",
            active_model: "ag/gemini-3.8-flash",
            telegram_connected: true,
            memory_stats: Some("15.5 MB RSS / 120.0 MB VIRT".to_string()),
        };

        let res = handle_command(SlashCommand::Status, &ctx, |_| Ok(()));
        assert!(res.contains("hermes-gateway v0.1.0"));
        assert!(res.contains("alfred"));
        assert!(res.contains("Connected"));
        assert!(res.contains("15.5 MB RSS"));
    }

    #[test]
    fn test_handle_command_model_query_and_switch() {
        let ctx = CommandContext {
            gateway_name: "hermes-gateway",
            gateway_version: "0.1.0",
            active_profile: "alfred",
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
            active_profile: "alfred",
            active_model: "gemini-2.5-flash",
            telegram_connected: true,
            memory_stats: None,
        };

        let clear_res = handle_command(SlashCommand::Clear, &ctx, |_| Ok(()));
        assert!(clear_res.contains("Session Reset"));
        assert!(clear_res.contains("Conversation turn memory has been cleared."));
    }
}
