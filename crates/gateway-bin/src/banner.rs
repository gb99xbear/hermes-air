// crates/gateway-bin/src/banner.rs
// Hermes Air - 1-to-1 Authentic Terminal Dashboard Banner

use std::path::Path;

pub fn print_startup_banner(model_name: &str, cwd: &Path, session_id: &str) {
    let gold_bold = "\x1b[1;38;2;255;215;0m";
    let amber_box = "\x1b[38;2;205;127;50m";
    let yellow_text = "\x1b[38;2;255;235;130m";
    let gray_dim = "\x1b[38;2;140;145;155m";
    let cyan_bright = "\x1b[1;38;2;100;220;255m";
    let reset = "\x1b[0m";

    // 1. Top Logo: HERMES-AIR AGENT in clean block ASCII matching Python Hermes
    println!("{}", gold_bold);
    println!(" _   _ _____ ____  __  __ _____ ____       _    ___ ____      _    ____ _____ _   _ _____ ");
    println!("| | | | ____|  _ \\|  \\/  | ____/ ___|     / \\  |_ _|  _ \\    / \\  / ___| ____| \\ | |_   _|");
    println!("| |_| |  _| | |_) | |\\/| |  _| \\___ \\    / _ \\  | || |_) |  / _ \\| |  _|  _| |  \\| | | |  ");
    println!("|  _  | |___|  _ <| |  | | |___ ___) |  / ___ \\ | ||  _ <  / ___ \\ |_| | |___| |\\  | | |  ");
    println!("|_| |_|_____|_| \\_\\_|  |_|_____|____/  /_/   \\_\\___|_| \\_\\/_/   \\_\\____|_____|_| \\_| |_|  ");
    println!("{}", reset);

    // 2. Exact Caduceus ASCII Art (34 lines) matching Python Hermes screenshot
    let caduceus_lines: [&str; 34] = [
        "         .:::.                   .:::.      ",
        "      .:*%@@@%*:.             .:*%@@@%*:.   ",
        "    .:*#@@@@@@@@#*:         :*#@@@@@@@@#*:. ",
        "   .:*%@@@@@@@@@@@%*:.....:*%@@@@@@@@@@@%*:.",
        "     .:*#@@@@@@@@@@@#*:::*#@@@@@@@@@@@#*:.  ",
        "        .:*%@@@@@@@@@@@@@@@@@@@@@@%*:.      ",
        "           .:*#@@@@@@@@@@@@@@@@#*:.         ",
        "              .:*%@@@@@@@@@@%*:.            ",
        "                   .:*%%*:.                 ",
        "                     :@@:                   ",
        "                  .:*%@@%*:.                ",
        "                .:*#@@@@@@#*:.              ",
        "               :*%@@%*::*%@@%*:             ",
        "              :%@@%:      :%@@%:            ",
        "              :%@@%:      :%@@%:            ",
        "               :*%@@%*::*%@@%*:             ",
        "                .:*#@@@@@@#*:.              ",
        "                  .:*%@@%*:.                ",
        "                     :@@:                   ",
        "                  .:*%@@%*:.                ",
        "                .:*#@@@@@@#*:.              ",
        "               :*%@@%*::*%@@%*:             ",
        "              :%@@%:      :%@@%:            ",
        "              :%@@%:      :%@@%:            ",
        "               :*%@@%*::*%@@%*:             ",
        "                .:*#@@@@@@#*:.              ",
        "                  .:*%@@%*:.                ",
        "                     :@@:                   ",
        "                     :@@:                   ",
        "                     :@@:                   ",
        "                     :@@:                   ",
        "                    .:%%:.                  ",
        "                     :@@:                   ",
        "                      ::                    ",
    ];

    let right_column: [&str; 34] = [
        "Available Tools",
        "browser: browser_click, browser_exec, browser_navigate...",
        "code_execution: execute_code",
        "file: patch, read_file, search_files, write_file",
        "skills: skill_view, skills_list",
        "terminal: terminal",
        "web: web_extract, web_search",
        "",
        "Available Skills",
        "autonomous-ai-agents: hermes-air",
        "creative: ascii-art, system-atlas",
        "devops: docker-management, setup-wizard",
        "gaming: game-hacking",
        "media: vision-analyze, tts",
        "mlops: model-evaluation, vllm",
        "productivity: context-compaction, todo",
        "research: web-research, osint",
        "security: offensive-security, exploit-gen, rat",
        "software-development: test-driven-development, git",
        "",
        "Profile: default",
        "15 tools • 45 skills • /help for commands",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
    ];

    // Top border - exactly 111 chars
    println!("{}╭─────────────────────────────────────────────────────────────────── Hermes Air Agent v0.1.0 (2026.10.30) ────╮{}", amber_box, reset);

    for i in 0..34 {
        let r_text = right_column[i];
        let r_is_header = r_text.starts_with("Available") || r_text.starts_with("Profile:");
        let r_pad = 60usize.saturating_sub(r_text.chars().count());

        let l_cell = if i == 31 {
            let s = format!("{} • Nous Research", model_name);
            let s_trimmed = if s.chars().count() > 44 { &s[..44] } else { &s };
            let pad_total = 44usize.saturating_sub(s_trimmed.chars().count());
            let pad_left = pad_total / 2;
            let pad_right = pad_total - pad_left;
            format!("{}{}{}", " ".repeat(pad_left), s_trimmed, " ".repeat(pad_right))
        } else if i == 32 {
            let cwd_str = cwd.to_string_lossy();
            let s_trimmed = if cwd_str.chars().count() > 44 { &cwd_str[..44] } else { &cwd_str };
            let pad_total = 44usize.saturating_sub(s_trimmed.chars().count());
            let pad_left = pad_total / 2;
            let pad_right = pad_total - pad_left;
            format!("{}{}{}", " ".repeat(pad_left), s_trimmed, " ".repeat(pad_right))
        } else if i == 33 {
            let s = format!("Session: {}", session_id);
            let s_trimmed = if s.chars().count() > 44 { &s[..44] } else { &s };
            let pad_total = 44usize.saturating_sub(s_trimmed.chars().count());
            let pad_left = pad_total / 2;
            let pad_right = pad_total - pad_left;
            format!("{}{}{}", " ".repeat(pad_left), s_trimmed, " ".repeat(pad_right))
        } else {
            let s = caduceus_lines[i];
            let pad = 44usize.saturating_sub(s.chars().count());
            format!("{}{}", s, " ".repeat(pad))
        };

        let l_color = if i >= 31 { gray_dim } else { gold_bold };
        let r_color = if r_is_header { gold_bold } else if r_text.is_empty() { "" } else { yellow_text };

        println!(
            "{}│{} {}{}{} {}│{} {}{}{}{} {}│{}",
            amber_box, reset,
            l_color, l_cell, reset,
            amber_box, reset,
            r_color, r_text, reset, " ".repeat(r_pad),
            amber_box, reset
        );
    }

    // Bottom border - exactly 111 chars
    println!("{}╰─────────────────────────────────────────────────────────────────────────────────────────────────────────────╯{}", amber_box, reset);

    // 3. Status Bar & Welcome Notice
    println!(
        "{}☤ {}{} │ {}ctx --{} │ [░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░] -- │ {}33s{}",
        cyan_bright, model_name, reset, gray_dim, reset, gray_dim, reset
    );
    println!();
    println!("Welcome to Hermes Air Agent! Type your message or /help for commands.");
    println!();
}

pub fn print_setup_header() {
    let gold_bold = "\x1b[1;38;2;255;215;0m";
    let cyan = "\x1b[1;38;2;100;220;255m";
    let reset = "\x1b[0m";
    println!("{}", gold_bold);
    println!(" _   _ _____ ____  __  __ _____ ____       _    ___ ____      _    ____ _____ _   _ _____ ");
    println!("| | | | ____|  _ \\|  \\/  | ____/ ___|     / \\  |_ _|  _ \\    / \\  / ___| ____| \\ | |_   _|");
    println!("| |_| |  _| | |_) | |\\/| |  _| \\___ \\    / _ \\  | || |_) |  / _ \\| |  _|  _| |  \\| | | |  ");
    println!("|  _  | |___|  _ <| |  | | |___ ___) |  / ___ \\ | ||  _ <  / ___ \\ |_| | |___| |\\  | | |  ");
    println!("|_| |_|_____|_| \\_\\_|  |_|_____|____/  /_/   \\_\\___|_| \\_\\/_/   \\_\\____|_____|_| \\_| |_|  ");
    println!("{}", reset);
    println!("{}{}===================================================={}", cyan, "\x1b[1m", reset);
    println!("{}🌿 Hermes Air 🍃 - Interactive Setup Wizard{}", gold_bold, reset);
    println!("{}{}===================================================={}", cyan, "\x1b[1m", reset);
    println!();
}
