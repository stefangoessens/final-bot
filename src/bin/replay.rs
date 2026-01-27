#![allow(dead_code)]

#[path = "../persistence/mod.rs"]
mod persistence;

use persistence::{
    EventHandler, ReplayConfig, ReplayControl, ReplayOrdering, ReplayRunner, StressConfig,
    StressRunner,
};

use std::path::PathBuf;

struct NoopHandler;

impl EventHandler for NoopHandler {
    fn handle_event(&mut self, _event: &persistence::EventRecord) -> ReplayControl {
        ReplayControl::Continue
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }

    let mode = args.remove(0);
    match mode.as_str() {
        "replay" => {
            let (path, ordering, strict) = parse_common_args(&args);
            let runner = ReplayRunner::new(ReplayConfig { ordering, strict });
            let mut handler = NoopHandler;
            match runner.run(&path, &mut handler) {
                Ok(summary) => {
                    println!(
                        "replay: total={} processed={} first_ts_ms={:?} last_ts_ms={:?}",
                        summary.total_events,
                        summary.processed_events,
                        summary.first_ts_ms,
                        summary.last_ts_ms
                    );
                }
                Err(err) => {
                    eprintln!("replay failed: {err}");
                    std::process::exit(2);
                }
            }
        }
        "stress" => {
            let (path, ordering, strict) = parse_common_args(&args);
            let mut speed = 1.0f64;
            let mut sleep = false;

            let mut i = 0usize;
            while i < args.len() {
                match args[i].as_str() {
                    "--speed" => {
                        if let Some(v) = args.get(i + 1) {
                            speed = v.parse::<f64>().unwrap_or(1.0);
                            i += 1;
                        }
                    }
                    "--sleep" => {
                        sleep = true;
                    }
                    _ => {}
                }
                i += 1;
            }

            let runner = StressRunner::new(StressConfig {
                ordering,
                speed,
                sleep,
                strict,
            });
            let mut handler = NoopHandler;
            match runner.run(&path, &mut handler) {
                Ok(summary) => {
                    println!(
                        "stress: total={} processed={} simulated_ms={:?} wall_sleep_ms={} min_ns={:?} mean_ns={:?} p95_ns={:?}",
                        summary.total_events,
                        summary.processed_events,
                        summary.simulated_duration_ms,
                        summary.wall_sleep_ms,
                        summary.processing_min_ns,
                        summary.processing_mean_ns,
                        summary.processing_p95_ns
                    );
                }
                Err(err) => {
                    eprintln!("stress failed: {err}");
                    std::process::exit(2);
                }
            }
        }
        _ => {
            eprintln!("unknown mode '{mode}'");
            print_usage();
            std::process::exit(1);
        }
    }
}

fn parse_common_args(args: &[String]) -> (PathBuf, ReplayOrdering, bool) {
    let mut path = None::<PathBuf>;
    let mut ordering = ReplayOrdering::BySeq;
    let mut strict = true;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => {
                if let Some(v) = args.get(i + 1) {
                    path = Some(PathBuf::from(v));
                    i += 1;
                }
            }
            "--ordering" => {
                if let Some(v) = args.get(i + 1) {
                    ordering = parse_ordering(v);
                    i += 1;
                }
            }
            "--strict" => {
                if let Some(v) = args.get(i + 1) {
                    strict = parse_bool(v, true);
                    i += 1;
                }
            }
            _ => {
                if path.is_none() {
                    path = Some(PathBuf::from(&args[i]));
                }
            }
        }
        i += 1;
    }

    let path = path.unwrap_or_else(|| PathBuf::from("./logs/events.jsonl"));
    (path, ordering, strict)
}

fn parse_ordering(value: &str) -> ReplayOrdering {
    match value.to_ascii_lowercase().as_str() {
        "file" | "infile" | "in_file" => ReplayOrdering::InFileOrder,
        "ts" | "timestamp" => ReplayOrdering::ByTimestamp,
        _ => ReplayOrdering::BySeq,
    }
}

fn parse_bool(value: &str, default: bool) -> bool {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => true,
        "0" | "false" | "no" => false,
        _ => default,
    }
}

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("  replay <path> [--ordering seq|ts|file] [--strict true|false]");
    eprintln!("  stress <path> [--ordering seq|ts|file] [--strict true|false] [--speed N] [--sleep]");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  cargo run --bin replay -- replay ./logs/events.jsonl");
    eprintln!("  cargo run --bin replay -- stress ./logs/events.jsonl --speed 5");
}
