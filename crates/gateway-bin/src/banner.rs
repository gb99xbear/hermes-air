// Hermes Air - Visual Branding & Startup Banner
use std::env;

pub const LOGO_HERMES_AIR: [(&str, &str); 6] = [
    ("\x1b[1;38;2;255;215;0m", "██╗  ██╗███████╗██████╗ ███╗   ███╗███████╗███████╗    █████╗ ██╗██████╗     █████╗  ██████╗ ███████╗███╗   ██╗████████╗"),
    ("\x1b[1;38;2;255;215;0m", "██║  ██║██╔════╝██╔══██╗████╗ ████║██╔════╝██╔════╝   ██╔══██╗██║██╔══██╗   ██╔══██╗██╔════╝ ██╔════╝████╗  ██║╚══██╔══╝"),
    ("\x1b[38;2;255;191;0m",   "███████║█████╗  ██████╔╝██╔████╔██║█████╗  ███████╗   ███████║██║██████╔╝   ███████║██║  ███╗█████╗  ██╔██╗ ██║   ██║   "),
    ("\x1b[38;2;255;191;0m",   "██╔══██║██╔══╝  ██╔══██╗██║╚██╔╝██║██╔══╝  ╚════██║   ██╔══██║██║██╔══██╗   ██╔══██║██║   ██║██╔══╝  ██║╚██╗██║   ██║   "),
    ("\x1b[38;2;205;127;50m",  "██║  ██║███████╗██║  ██║██║ ╚═╝ ██║███████╗███████║   ██║  ██║██║██║  ██║   ██║  ██║╚██████╔╝███████╗██║ ╚████║   ██║   "),
    ("\x1b[38;2;205;127;50m",  "╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝╚══════╝╚══════╝   ╚═╝  ╚═╝╚═╝╚═╝  ╚═╝   ╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝   ╚═╝   "),
];

pub const CADUCEUS_HERO: [(&str, &str); 15] = [
    ("\x1b[38;2;205;127;50m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⡀⠀⣀⣀⠀⢀⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;205;127;50m", "⠀⠀⠀⠀⠀⠀⢀⣠⣴⣾⣿⣿⣇⠸⣿⣿⠇⣸⣿⣿⣷⣦⣄⡀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;255;191;0m", "⠀⢀⣠⣴⣶⠿⠋⣩⡿⣿⡿⠻⣿⡇⢠⡄⢸⣿⠟⢿⣿⢿⣍⠙⠿⣶⣦⣄⡀⠀"),
    ("\x1b[38;2;255;191;0m", "⠀⠀⠉⠉⠁⠶⠟⠋⠀⠉⠀⢀⣈⣁⡈⢁⣈⣁⡀⠀⠉⠀⠙⠻⠶⠈⠉⠉⠀⠀"),
    ("\x1b[38;2;255;215;0m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣴⣿⡿⠛⢁⡈⠛⢿⣿⣦⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;255;215;0m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠿⣿⣦⣤⣈⠁⢠⣴⣿⠿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;255;191;0m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠉⠻⢿⣿⣦⡉⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;255;191;0m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠘⢷⣦⣈⠛⠃⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;205;127;50m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢠⣴⠦⠈⠙⠿⣦⡄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;205;127;50m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠸⣿⣤⡈⠁⢤⣿⠇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;184;134;11m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠛⠷⠄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;184;134;11m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⠑⢶⣄⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;184;134;11m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⠁⢰⡆⠈⡿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;184;134;11m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠳⠈⣡⠞⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
    ("\x1b[38;2;184;134;11m", "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀"),
];

/// Helper to compute display width in terminal columns (handling emojis/full-width chars)
pub fn str_display_width(s: &str) -> usize {
    s.chars().fold(0, |acc, ch| {
        let cp = ch as u32;
        if (0x1F300..=0x1FAFF).contains(&cp) || (0x2600..=0x27BF).contains(&cp) && cp != 0x2624 {
            acc + 2
        } else {
            acc + 1
        }
    })
}

/// Print the official 3D Isometric ASCII Logo
pub fn print_ascii_logo() {
    let rst = "\x1b[0m";
    for (style, line) in LOGO_HERMES_AIR {
        println!("{}{}{}", style, line, rst);
    }
}

/// Print the full startup banner matching Python Hermes Agent specifications
pub fn print_startup_banner(model: &str, session_id: &str) {
    let c_gold1 = "\x1b[38;2;255;215;0m";    // #FFD700
    let c_gold2 = "\x1b[38;2;255;191;0m";    // #FFBF00
    let c_bronze = "\x1b[38;2;205;127;50m";  // #CD7F32
    let c_dim = "\x1b[38;2;184;134;11m";     // #B8860B
    let c_gray = "\x1b[38;2;139;134;130m";   // #8B8682
    let c_text = "\x1b[38;2;255;248;220m";   // #FFF8DC
    let c_cyan = "\x1b[1;36m";
    let c_bold = "\x1b[1m";
    let rst = "\x1b[0m";

    // 1. Title Logo
    print_ascii_logo();
    println!();

    // 2. 2-column Retro Dashboard Box
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());

    let mut left_lines: Vec<(String, String)> = Vec::new();
    for (col, art) in CADUCEUS_HERO {
        left_lines.push((art.to_string(), format!("{}{}{}", col, art, rst)));
    }
    left_lines.push(("".to_string(), "".to_string()));
    left_lines.push((
        format!("{} · 1.0M context · Nous Research", model),
        format!("{}{}{} {}· 1.0M context · Nous Research{}", c_gold2, model, rst, c_dim, rst),
    ));
    left_lines.push((
        format!("dir: {}", cwd),
        format!("{}dir: {}{}", c_dim, cwd, rst),
    ));
    left_lines.push((
        format!("Session: {}", session_id),
        format!("{}Session: {}{}", c_gray, session_id, rst),
    ));

    let mut right_lines: Vec<(String, String)> = vec![
        ("Available Tools".to_string(), format!("{}{}Available Tools{}", c_bold, c_gold2, rst)),
        ("  • terminal, read_file, write_file, patch, search_files".to_string(), format!("{}  • terminal, read_file, write_file, patch, search_files{}", c_text, rst)),
        ("  • web_search, web_extract, execute_code".to_string(), format!("{}  • web_search, web_extract, execute_code{}", c_text, rst)),
        ("  • browser_navigate, browser_snapshot, browser_click".to_string(), format!("{}  • browser_navigate, browser_snapshot, browser_click{}", c_text, rst)),
        ("  • browser_type, browser_exec".to_string(), format!("{}  • browser_type, browser_exec{}", c_text, rst)),
        ("  • skills_list, skill_view".to_string(), format!("{}  • skills_list, skill_view{}", c_text, rst)),
        ("".to_string(), "".to_string()),
        ("Available Skills".to_string(), format!("{}{}Available Skills{}", c_bold, c_gold2, rst)),
        ("  Security · Automation · DevOps · Research · QA-Testing".to_string(), format!("{}  Security · Automation · DevOps · Research · QA-Testing{}", c_text, rst)),
        ("  Programming · Productivity · Media · Finance".to_string(), format!("{}  Programming · Productivity · Media · Finance{}", c_text, rst)),
        ("".to_string(), "".to_string()),
        ("Profile: default".to_string(), format!("{}{}Profile: {}{}{}", c_bold, c_gold2, c_text, "default", rst)),
        ("15 tools · skills · /help for commands".to_string(), format!("{}15 tools · skills · /help for commands{}", c_dim, rst)),
    ];

    let max_lines = left_lines.len().max(right_lines.len());
    while left_lines.len() < max_lines {
        left_lines.push(("".to_string(), "".to_string()));
    }
    while right_lines.len() < max_lines {
        right_lines.push(("".to_string(), "".to_string()));
    }

    let inner_w: usize = 118;
    let left_w: usize = 46;
    let right_w: usize = inner_w - left_w - 4;

    let box_title = "Hermes Air Agent v0.1.0 (2026.10.30)";
    let dashes = inner_w.saturating_sub(2 + box_title.len() + 2);
    let top_border = format!(
        "{}╭─ {}{}{}{} {}╮{}",
        c_bronze, c_bold, c_gold1, box_title, c_bronze, "─".repeat(dashes), rst
    );
    let bot_border = format!("{}╰{}╯{}", c_bronze, "─".repeat(inner_w), rst);

    println!("{}", top_border);
    for i in 0..max_lines {
        let (l_plain, l_styled) = &left_lines[i];
        let (r_plain, r_styled) = &right_lines[i];

        let l_pad = " ".repeat(left_w.saturating_sub(str_display_width(l_plain)));
        let r_pad = " ".repeat(right_w.saturating_sub(str_display_width(r_plain)));

        println!(
            "{}│{}  {}{}{}{}  {}│{}",
            c_bronze, rst, l_styled, l_pad, r_styled, r_pad, c_bronze, rst
        );
    }
    println!("{}", bot_border);
    println!();

    // 3. Status Bar
    let status_bar_bg = "\x1b[48;2;26;26;46m";
    let status_bar_fg = "\x1b[38;2;192;192;192m";
    let sb_strong = format!("{}\x1b[1;38;2;255;215;0m", status_bar_bg);
    let sb_dim = format!("{}\x1b[38;2;138;122;74m", status_bar_bg);
    let sb_text = format!("{}{}", status_bar_bg, status_bar_fg);
    let sb_good = format!("{}\x1b[1;38;2;143;188;143m", status_bar_bg);

    let gauge = "[░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░]";
    println!(
        "{}  {}☤ {}{}  │  {}Context {}{}{} 0%{}  │  {}Runtime: Pure Async Rust (~20MB RAM){}  │  {}Uptime: 0s  {}",
        status_bar_bg, sb_strong, model, sb_dim, sb_text, sb_good, gauge, sb_text, sb_dim, sb_text, sb_dim, sb_text, rst
    );
    println!();

    // 4. Welcome Message & Quick Command Hints
    println!("{}Welcome to Hermes Air Agent! Type your message or /help for commands.{}", c_cyan, rst);
    println!("{}Quick command hints:{}", c_dim, rst);
    println!("{}  /model    - Switch or view active model{}", c_dim, rst);
    println!("{}  /context  - Show context window usage & token headroom{}", c_dim, rst);
    println!("{}  /clear    - Reset conversation history{}", c_dim, rst);
    println!("{}  /compress - Compact conversation context{}", c_dim, rst);
    println!("{}  exit      - Exit chat{}", c_dim, rst);
    println!();
}

/// Print the logo header when setup is launched
pub fn print_setup_header() {
    let c_cyan = "\x1b[1;36m";
    let c_dim = "\x1b[38;2;184;134;11m";
    let rst = "\x1b[0m";

    print_ascii_logo();
    println!();
    println!("{}🍃 Hermes Air v0.1.0 — The Featherlight Agent Gateway{}", c_cyan, rst);
    println!("{}Interactive Setup Wizard — Configuration & Diagnostics{}", c_dim, rst);
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_width_calculations() {
        assert_eq!(str_display_width("abc"), 3);
        assert_eq!(str_display_width("🍃"), 2);
        assert_eq!(str_display_width("•"), 1);
        assert_eq!(str_display_width("☤"), 1);
    }

    #[test]
    fn test_logo_dimensions() {
        assert_eq!(LOGO_HERMES_AIR.len(), 6);
        for (_, line) in LOGO_HERMES_AIR {
            assert_eq!(line.chars().count(), 120);
        }
    }

    #[test]
    fn test_caduceus_dimensions() {
        assert_eq!(CADUCEUS_HERO.len(), 15);
        for (_, line) in CADUCEUS_HERO {
            assert_eq!(line.chars().count(), 30);
        }
    }
}
