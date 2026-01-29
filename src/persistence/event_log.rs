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
pub struct EventSampleRule {
    pub prefix: String,
    pub rate: f64,
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
    pub max_total_bytes: u64,
    pub default_sample_rate: f64,
    pub sample_rates: Vec<EventSampleRule>,
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
            max_total_bytes: 2 * 1024 * 1024 * 1024,
            default_sample_rate: 1.0,
            sample_rates: Vec::new(),
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
    buffer: VecDeque<Vec<u8>>,
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
            logger.prune_to_budget()?;
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
        let event = event.into();
        if !self.should_sample(&event, ts_ms) {
            return Ok(());
        }
        let record = EventRecord {
            seq: self.next_seq(),
            ts_ms,
            event,
            payload,
        };
        let mut line = Vec::new();
        serde_json::to_writer(&mut line, &record)
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
        self.prune_to_budget()?;
        Ok(())
    }

    pub fn current_path(&self) -> &Path {
        &self.log_path
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    fn should_sample(&self, event: &str, ts_ms: i64) -> bool {
        let rate = clamp_sample_rate(self.sample_rate_for_event(event));
        if rate >= 1.0 {
            return true;
        }
        if rate <= 0.0 {
            return false;
        }
        let hash = sample_hash(event, ts_ms);
        let bucket = (hash as f64) / (u64::MAX as f64);
        bucket < rate
    }

    fn sample_rate_for_event(&self, event: &str) -> f64 {
        let mut best_rate = None;
        let mut best_len = 0usize;
        for rule in &self.config.sample_rates {
            if event.starts_with(&rule.prefix) {
                let len = rule.prefix.len();
                if len >= best_len {
                    best_rate = Some(rule.rate);
                    best_len = len;
                }
            }
        }
        best_rate.unwrap_or(self.config.default_sample_rate)
    }

    fn push_line(&mut self, line: Vec<u8>) {
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
            writer.write_all(&line)?;
            writer.write_all(b"\n")?;
            self.written_bytes = self.written_bytes.saturating_add((line.len() + 1) as u64);
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

    fn prune_to_budget(&mut self) -> io::Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let max_total_bytes = self.config.max_total_bytes;
        if max_total_bytes == 0 {
            return Ok(());
        }
        let (mut total_bytes, mut rotated) = collect_log_sizes(&self.log_path)?;
        if total_bytes <= max_total_bytes {
            return Ok(());
        }
        rotated.sort_by(|a, b| a.ts_ms.cmp(&b.ts_ms).then_with(|| a.path.cmp(&b.path)));
        for entry in rotated {
            if total_bytes <= max_total_bytes {
                break;
            }
            if fs::remove_file(&entry.path).is_ok() {
                total_bytes = total_bytes.saturating_sub(entry.size);
            }
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

fn clamp_sample_rate(rate: f64) -> f64 {
    if !rate.is_finite() {
        return 0.0;
    }
    rate.clamp(0.0, 1.0)
}

fn sample_hash(event: &str, ts_ms: i64) -> u64 {
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut hash = FNV_OFFSET;
    for byte in event.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    for byte in ts_ms.to_le_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[derive(Debug)]
struct RotatedLogEntry {
    path: PathBuf,
    size: u64,
    ts_ms: i64,
}

fn collect_log_sizes(log_path: &Path) -> io::Result<(u64, Vec<RotatedLogEntry>)> {
    let Some(log_dir) = log_path.parent() else {
        return Ok((0, Vec::new()));
    };
    let stem = log_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("events");
    let ext = log_path.extension().and_then(|value| value.to_str());
    let base_name = log_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let prefix = format!("{stem}.");
    let suffix = ext.map(|ext| format!(".{ext}"));

    let mut total_bytes = 0u64;
    let mut rotated = Vec::new();
    let entries = match fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((0, Vec::new())),
        Err(err) => return Err(err),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        let size = metadata.len();
        if name == base_name {
            total_bytes = total_bytes.saturating_add(size);
            continue;
        }
        if !name.starts_with(&prefix) {
            continue;
        }
        if let Some(suffix) = suffix.as_deref() {
            if !name.ends_with(suffix) {
                continue;
            }
        }
        let ts_ms = parse_rotated_ts(&name, stem, ext).unwrap_or(0);
        total_bytes = total_bytes.saturating_add(size);
        rotated.push(RotatedLogEntry { path, size, ts_ms });
    }

    Ok((total_bytes, rotated))
}

fn parse_rotated_ts(name: &str, stem: &str, ext: Option<&str>) -> Option<i64> {
    let prefix = format!("{stem}.");
    let mut middle = name.strip_prefix(&prefix)?;
    if let Some(ext) = ext {
        let suffix = format!(".{ext}");
        middle = middle.strip_suffix(&suffix)?;
    }
    middle.parse::<i64>().ok()
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

    fn write_bytes(path: &Path, size: usize) {
        let data = vec![b'x'; size];
        fs::write(path, data).expect("write bytes");
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

    #[test]
    fn prune_removes_oldest_rotated_files() {
        let dir = temp_dir("event_log_prune");
        let mut cfg = EventLogConfig::default();
        cfg.log_dir = dir.clone();
        cfg.file_prefix = "events".to_string();
        cfg.rotation_max_bytes = 0;
        cfg.max_total_bytes = 50;

        let current = cfg.log_path();
        let rotated_old = dir.join("events.100.jsonl");
        let rotated_new = dir.join("events.200.jsonl");

        write_bytes(&current, 10);
        write_bytes(&rotated_old, 30);
        write_bytes(&rotated_new, 30);

        let _logger = EventLogger::new(cfg).expect("create logger");

        assert!(!rotated_old.exists());
        assert!(rotated_new.exists());
        assert!(current.exists());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_keeps_current_file() {
        let dir = temp_dir("event_log_prune_current");
        let mut cfg = EventLogConfig::default();
        cfg.log_dir = dir.clone();
        cfg.file_prefix = "events".to_string();
        cfg.rotation_max_bytes = 0;
        cfg.max_total_bytes = 50;

        let current = cfg.log_path();
        let rotated_old = dir.join("events.100.jsonl");

        write_bytes(&current, 100);
        write_bytes(&rotated_old, 10);

        let _logger = EventLogger::new(cfg).expect("create logger");

        assert!(current.exists());
        assert!(!rotated_old.exists());

        let _ = fs::remove_dir_all(dir);
    }
}
