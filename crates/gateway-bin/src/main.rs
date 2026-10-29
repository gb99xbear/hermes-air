// Hermes Gateway - Main Binary
use hermes_config::{ConfigLoader, LogFormat, LogOutput, LoggingConfig};
use hermes_gateway_core::Gateway;
use clap::Parser;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(name = "hermes-gateway", version, about = "Hermes Gateway - Rust Rewrite")]
struct Args {
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    #[arg(long)]
    validate: bool,
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

fn init_logging(cfg: &LoggingConfig) -> anyhow::Result<()> {
    // RUST_LOG env var overrides the config file level when set.
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

        // First write exceeds max_bytes=1, so the (empty) original file rotates to .1
        // and the new content lands in a fresh file.
        assert!(path.exists());
        assert!(rotated_name(&path, 1).exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "AAAA");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_log_writer_stdout_variant_writes() {
        let mut w = LogWriter(LogSink::Stdout);
        // Should not panic; stdout write result ignored.
        let _ = w.write(b"smoke");
        let _ = w.flush();
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let config = ConfigLoader::load(&args.config)?;
    init_logging(&config.logging)?;

    if args.validate {
        println!("Configuration valid!");
        println!("Profiles: {}", config.profiles.len());
        println!("Platforms: {}", config.platforms.len());
        return Ok(());
    }

    let gateway = Gateway::new(config).await?;
    gateway.run().await
}
