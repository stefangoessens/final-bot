use crate::persistence::event_log::EventRecord;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    pub ordering: ReplayOrdering,
    pub strict: bool,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            ordering: ReplayOrdering::BySeq,
            strict: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayOrdering {
    InFileOrder,
    BySeq,
    ByTimestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayControl {
    Continue,
    Stop,
}

pub trait EventHandler {
    fn handle_event(&mut self, event: &EventRecord) -> ReplayControl;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaySummary {
    pub total_events: usize,
    pub processed_events: usize,
    pub first_ts_ms: Option<i64>,
    pub last_ts_ms: Option<i64>,
}

pub struct ReplayRunner {
    config: ReplayConfig,
}

impl ReplayRunner {
    pub fn new(config: ReplayConfig) -> Self {
        Self { config }
    }

    pub fn run<P: AsRef<Path>, H: EventHandler>(
        &self,
        path: P,
        handler: &mut H,
    ) -> io::Result<ReplaySummary> {
        if self.config.ordering == ReplayOrdering::InFileOrder {
            return run_in_file_order(path.as_ref(), self.config.strict, handler);
        }
        let mut events = read_events(path.as_ref(), self.config.strict)?;
        order_events(&mut events, self.config.ordering);

        let total_events = events.len();
        let first_ts_ms = events.first().map(|entry| entry.event.ts_ms);
        let last_ts_ms = events.last().map(|entry| entry.event.ts_ms);

        let mut processed_events = 0usize;
        for entry in events {
            processed_events = processed_events.saturating_add(1);
            if matches!(handler.handle_event(&entry.event), ReplayControl::Stop) {
                break;
            }
        }

        Ok(ReplaySummary {
            total_events,
            processed_events,
            first_ts_ms,
            last_ts_ms,
        })
    }
}

#[derive(Debug, Clone)]
pub struct StressConfig {
    pub ordering: ReplayOrdering,
    pub speed: f64,
    pub sleep: bool,
    pub strict: bool,
}

impl Default for StressConfig {
    fn default() -> Self {
        Self {
            ordering: ReplayOrdering::BySeq,
            speed: 1.0,
            sleep: false,
            strict: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StressSummary {
    pub total_events: usize,
    pub processed_events: usize,
    pub simulated_duration_ms: Option<i64>,
    pub wall_sleep_ms: u64,
    pub processing_min_ns: Option<u64>,
    pub processing_mean_ns: Option<u64>,
    pub processing_p95_ns: Option<u64>,
}

pub struct StressRunner {
    config: StressConfig,
}

impl StressRunner {
    pub fn new(config: StressConfig) -> Self {
        Self { config }
    }

    pub fn run<P: AsRef<Path>, H: EventHandler>(
        &self,
        path: P,
        handler: &mut H,
    ) -> io::Result<StressSummary> {
        if self.config.speed <= 0.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "speed must be > 0",
            ));
        }
        if self.config.ordering == ReplayOrdering::InFileOrder {
            return run_stress_in_file_order(path.as_ref(), &self.config, handler);
        }
        let mut events = read_events(path.as_ref(), self.config.strict)?;
        order_events(&mut events, self.config.ordering);

        let total_events = events.len();
        let first_ts_ms = events.first().map(|entry| entry.event.ts_ms);
        let last_ts_ms = events.last().map(|entry| entry.event.ts_ms);
        let simulated_duration_ms = match (first_ts_ms, last_ts_ms) {
            (Some(first), Some(last)) => Some(last.saturating_sub(first)),
            _ => None,
        };

        let mut processed_events = 0usize;
        let mut wall_sleep_ms = 0u64;
        let mut prev_ts_ms: Option<i64> = None;
        let mut processing_ns = Vec::new();
        for entry in events {
            if let Some(prev) = prev_ts_ms {
                let delta = entry.event.ts_ms.saturating_sub(prev);
                if delta > 0 {
                    let scaled = ((delta as f64) / self.config.speed).round() as u64;
                    if self.config.sleep && scaled > 0 {
                        std::thread::sleep(Duration::from_millis(scaled));
                    }
                    wall_sleep_ms = wall_sleep_ms.saturating_add(scaled);
                }
            }
            prev_ts_ms = Some(entry.event.ts_ms);

            let start = Instant::now();
            let control = handler.handle_event(&entry.event);
            let elapsed = start.elapsed();
            processing_ns.push(duration_to_ns(elapsed));
            processed_events = processed_events.saturating_add(1);
            if matches!(control, ReplayControl::Stop) {
                break;
            }
        }

        let (processing_min_ns, processing_mean_ns, processing_p95_ns) =
            processing_stats(&processing_ns);

        Ok(StressSummary {
            total_events,
            processed_events,
            simulated_duration_ms,
            wall_sleep_ms,
            processing_min_ns,
            processing_mean_ns,
            processing_p95_ns,
        })
    }
}

struct IndexedEvent {
    line_no: usize,
    event: EventRecord,
}

fn read_events(path: &Path, strict: bool) -> io::Result<Vec<IndexedEvent>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<EventRecord>(&line) {
            Ok(event) => events.push(IndexedEvent { line_no, event }),
            Err(err) if strict => return Err(io::Error::new(io::ErrorKind::InvalidData, err)),
            Err(_) => continue,
        }
    }
    Ok(events)
}

fn order_events(events: &mut [IndexedEvent], ordering: ReplayOrdering) {
    match ordering {
        ReplayOrdering::InFileOrder => {}
        ReplayOrdering::BySeq => events.sort_by_key(|entry| (entry.event.seq, entry.line_no)),
        ReplayOrdering::ByTimestamp => {
            events.sort_by_key(|entry| (entry.event.ts_ms, entry.event.seq, entry.line_no))
        }
    }
}

fn run_in_file_order<H: EventHandler>(
    path: &Path,
    strict: bool,
    handler: &mut H,
) -> io::Result<ReplaySummary> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut total_events = 0usize;
    let mut processed_events = 0usize;
    let mut first_ts_ms: Option<i64> = None;
    let mut last_ts_ms: Option<i64> = None;
    let mut stopped = false;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event = match serde_json::from_str::<EventRecord>(&line) {
            Ok(event) => event,
            Err(err) if strict => return Err(io::Error::new(io::ErrorKind::InvalidData, err)),
            Err(_) => continue,
        };

        total_events = total_events.saturating_add(1);
        if first_ts_ms.is_none() {
            first_ts_ms = Some(event.ts_ms);
        }
        last_ts_ms = Some(event.ts_ms);

        if stopped {
            continue;
        }
        processed_events = processed_events.saturating_add(1);
        if matches!(handler.handle_event(&event), ReplayControl::Stop) {
            stopped = true;
        }
    }

    Ok(ReplaySummary {
        total_events,
        processed_events,
        first_ts_ms,
        last_ts_ms,
    })
}

fn run_stress_in_file_order<H: EventHandler>(
    path: &Path,
    config: &StressConfig,
    handler: &mut H,
) -> io::Result<StressSummary> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut total_events = 0usize;
    let mut processed_events = 0usize;
    let mut first_ts_ms: Option<i64> = None;
    let mut last_ts_ms: Option<i64> = None;
    let mut prev_ts_ms: Option<i64> = None;
    let mut wall_sleep_ms = 0u64;
    let mut stopped = false;
    let mut processing_ns = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event = match serde_json::from_str::<EventRecord>(&line) {
            Ok(event) => event,
            Err(err) if config.strict => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, err))
            }
            Err(_) => continue,
        };

        total_events = total_events.saturating_add(1);
        if first_ts_ms.is_none() {
            first_ts_ms = Some(event.ts_ms);
        }
        last_ts_ms = Some(event.ts_ms);

        if stopped {
            continue;
        }

        if let Some(prev) = prev_ts_ms {
            let delta = event.ts_ms.saturating_sub(prev);
            if delta > 0 {
                let scaled = ((delta as f64) / config.speed).round() as u64;
                if config.sleep && scaled > 0 {
                    std::thread::sleep(Duration::from_millis(scaled));
                }
                wall_sleep_ms = wall_sleep_ms.saturating_add(scaled);
            }
        }
        prev_ts_ms = Some(event.ts_ms);

        let start = Instant::now();
        let control = handler.handle_event(&event);
        let elapsed = start.elapsed();
        processing_ns.push(duration_to_ns(elapsed));
        processed_events = processed_events.saturating_add(1);
        if matches!(control, ReplayControl::Stop) {
            stopped = true;
        }
    }

    let simulated_duration_ms = match (first_ts_ms, last_ts_ms) {
        (Some(first), Some(last)) => Some(last.saturating_sub(first)),
        _ => None,
    };
    let (processing_min_ns, processing_mean_ns, processing_p95_ns) =
        processing_stats(&processing_ns);

    Ok(StressSummary {
        total_events,
        processed_events,
        simulated_duration_ms,
        wall_sleep_ms,
        processing_min_ns,
        processing_mean_ns,
        processing_p95_ns,
    })
}

fn duration_to_ns(duration: Duration) -> u64 {
    let nanos = duration.as_nanos();
    if nanos > u64::MAX as u128 {
        u64::MAX
    } else {
        nanos as u64
    }
}

fn processing_stats(values: &[u64]) -> (Option<u64>, Option<u64>, Option<u64>) {
    if values.is_empty() {
        return (None, None, None);
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let min = *sorted.first().unwrap_or(&0);
    let sum: u128 = sorted.iter().map(|value| *value as u128).sum();
    let mean = (sum / sorted.len() as u128) as u64;
    let rank = (sorted.len() * 95).div_ceil(100);
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    let p95 = sorted[idx];
    (Some(min), Some(mean), Some(p95))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
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
    fn replay_ordering_deterministic_by_seq() {
        let dir = temp_dir("replay_test");
        let path = dir.join("events.jsonl");
        let mut file = fs::File::create(&path).expect("create log file");

        let first = EventRecord {
            seq: 2,
            ts_ms: 200,
            event: "b".to_string(),
            payload: json!({"v": 2}),
        };
        let second = EventRecord {
            seq: 1,
            ts_ms: 100,
            event: "a".to_string(),
            payload: json!({"v": 1}),
        };

        writeln!(file, "{}", serde_json::to_string(&first).unwrap()).unwrap();
        writeln!(file, "{}", serde_json::to_string(&second).unwrap()).unwrap();

        struct Collect {
            seqs: Vec<u64>,
        }

        impl EventHandler for Collect {
            fn handle_event(&mut self, event: &EventRecord) -> ReplayControl {
                self.seqs.push(event.seq);
                ReplayControl::Continue
            }
        }

        let runner = ReplayRunner::new(ReplayConfig {
            ordering: ReplayOrdering::BySeq,
            strict: true,
        });
        let mut handler = Collect { seqs: Vec::new() };
        let summary = runner.run(&path, &mut handler).expect("run replay");

        assert_eq!(handler.seqs, vec![1, 2]);
        assert_eq!(summary.total_events, 2);
        assert_eq!(summary.processed_events, 2);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn replay_in_file_order_streaming() {
        let dir = temp_dir("replay_stream");
        let path = dir.join("events.jsonl");
        let mut file = fs::File::create(&path).expect("create log file");

        let first = EventRecord {
            seq: 2,
            ts_ms: 200,
            event: "b".to_string(),
            payload: json!({"v": 2}),
        };
        let second = EventRecord {
            seq: 1,
            ts_ms: 100,
            event: "a".to_string(),
            payload: json!({"v": 1}),
        };
        let third = EventRecord {
            seq: 3,
            ts_ms: 300,
            event: "c".to_string(),
            payload: json!({"v": 3}),
        };

        writeln!(file, "{}", serde_json::to_string(&first).unwrap()).unwrap();
        writeln!(file, "{}", serde_json::to_string(&second).unwrap()).unwrap();
        writeln!(file, "{}", serde_json::to_string(&third).unwrap()).unwrap();

        struct Collect {
            seqs: Vec<u64>,
        }

        impl EventHandler for Collect {
            fn handle_event(&mut self, event: &EventRecord) -> ReplayControl {
                self.seqs.push(event.seq);
                ReplayControl::Continue
            }
        }

        let runner = ReplayRunner::new(ReplayConfig {
            ordering: ReplayOrdering::InFileOrder,
            strict: true,
        });
        let mut handler = Collect { seqs: Vec::new() };
        let summary = runner.run(&path, &mut handler).expect("run replay");

        assert_eq!(handler.seqs, vec![2, 1, 3]);
        assert_eq!(summary.total_events, 3);
        assert_eq!(summary.processed_events, 3);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stress_processing_stats_present() {
        let dir = temp_dir("stress_stats");
        let path = dir.join("events.jsonl");
        let mut file = fs::File::create(&path).expect("create log file");

        for i in 0..3 {
            let event = EventRecord {
                seq: i + 1,
                ts_ms: 100 * (i as i64),
                event: "tick".to_string(),
                payload: json!({"v": i}),
            };
            writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        }

        struct Sleeper {
            count: u64,
        }

        impl EventHandler for Sleeper {
            fn handle_event(&mut self, _event: &EventRecord) -> ReplayControl {
                self.count += 1;
                std::thread::sleep(std::time::Duration::from_millis(self.count));
                ReplayControl::Continue
            }
        }

        let runner = StressRunner::new(StressConfig {
            ordering: ReplayOrdering::InFileOrder,
            speed: 1.0,
            sleep: false,
            strict: true,
        });
        let mut handler = Sleeper { count: 0 };
        let summary = runner.run(&path, &mut handler).expect("run stress");

        assert_eq!(summary.total_events, 3);
        assert_eq!(summary.processed_events, 3);
        assert!(summary.processing_min_ns.is_some());
        assert!(summary.processing_mean_ns.is_some());
        assert!(summary.processing_p95_ns.is_some());

        let min = summary.processing_min_ns.unwrap();
        let mean = summary.processing_mean_ns.unwrap();
        let p95 = summary.processing_p95_ns.unwrap();
        assert!(min > 0);
        assert!(mean >= min);
        assert!(p95 >= min);

        let _ = fs::remove_dir_all(dir);
    }
}
