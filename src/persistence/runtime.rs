use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{self, Duration, MissedTickBehavior};

use crate::persistence::event_log::{EventLogConfig, EventLogger};

const DEFAULT_FLUSH_INTERVAL_MS: u64 = 500;

#[derive(Debug)]
pub struct LogEvent {
    pub ts_ms: i64,
    pub event: String,
    pub payload: Value,
}

/// Spawns a background logger task and returns a bounded sender.
/// Callers should prefer `try_send` to keep hot paths non-blocking.
pub fn spawn_event_logger(config: EventLogConfig) -> mpsc::Sender<LogEvent> {
    let capacity = config.buffer_max_events.max(1);
    let (tx, mut rx) = mpsc::channel(capacity);

    tokio::spawn(async move {
        let mut logger = match EventLogger::new(config) {
            Ok(logger) => logger,
            Err(err) => {
                tracing::error!(
                    target: "event_log",
                    error = %err,
                    "failed to initialize event logger"
                );
                return;
            }
        };

        let mut ticker = time::interval(Duration::from_millis(DEFAULT_FLUSH_INTERVAL_MS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                maybe_event = rx.recv() => {
                    match maybe_event {
                        Some(event) => {
                            handle_event(&mut logger, event);
                            while let Ok(event) = rx.try_recv() {
                                handle_event(&mut logger, event);
                            }
                        }
                        None => break,
                    }
                }
                _ = ticker.tick() => {
                    if let Err(err) = logger.flush() {
                        tracing::warn!(
                            target: "event_log",
                            error = %err,
                            "event log flush failed"
                        );
                    }
                }
            }
        }

        if let Err(err) = logger.flush() {
            tracing::warn!(
                target: "event_log",
                error = %err,
                "event log flush failed"
            );
        }
    });

    tx
}

fn handle_event(logger: &mut EventLogger, event: LogEvent) {
    let LogEvent {
        ts_ms,
        event,
        payload,
    } = event;
    if let Err(err) = logger.log_value_with_ts(event, payload, ts_ms) {
        tracing::warn!(
            target: "event_log",
            error = %err,
            "event log write failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::event_log::EventRecord;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

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

    async fn wait_for_log(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if path.exists() {
                if let Ok(content) = fs::read_to_string(path) {
                    if !content.is_empty() {
                        return;
                    }
                }
            }
            if Instant::now() >= deadline {
                panic!("timeout waiting for event log");
            }
            time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn runtime_logger_writes_jsonl() {
        let dir = temp_dir("event_log_runtime");
        let mut cfg = EventLogConfig::default();
        cfg.log_dir = dir.clone();
        cfg.file_prefix = "events".to_string();
        cfg.flush_threshold_events = 1;
        cfg.flush_threshold_bytes = 1;
        let log_path = cfg.log_path();

        let tx = spawn_event_logger(cfg);
        tx.try_send(LogEvent {
            ts_ms: 123,
            event: "runtime.test".to_string(),
            payload: json!({"a": 1}),
        })
        .expect("try_send event");
        drop(tx);

        wait_for_log(&log_path).await;

        let content = fs::read_to_string(&log_path).expect("read log");
        let line = content.lines().next().expect("line");
        let record: EventRecord = serde_json::from_str(line).expect("parse json");
        assert_eq!(record.seq, 1);
        assert_eq!(record.ts_ms, 123);
        assert_eq!(record.event, "runtime.test");
        assert_eq!(record.payload, json!({"a": 1}));

        let _ = fs::remove_dir_all(dir);
    }
}
