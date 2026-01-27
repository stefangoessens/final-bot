use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventRecord {
    pub seq: u64,
    pub ts_ms: i64,
    pub event: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct EventLogConfig {
    pub enabled: bool,
    pub log_dir: PathBuf,
    pub file_prefix: String,
    pub buffer_max_events: usize,
    pub buffer_max_bytes: usize,
    pub flush_threshold_events: usize,
    pub flush_threshold_bytes: usize,
    pub rotation_max_bytes: u64,
}

impl Default for EventLogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_dir: PathBuf::from("./logs"),
            file_prefix: "events".to_string(),
            buffer_max_events: 10_000,
            buffer_max_bytes: 8 * 1024 * 1024,
            flush_threshold_events: 1_000,
            flush_threshold_bytes: 512 * 1024,
            rotation_max_bytes: 256 * 1024 * 1024,
        }
    }
}

impl EventLogConfig {
    pub fn log_path(&self) -> PathBuf {
        build_log_path(&self.log_dir, &self.file_prefix)
    }
}

#[derive(Debug, Clone)]
pub struct RotationState {
    pub current_path: PathBuf,
    pub written_bytes: u64,
    pub created_ms: i64,
    pub now_ms: i64,
}

pub trait RotationHook: Send + Sync + std::fmt::Debug {
    fn should_rotate(&self, state: &RotationState) -> bool;
    fn rotate(&self, state: &RotationState) -> io::Result<()>;
}

#[derive(Debug)]
pub struct NoopRotationHook;

impl RotationHook for NoopRotationHook {
    fn should_rotate(&self, _state: &RotationState) -> bool {
        false
    }

    fn rotate(&self, _state: &RotationState) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct SizeRotationHook {
    max_bytes: u64,
}

impl SizeRotationHook {
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

impl RotationHook for SizeRotationHook {
    fn should_rotate(&self, state: &RotationState) -> bool {
        self.max_bytes > 0 && state.written_bytes >= self.max_bytes
    }

    fn rotate(&self, state: &RotationState) -> io::Result<()> {
        if !state.current_path.exists() {
            return Ok(());
        }
        let rotated_path = build_rotated_path(&state.current_path, state.now_ms);
        fs::rename(&state.current_path, rotated_path)?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct EventLogger {
    config: EventLogConfig,
    rotation_hook: Arc<dyn RotationHook>,
    buffer: VecDeque<String>,
    buffer_bytes: usize,
    dropped_events: u64,
    written_bytes: u64,
    created_ms: i64,
    seq: u64,
    log_path: PathBuf,
}

impl EventLogger {
    pub fn new(config: EventLogConfig) -> io::Result<Self> {
        let hook = default_rotation_hook(&config);
        Self::with_rotation_hook(config, hook)
    }

    pub fn with_rotation_hook(
        config: EventLogConfig,
        rotation_hook: Arc<dyn RotationHook>,
    ) -> io::Result<Self> {
        let log_path = config.log_path();
        let mut written_bytes = 0u64;
        if config.enabled {
            fs::create_dir_all(&config.log_dir)?;
            if let Ok(metadata) = fs::metadata(&log_path) {
                written_bytes = metadata.len();
            }
        }
        let mut logger = Self {
            config,
            rotation_hook,
            buffer: VecDeque::new(),
            buffer_bytes: 0,
            dropped_events: 0,
            written_bytes,
            created_ms: now_ms(),
            seq: 0,
            log_path,
        };
        if logger.config.enabled {
            logger.maybe_rotate()?;
        }
        Ok(logger)
    }

    pub fn log<T: Serialize>(&mut self, event: impl Into<String>, payload: &T) -> io::Result<()> {
        let value = serde_json::to_value(payload)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.log_value(event, value)
    }

    pub fn log_with_ts<T: Serialize>(
        &mut self,
        event: impl Into<String>,
        payload: &T,
        ts_ms: i64,
    ) -> io::Result<()> {
        let value = serde_json::to_value(payload)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.log_value_with_ts(event, value, ts_ms)
    }

    pub fn log_value(&mut self, event: impl Into<String>, payload: Value) -> io::Result<()> {
        let ts_ms = now_ms();
        self.log_value_with_ts(event, payload, ts_ms)
    }

    pub fn log_value_with_ts(
        &mut self,
        event: impl Into<String>,
        payload: Value,
        ts_ms: i64,
    ) -> io::Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let record = EventRecord {
            seq: self.next_seq(),
            ts_ms,
            event: event.into(),
            payload,
        };
        let line = serde_json::to_string(&record)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.push_line(line);
        if self.should_flush() {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if !self.config.enabled || self.buffer.is_empty() {
            return Ok(());
        }
        self.flush_buffer()?;
        if self.dropped_events > 0 {
            tracing::warn!(
                target: "event_log",
                dropped_events = self.dropped_events,
                "event log dropped entries due to buffer limits"
            );
        }
        self.maybe_rotate()?;
        Ok(())
    }

    pub fn current_path(&self) -> &Path {
        &self.log_path
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    fn push_line(&mut self, line: String) {
        let line_bytes = line.len() + 1;
        if line_bytes > self.config.buffer_max_bytes {
            self.dropped_events = self.dropped_events.saturating_add(1);
            return;
        }
        while self.buffer.len() >= self.config.buffer_max_events
            || self.buffer_bytes + line_bytes > self.config.buffer_max_bytes
        {
            if let Some(evicted) = self.buffer.pop_front() {
                self.buffer_bytes = self
                    .buffer_bytes
                    .saturating_sub(evicted.len().saturating_add(1));
                self.dropped_events = self.dropped_events.saturating_add(1);
            } else {
                break;
            }
        }
        self.buffer.push_back(line);
        self.buffer_bytes = self.buffer_bytes.saturating_add(line_bytes);
    }

    fn should_flush(&self) -> bool {
        self.buffer.len() >= self.config.flush_threshold_events
            || self.buffer_bytes >= self.config.flush_threshold_bytes
    }

    fn flush_buffer(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        let mut writer = BufWriter::new(file);
        while let Some(line) = self.buffer.pop_front() {
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
            self.written_bytes = self
                .written_bytes
                .saturating_add((line.len() + 1) as u64);
            self.buffer_bytes = self
                .buffer_bytes
                .saturating_sub(line.len().saturating_add(1));
        }
        writer.flush()?;
        Ok(())
    }

    fn maybe_rotate(&mut self) -> io::Result<()> {
        let now = now_ms();
        let state = RotationState {
            current_path: self.log_path.clone(),
            written_bytes: self.written_bytes,
            created_ms: self.created_ms,
            now_ms: now,
        };
        if self.rotation_hook.should_rotate(&state) {
            self.rotation_hook.rotate(&state)?;
            self.written_bytes = 0;
            self.created_ms = now;
        }
        Ok(())
    }
}

impl Drop for EventLogger {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn build_log_path(log_dir: &Path, file_prefix: &str) -> PathBuf {
    let mut name = file_prefix.to_string();
    if !name.ends_with(".jsonl") {
        name.push_str(".jsonl");
    }
    log_dir.join(name)
}

fn build_rotated_path(path: &Path, ts_ms: i64) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("events");
    let ext = path.extension().and_then(|value| value.to_str());
    let name = match ext {
        Some(ext) => format!("{stem}.{ts_ms}.{ext}"),
        None => format!("{stem}.{ts_ms}"),
    };
    path.with_file_name(name)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn default_rotation_hook(config: &EventLogConfig) -> Arc<dyn RotationHook> {
    if config.rotation_max_bytes > 0 {
        Arc::new(SizeRotationHook::new(config.rotation_max_bytes))
    } else {
        Arc::new(NoopRotationHook)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn temp_dir(prefix: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        dir.push(format!("{prefix}_{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn jsonl_serialization_format() {
        let dir = temp_dir("event_log_test");
        let mut cfg = EventLogConfig::default();
        cfg.log_dir = dir.clone();
        cfg.file_prefix = "events".to_string();
        cfg.flush_threshold_events = 1;
        cfg.flush_threshold_bytes = 1;
        let mut logger = EventLogger::new(cfg).expect("create logger");

        logger
            .log_value_with_ts("test.event", json!({"a": 1}), 123)
            .expect("log event");
        logger.flush().expect("flush");

        let content = fs::read_to_string(logger.current_path()).expect("read log");
        assert!(content.ends_with('\n'));
        let line = content.lines().next().expect("line");
        let record: EventRecord = serde_json::from_str(line).expect("parse json");
        assert_eq!(record.seq, 1);
        assert_eq!(record.ts_ms, 123);
        assert_eq!(record.event, "test.event");
        assert_eq!(record.payload, json!({"a": 1}));

        let _ = fs::remove_dir_all(dir);
    }

    fn list_rotated(dir: &Path, prefix: &str) -> Vec<PathBuf> {
        let mut matches = Vec::new();
        let base_name = format!("{prefix}.jsonl");
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&format!("{prefix}."))
                    && name.ends_with(".jsonl")
                    && name != base_name
                {
                    matches.push(entry.path());
                }
            }
        }
        matches
    }

    #[test]
    fn size_rotation_hook_rotates_on_threshold() {
        let dir = temp_dir("event_log_rotate");
        let mut cfg = EventLogConfig::default();
        cfg.log_dir = dir.clone();
        cfg.file_prefix = "events".to_string();
        cfg.flush_threshold_events = 1;
        cfg.flush_threshold_bytes = 1;
        let hook = Arc::new(SizeRotationHook::new(1));
        let mut logger = EventLogger::with_rotation_hook(cfg, hook).expect("create logger");

        logger
            .log_value_with_ts("rotate", json!({"a": 1}), 10)
            .expect("log event");
        logger.flush().expect("flush");

        let rotated = list_rotated(&dir, "events");
        assert_eq!(rotated.len(), 1);
        let content = fs::read_to_string(&rotated[0]).expect("read rotated log");
        assert!(content.contains("\"event\":\"rotate\""));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn size_rotation_hook_skips_below_threshold() {
        let dir = temp_dir("event_log_no_rotate");
        let mut cfg = EventLogConfig::default();
        cfg.log_dir = dir.clone();
        cfg.file_prefix = "events".to_string();
        cfg.flush_threshold_events = 1;
        cfg.flush_threshold_bytes = 1;
        let hook = Arc::new(SizeRotationHook::new(1_000_000));
        let mut logger = EventLogger::with_rotation_hook(cfg, hook).expect("create logger");

        logger
            .log_value_with_ts("keep", json!({"a": 1}), 10)
            .expect("log event");
        logger.flush().expect("flush");

        let rotated = list_rotated(&dir, "events");
        assert!(rotated.is_empty());
        assert!(logger.current_path().exists());

        let _ = fs::remove_dir_all(dir);
    }
}
