# Hermes Air 🍃

> **The Featherlight, Blazing-Fast Rust Gateway Core for Autonomous AI Agents**

Hermes Air is a high-performance, native Rust re-implementation of the **Hermes Agent Gateway Core**. Designed with an architecture philosophy inspired by the Apple "Air" ethos—ultra-lightweight, near-zero memory footprint (~27.4MB RSS measured vs ~150-500MB+ in Python), blazing fast asynchronous concurrency, and 1-to-1 parity on the core gateway, tool execution, and messaging pipelines.

---

## 🎯 Architecture Scope: Gateway Core & Autonomous Tools

To be transparent with the open-source community: **Hermes Air focuses specifically on the Gateway, Autonomous Multi-Turn Execution Loop, and Core Tooling Suite.** 

The original [Nous Research Hermes Agent](https://github.com/NousResearch/hermes-agent) repository contains over 10,000 files spanning specialized enterprise integrations (e.g., Feishu, DingTalk, WeCom, ACP IDE server, Electron desktop app). Hermes Air deliberately ports the **mission-critical gateway runtime** to pure Rust, achieving 1-to-1 behavioral parity on:
- Terminal interactive REPL & One-shot CLI query (`hermes-air` / `hermes-air -q`).
- Interactive Setup Wizard (`hermes-air setup`).
- Production Telegram Bot API with native media, forum topics, and progressive streaming.
- Production Discord Gateway & REST client with mention filtering and thread sessions.
- Comprehensive 17-tool execution runtime (including 9-strategy fuzzy matching patch engine and headless browser automation).
- 85% Auto-compaction & context hygiene engine.

---

## ⚡ Why Hermes Air?

- **Up to 10x Less Memory Usage**: Runs at ~27.4MB RSS measured on Linux ARM64/x86_64 active idle (~18x lighter than Python)—ideal for budget VPS instances (512MB RAM), Raspberry Pi, and resource-constrained edge servers.
- **Pure Async Rust Performance**: Built on Tokio, hyper, and reqwest with zero Python runtime overhead, zero GIL contention, and deterministic memory management.
- **Interactive CLI & Gateway Dual-Mode**:
  - Run `hermes-air` or `hermes-air chat` for an instant interactive REPL terminal chat session with persistent history and ANSI streaming.
  - Run `hermes-air -q "<query>"` for instant one-shot CLI execution.
  - Run `hermes-air gateway` to launch the multi-platform background daemon.
- **1-to-1 Functional Parity on Gateway Core**:
  - **Telegram Messaging Platform**: Native Bot API long polling with stall watchdog, 409 conflict backoff ladder, native voice bubbles (`sendVoice`), high-fidelity audio (`sendAudio`), video attachments (`sendVideo`), forum topic sessions (`message_thread_id`), group mention filters, and interactive 2-tier inline keyboard model picker.
  - **Discord Platform Adapter**: Native REST integration, 2000-character boundary chunking, guild channel mention filter, and thread session isolation.
  - **Autonomous Agentic Loop**: Multi-turn tool execution loop (up to 30 iterations) with conversation memory persistence and zero fake echo fallback.
  - **Advanced Coding & File Suite**: Complete 9-strategy fuzzy matching patch engine (`SequenceMatcher` algorithm), line-indexed file reads, atomic file writes, and ripgrep-powered regex/glob file search.
  - **Browser Automation Suite**: Chrome DevTools Protocol (CDP) WebSocket engine supporting `browser_navigate`, `browser_snapshot`, `browser_click`, `browser_type`, `browser_take_screenshot`, and `browser_exec` with strict SSRF private network filtering.
  - **Auto-Compaction & Context Hygiene**: Automatic 85% context occupancy triggering, sliding tail turn preservation, standard compaction anchor generation, and manual `/compress` & `/compact` slash commands.
  - **Programmatic Tool Calling (PTC)**: Persistent REPL code execution kernel (`execute_code`) with pre-imported `from hermes_tools import ...` RPC bridge and 50KB stdout head/tail auto-spill.
  - **Task State Tracking**: Built-in `todo_list` engine with parent-child task hierarchy and post-compaction state injection.
  - **Multi-Platform Support**: Built-in native support for Telegram (verified 1-to-1), Discord, Slack, and WhatsApp (unit-tested & session-mapped).

---

## 🛠️ Built-in Tools Inventory (17 Core Tools)

Hermes Air exposes a comprehensive OpenAI function-calling toolset:

1. `terminal`: Asynchronous shell execution with timeout handling and merged stdout/stderr.
2. `read_file`: Line-paginated file reader with gutter numbering and 100KB budget guards.
3. `write_file`: Atomic file writer with automatic directory hierarchy creation.
4. `patch`: 9-strategy fuzzy matching patch engine with idempotency check and unified diffs.
5. `search_files`: Ripgrep-backed dual-mode regex content search and glob filename finder.
6. `web_search`: Multi-engine web search with SSRF protection.
7. `web_extract`: Clean markdown web content extractor with head/tail budget truncation.
8. `browser_navigate`: Safe browser navigation via CDP with SSRF private network rejection.
9. `browser_snapshot`: Textual accessibility DOM tree extractor.
10. `browser_click`: Element clicker via CSS selector or viewport coordinates.
11. `browser_type`: Form input filler with simulated DOM input events.
12. `browser_take_screenshot`: Native full-page capture via CDP returning `MEDIA:<path>` image attachments.
13. `browser_exec`: JavaScript/CDP execution directly in browser context.
14. `skills_list`: Skills directory scanner with YAML frontmatter parsing.
15. `skill_view`: Full skill markdown viewer with linked files resolution.
16. `todo_list`: In-memory revisioned task list manager for multi-step workflows.
17. `execute_code`: Persistent Python REPL session with `hermes_tools` RPC helpers.

---

## 🚀 Quick Start (One-Line Installer)

Install Hermes Air instantly with a single command:

```bash
curl -fsSL https://raw.githubusercontent.com/gb99xbear/hermes-air/main/install.sh | bash
```

Or build manually from source:

```bash
git clone https://github.com/gb99xbear/hermes-air.git
cd hermes-air
cargo build --release -p hermes-gateway
```

---

## 💻 CLI Usage

```bash
# Launch interactive configuration wizard
hermes-air setup

# Start interactive terminal chat
hermes-air

# Run a one-shot query and exit
hermes-air -q "Analyze the current system memory usage"

# Launch background multi-platform gateway
hermes-air gateway --config config.toml
```

---

## 🧪 Test Suite

Hermes Air includes a comprehensive automated test suite verifying its subsystems:

```bash
cargo test --workspace
```
*148 passing unit and integration tests with zero failures.*

---

## 📜 License

MIT License. Open-source, featherlight, and ready for production deployment.
