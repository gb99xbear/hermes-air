pub mod banner;
pub mod setup;
// Hermes Air - Main Binary (CLI & Gateway)
use hermes_config::{ConfigLoader, LogFormat, LogOutput, LoggingConfig};
use hermes_gateway_core::{commands::parse_command, Gateway, TurnEvent};
use clap::{Parser, Subcommand};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{fmt, EnvFilter};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(
    name = "hermes-air",
    version,
    about = "Hermes Air - Featherlight AI Agent CLI & Gateway"
)]
pub struct Cli {
    #[arg(short, long, default_value = "config.toml", global = true)]
    pub config: String,

    #[arg(long, global = true)]
    pub validate: bool,

    #[arg(short, long, global = true, help = "One-shot query mode")]
    pub query: Option<String>,

    #[arg(short, long, global = true, help = "Model override for session")]
    pub model: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum Commands {
    /// Interactive terminal chat REPL (default mode)
    Chat {
        #[arg(short, long, help = "Model override for session")]
        model: Option<String>,
    },
    /// Run multi-platform gateway daemon
    Gateway,
    /// Interactive setup wizard for models, agent settings, tools, and messaging platforms
    Setup {
        #[arg(help = "Optional setup section: model, agent, tools, gateway")]
        section: Option<String>,
    },
}

/// Rotating log file writer. Rotates the current file into `.1`, `.2`, ... up to
/// `max_files` generations when it exceeds `max_bytes`.
#[derive(Clone)]
struct RotatingFileWriter {
    inner: Arc<Mutex<RotatingFileInner>>,
}

struct RotatingFileInner {
    path: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    max_files: u32,
}

fn rotated_name(path: &Path, generation: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}", generation));
    PathBuf::from(name)
}

impl RotatingFileInner {
    fn rotate(&mut self) -> io::Result<()> {
        if self.max_files == 0 {
            // No retention configured: just truncate and start over.
            self.file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)?;
            self.written = 0;
            return Ok(());
        }

        // Drop the oldest generation if it exists.
        let oldest = rotated_name(&self.path, self.max_files);
        let _ = std::fs::remove_file(&oldest);

        // Shift existing generations up by one.
        for gen in (1..self.max_files).rev() {
            let from = rotated_name(&self.path, gen);
            let to = rotated_name(&self.path, gen + 1);
            if from.exists() {
                let _ = std::fs::rename(from, to);
            }
        }

        // Move the current file into generation 1 and start a fresh file.
        let first = rotated_name(&self.path, 1);
        let _ = std::fs::rename(&self.path, &first);
        self.file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl RotatingFileWriter {
    fn new(path: &Path, max_bytes: u64, max_files: u32) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            inner: Arc::new(Mutex::new(RotatingFileInner {
                path: path.to_path_buf(),
                file,
                written,
                max_bytes,
                max_files,
            })),
        })
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        if inner.max_bytes > 0 && inner.written + buf.len() as u64 > inner.max_bytes {
            inner.rotate()?;
        }
        let n = inner.file.write(buf)?;
        inner.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().unwrap().file.flush()
    }
}

#[derive(Clone)]
enum LogSink {
    Stdout,
    Stderr,
    File(RotatingFileWriter),
    Both(RotatingFileWriter),
}

/// Single concrete writer type so tracing-subscriber can build a `MakeWriter`
/// closure from a cheap `Clone` regardless of configured sink.
#[derive(Clone)]
struct LogWriter(LogSink);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.0 {
            LogSink::Stdout => io::stdout().write(buf),
            LogSink::Stderr => io::stderr().write(buf),
            LogSink::File(f) => f.write(buf),
            LogSink::Both(f) => {
                let _ = io::stdout().write(buf);
                f.write(buf)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.0 {
            LogSink::Stdout => io::stdout().flush(),
            LogSink::Stderr => io::stderr().flush(),
            LogSink::File(f) => f.flush(),
            LogSink::Both(f) => {
                let _ = io::stdout().flush();
                f.flush()
            }
        }
    }
}

fn build_log_writer(cfg: &LoggingConfig) -> io::Result<LogWriter> {
    let file_writer = |cfg: &LoggingConfig| -> io::Result<RotatingFileWriter> {
        let path = cfg
            .file
            .clone()
            .unwrap_or_else(|| PathBuf::from("hermes-gateway.log"));
        let max_bytes = cfg.max_file_size_mb.saturating_mul(1024 * 1024);
        RotatingFileWriter::new(&path, max_bytes, cfg.max_files)
    };

    let sink = match cfg.output {
        LogOutput::Stdout => LogSink::Stdout,
        LogOutput::Stderr => LogSink::Stderr,
        LogOutput::File => LogSink::File(file_writer(cfg)?),
        LogOutput::Both => LogSink::Both(file_writer(cfg)?),
    };
    Ok(LogWriter(sink))
}

fn init_logging(cfg: &LoggingConfig, quiet: bool) -> anyhow::Result<()> {
    if quiet {
        return Ok(());
    }

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cfg.level.clone()));

    let writer = build_log_writer(cfg)?;
    let builder = fmt()
        .with_env_filter(filter)
        .with_writer(move || writer.clone());

    match cfg.format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.init(),
        LogFormat::Compact => builder.compact().init(),
    }
    Ok(())
}

/// Run one-shot query mode: execute single turn and print output
async fn run_oneshot_query(
    gateway: &Gateway,
    prompt: &str,
    model_override: Option<&str>,
) -> anyhow::Result<()> {
    let processor = gateway.turn_processor();
    let session_id = Uuid::new_v4();
    let token = CancellationToken::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);

    let prompt_owned = prompt.to_string();
    let model_owned = model_override.map(|s| s.to_string());
    let token_clone = token.clone();
    let processor_clone = processor.clone();

    let task = tokio::spawn(async move {
        processor_clone
            .process_turn(
                session_id,
                &prompt_owned,
                model_owned.as_deref(),
                token_clone,
                tx,
            )
            .await
    });

    let mut printed_tokens = false;
    while let Some(event) = rx.recv().await {
        match event {
            TurnEvent::Token(token_str) => {
                print!("{}", token_str);
                let _ = io::stdout().flush();
                printed_tokens = true;
            }
            TurnEvent::ToolExecuting { name, .. } => {
                eprintln!("\x1b[33m🔧 Executing tool: {}\x1b[0m", name);
            }
            TurnEvent::ToolCompleted { name, success, .. } => {
                if !success {
                    eprintln!("\x1b[31m❌ Tool {} failed\x1b[0m", name);
                }
            }
            TurnEvent::SlashReply(reply) => {
                println!("{}", reply);
            }
            TurnEvent::Finished(text) => {
                if !printed_tokens && !text.is_empty() {
                    println!("{}", text);
                } else if printed_tokens {
                    println!();
                }
            }
            TurnEvent::Error(err) => {
                eprintln!("\x1b[31mError: {}\x1b[0m", err);
            }
            TurnEvent::Cancelled => {
                eprintln!("\x1b[33mTurn cancelled.\x1b[0m");
            }
            _ => {}
        }
    }

    let _ = task.await?;
    Ok(())
}

/// Print Hermes Air startup banner & dashboard
fn print_interactive_banner(model: &str, session_id: &str) {
    banner::print_startup_banner(model, session_id);
}

/// Interactive Chat REPL mode
async fn run_interactive_chat(
    gateway: &Gateway,
    initial_model: Option<&str>,
) -> anyhow::Result<()> {
    let processor = gateway.turn_processor();
    let session_id = Uuid::new_v4();
    let mut current_model_override = initial_model.map(|s| s.to_string());

    let active_model_display = current_model_override
        .clone()
        .unwrap_or_else(|| "ag/gemini-3.8-flash".to_string());

    print_interactive_banner(&active_model_display, &session_id.to_string());

    let mut rl = DefaultEditor::new()?;
    let history_path = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".hermes_air_history"));
    if let Some(ref path) = history_path {
        let _ = rl.load_history(path);
    }

    loop {
        let prompt_str = "\x1b[1;32mHermes Air 🍃 > \x1b[0m";
        let readline = rl.readline(prompt_str);

        match readline {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let _ = rl.add_history_entry(trimmed);

                // Check for local exit commands
                if trimmed == "exit" || trimmed == "quit" || trimmed == "/exit" || trimmed == "/quit" {
                    println!("\x1b[32mFarewell!\x1b[0m");
                    break;
                }

                // Check for model switch command
                if let Some(cmd) = parse_command(trimmed) {
                    if let hermes_gateway_core::commands::SlashCommand::Model(Some(ref new_m)) = cmd {
                        let target = new_m.trim().to_string();
                        current_model_override = Some(target.clone());
                        println!("\x1b[1;32m⭐ Model switched to: \x1b[33m{}\x1b[0m", target);
                        continue;
                    }
                }

                let token = CancellationToken::new();
                let (tx, mut rx) = tokio::sync::mpsc::channel(100);

                let input_str = trimmed.to_string();
                let model_str = current_model_override.clone();
                let token_clone = token.clone();
                let proc_clone = processor.clone();

                let turn_handle = tokio::spawn(async move {
                    proc_clone
                        .process_turn(
                            session_id,
                            &input_str,
                            model_str.as_deref(),
                            token_clone,
                            tx,
                        )
                        .await
                });

                let mut printed_tokens = false;
                while let Some(event) = rx.recv().await {
                    match event {
                        TurnEvent::Token(token_str) => {
                            print!("{}", token_str);
                            let _ = io::stdout().flush();
                            printed_tokens = true;
                        }
                        TurnEvent::ToolExecuting { name, .. } => {
                            println!("\x1b[33m🔧 Executing tool: {}\x1b[0m...", name);
                        }
                        TurnEvent::ToolCompleted { name, success, .. } => {
                            if !success {
                                println!("\x1b[31m❌ Tool {} returned an error\x1b[0m", name);
                            }
                        }
                        TurnEvent::SlashReply(reply) => {
                            println!("{}", reply);
                        }
                        TurnEvent::ContextCleared => {
                            println!("\x1b[1;32m🧹 Session conversation memory cleared.\x1b[0m");
                        }
                        TurnEvent::Finished(text) => {
                            if !printed_tokens && !text.is_empty() {
                                println!("{}", text);
                            } else if printed_tokens {
                                println!();
                            }
                        }
                        TurnEvent::Error(err) => {
                            println!("\x1b[31mError: {}\x1b[0m", err);
                        }
                        TurnEvent::Cancelled => {
                            println!("\x1b[33mTurn cancelled.\x1b[0m");
                        }
                    }
                }

                let _ = turn_handle.await;
                println!();
            }
            Err(ReadlineError::Interrupted) => {
                println!("\x1b[33m^C (Type /exit or quit to close)\x1b[0m");
            }
            Err(ReadlineError::Eof) => {
                println!("\x1b[32mFarewell!\x1b[0m");
                break;
            }
            Err(err) => {
                eprintln!("\x1b[31mReadline error: {:?}\x1b[0m", err);
                break;
            }
        }
    }

    if let Some(ref path) = history_path {
        let _ = rl.save_history(path);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Run setup wizard if requested
    if let Some(Commands::Setup { ref section }) = cli.command {
        banner::print_setup_header();
        return setup::run_setup_wizard(&cli.config, section.as_deref());
    }

    // Check if running interactive or oneshot mode to quiet the file/console logging
    let is_interactive = matches!(cli.command, None | Some(Commands::Chat { .. }));
    let is_oneshot = cli.query.is_some();
    let quiet_logging = is_interactive || is_oneshot;

    let config = ConfigLoader::load(&cli.config)?;
    init_logging(&config.logging, quiet_logging)?;

    if cli.validate {
        println!("Configuration valid!");
        println!("Profiles: {}", config.profiles.len());
        println!("Platforms: {}", config.platforms.len());
        return Ok(());
    }

    let gateway = Gateway::new(config).await?;

    // 1. One-shot query mode (-q, --query <TEXT>)
    if let Some(ref prompt) = cli.query {
        return run_oneshot_query(&gateway, prompt, cli.model.as_deref()).await;
    }

    // 2. Dispatch based on subcommand
    match cli.command {
        Some(Commands::Setup { ref section }) => {
            banner::print_setup_header();
            setup::run_setup_wizard(&cli.config, section.as_deref())
        }
        Some(Commands::Gateway) => {
            gateway.run().await
        }
        Some(Commands::Chat { model }) => {
            let selected_model = model.as_deref().or(cli.model.as_deref());
            run_interactive_chat(&gateway, selected_model).await
        }
        None => {
            // Default behavior: interactive chat REPL
            run_interactive_chat(&gateway, cli.model.as_deref()).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rotating_file_writer_rotates_when_exceeding_max_bytes() {
        let dir = std::env::temp_dir().join(format!("hermes-log-rot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.log");

        let mut w = RotatingFileWriter::new(&path, 1, 2).unwrap();
        w.write_all(b"AAAA").unwrap();

        assert!(path.exists());
        assert!(rotated_name(&path, 1).exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "AAAA");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_log_writer_stdout_variant_writes() {
        let mut w = LogWriter(LogSink::Stdout);
        let _ = w.write(b"smoke");
        let _ = w.flush();
    }

    #[test]
    fn test_cli_parsing_default_to_chat() {
        let cli = Cli::try_parse_from(&["hermes-air"]).unwrap();
        assert!(cli.command.is_none());
        assert!(cli.query.is_none());
    }

    #[test]
    fn test_cli_parsing_chat_subcommand() {
        let cli = Cli::try_parse_from(&["hermes-air", "chat", "--model", "custom-model"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Commands::Chat {
                model: Some("custom-model".to_string())
            })
        );
    }

    #[test]
    fn test_cli_parsing_setup_subcommand() {
        let cli = Cli::try_parse_from(&["hermes-air", "setup"]).unwrap();
        assert_eq!(cli.command, Some(Commands::Setup { section: None }));

        let cli_sec = Cli::try_parse_from(&["hermes-air", "setup", "model"]).unwrap();
        assert_eq!(
            cli_sec.command,
            Some(Commands::Setup {
                section: Some("model".to_string())
            })
        );
    }

    #[test]
    fn test_cli_parsing_gateway_subcommand() {
        let cli = Cli::try_parse_from(&["hermes-air", "gateway", "-c", "custom.toml"]).unwrap();
        assert_eq!(cli.command, Some(Commands::Gateway));
        assert_eq!(cli.config, "custom.toml");
    }

    #[test]
    fn test_cli_parsing_oneshot_query() {
        let cli = Cli::try_parse_from(&["hermes-air", "-q", "hello world", "-m", "fast-model"]).unwrap();
        assert_eq!(cli.query.as_deref(), Some("hello world"));
        assert_eq!(cli.model.as_deref(), Some("fast-model"));
    }
}
