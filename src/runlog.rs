use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::Value;

#[derive(Debug)]
pub struct RunLogger {
    path: PathBuf,
    writer: Mutex<BufWriter<File>>,
}

impl RunLogger {
    pub fn create(run_dir: &Path, command: &str) -> Result<Self> {
        let path = run_dir.join("run.log");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("failed to create run log: {}", path.display()))?;
        let logger = Self {
            path,
            writer: Mutex::new(BufWriter::new(file)),
        };
        logger.info(
            "run_start",
            &format!(
                "command={} version={} pid={}",
                quoted(command),
                quoted(env!("CARGO_PKG_VERSION")),
                std::process::id()
            ),
        );
        Ok(logger)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn config(&self, config: &Value) {
        let Some(entries) = config.as_object() else {
            self.warn("config_invalid", "configuration is not a JSON object");
            return;
        };
        for (key, value) in entries {
            self.write("CONFIG", "config", &format!("{key}={value}"));
        }
        self.info("config_complete", &format!("entries={}", entries.len()));
    }

    pub fn debug(&self, event: &str, message: &str) {
        self.write("DEBUG", event, message);
    }

    pub fn info(&self, event: &str, message: &str) {
        self.write("INFO", event, message);
    }

    pub fn warn(&self, event: &str, message: &str) {
        self.write("WARN", event, message);
    }

    pub fn error(&self, event: &str, message: &str) {
        self.write("ERROR", event, message);
    }

    pub fn sync(&self) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("run log mutex poisoned"))?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    }

    pub fn start_heartbeat(self: &Arc<Self>) -> RunLogHeartbeat {
        let logger = self.clone();
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await;
            loop {
                interval.tick().await;
                logger.debug(
                    "heartbeat",
                    "status=running; the last non-heartbeat event is the current pipeline step",
                );
            }
        });
        RunLogHeartbeat { handle }
    }

    fn write(&self, level: &str, event: &str, message: &str) {
        let event = one_line(event);
        let message = one_line(message);
        let result = (|| -> Result<()> {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| anyhow::anyhow!("run log mutex poisoned"))?;
            let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            writeln!(writer, "{timestamp} {level:<6} {event:<28} {message}")?;
            writer.flush()?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("[RUN-LOG] failed to write {}: {error}", self.path.display());
        }
    }
}

pub struct RunLogHeartbeat {
    handle: tokio::task::JoinHandle<()>,
}

impl RunLogHeartbeat {
    pub fn stop(&self) {
        self.handle.abort();
    }
}

impl Drop for RunLogHeartbeat {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn one_line(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            character if character.is_control() => "?".chars().collect(),
            character => vec![character],
        })
        .collect()
}

pub fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<unprintable>\"".to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn writes_timestamped_single_line_text_and_redacted_config() {
        let temporary = tempfile::tempdir().unwrap();
        let logger = RunLogger::create(temporary.path(), "taskgen generate").unwrap();
        logger.config(&serde_json::json!({
            "api_key": "[REDACTED: configured]",
            "count": 2,
            "model": "test/model"
        }));
        logger.warn("retry", "first line\nsecond line");
        logger.sync().unwrap();

        let contents = std::fs::read_to_string(logger.path()).unwrap();
        assert!(contents.contains("api_key=\"[REDACTED: configured]\""));
        assert!(!contents.contains("first line\nsecond line"));
        assert!(contents.contains("first line\\nsecond line"));
        for line in contents.lines() {
            let timestamp = line.split_whitespace().next().unwrap();
            assert!(chrono::DateTime::parse_from_rfc3339(timestamp).is_ok());
        }
    }

    #[test]
    fn concurrent_events_are_complete_lines() {
        let temporary = tempfile::tempdir().unwrap();
        let logger = Arc::new(RunLogger::create(temporary.path(), "taskgen review").unwrap());
        let threads: Vec<_> = (0..8)
            .map(|index| {
                let logger = logger.clone();
                std::thread::spawn(move || {
                    logger.debug("candidate", &format!("sequence={index}"));
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        logger.sync().unwrap();

        let contents = std::fs::read_to_string(logger.path()).unwrap();
        assert_eq!(contents.lines().count(), 9);
    }
}
