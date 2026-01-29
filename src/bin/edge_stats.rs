#![allow(dead_code)]

#[path = "../persistence/mod.rs"]
mod persistence;

use persistence::EventRecord;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
struct DesiredSnapshot {
    ts_ms: i64,
    target_total: Option<f64>,
    level0_total: Option<f64>,
    level0_edge: Option<f64>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }

    let path = parse_path(&args).unwrap_or_else(|| {
        eprintln!("missing --path <events.jsonl>");
        print_usage();
        std::process::exit(1);
    });

    if let Err(err) = run(path) {
        eprintln!("edge_stats failed: {err}");
        std::process::exit(2);
    }
}

fn print_usage() {
    eprintln!(
        "Usage:\n  cargo run --bin edge_stats -- --path ./logs/events.jsonl\n\nThis analyzes event-log JSONL offline and prints paired-fill stats + edge-at-fill proxy."
    );
}

fn parse_path(args: &[String]) -> Option<PathBuf> {
    let mut i = 0usize;
    while i < args.len() {
        if args[i] == "--path" {
            if let Some(v) = args.get(i + 1) {
                return Some(PathBuf::from(v));
            }
        }
        i += 1;
    }
    None
}

fn run(path: PathBuf) -> io::Result<()> {
    let file = File::open(&path)?;
    let reader = BufReader::new(file);

    let mut token_to_slug: HashMap<String, String> = HashMap::new();
    let mut slug_tokens: HashMap<String, HashSet<String>> = HashMap::new();
    let mut last_desired: HashMap<String, DesiredSnapshot> = HashMap::new();

    let mut fill_shares_by_slug_token: HashMap<(String, String), f64> = HashMap::new();
    let mut fill_count_by_slug_token: HashMap<(String, String), u64> = HashMap::new();

    let mut total_fills = 0u64;
    let mut total_fill_shares = 0f64;

    let mut edge_sum = 0f64;
    let mut edge_n = 0u64;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: EventRecord = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match record.event.as_str() {
            "strategy.desired" => {
                if let Some(slug) = record.payload.get("slug").and_then(Value::as_str) {
                    let slug = slug.to_string();

                    let snap = DesiredSnapshot {
                        ts_ms: record.ts_ms,
                        target_total: record.payload.get("target_total").and_then(Value::as_f64),
                        level0_total: record.payload.get("level0_total").and_then(Value::as_f64),
                        level0_edge: record.payload.get("level0_edge").and_then(Value::as_f64),
                    };
                    last_desired.insert(slug.clone(), snap);

                    if let Some(orders) = record.payload.get("orders").and_then(Value::as_array) {
                        for order in orders {
                            let Some(token_id) = order.get("token_id").and_then(Value::as_str)
                            else {
                                continue;
                            };
                            token_to_slug.insert(token_id.to_string(), slug.clone());
                            slug_tokens
                                .entry(slug.clone())
                                .or_default()
                                .insert(token_id.to_string());
                        }
                    }
                }
            }
            "ws.user.fill" => {
                let Some(token_id) = record.payload.get("token_id").and_then(Value::as_str) else {
                    continue;
                };
                let shares = record
                    .payload
                    .get("shares")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                total_fills = total_fills.saturating_add(1);
                total_fill_shares += shares;

                let slug = token_to_slug
                    .get(token_id)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());
                let key = (slug.clone(), token_id.to_string());
                *fill_shares_by_slug_token.entry(key.clone()).or_insert(0.0) += shares;
                *fill_count_by_slug_token.entry(key).or_insert(0) += 1;

                if let Some(snap) = last_desired.get(&slug) {
                    if let Some(edge) = snap.level0_edge {
                        edge_sum += edge;
                        edge_n = edge_n.saturating_add(1);
                    }
                }
            }
            _ => {}
        }
    }

    println!("events_path={}", path.display());
    println!(
        "fills_total={} fill_shares_total={:.4}",
        total_fills, total_fill_shares
    );
    if edge_n > 0 {
        println!(
            "edge_at_fill_mean={:.6} (n={})",
            edge_sum / (edge_n as f64),
            edge_n
        );
    } else {
        println!("edge_at_fill_mean=NA (no strategy.desired snapshots preceding fills)");
    }

    // Per-slug pairing stats.
    let mut slugs: Vec<_> = slug_tokens.keys().cloned().collect();
    slugs.sort();
    for slug in slugs {
        let tokens: Vec<String> = slug_tokens
            .get(&slug)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        let mut tokens = tokens;
        tokens.sort();

        let mut shares: Vec<(String, f64, u64)> = tokens
            .iter()
            .map(|t| {
                let key = (slug.clone(), t.clone());
                let s = *fill_shares_by_slug_token.get(&key).unwrap_or(&0.0);
                let c = *fill_count_by_slug_token.get(&key).unwrap_or(&0);
                (t.clone(), s, c)
            })
            .collect();

        // If we didn't observe both tokens yet, still print what we have.
        shares.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let total: f64 = shares.iter().map(|x| x.1).sum();
        let min = shares
            .iter()
            .map(|x| x.1)
            .fold(f64::INFINITY, |a, b| a.min(b));
        let max = shares.iter().map(|x| x.1).fold(0.0f64, |a, b| a.max(b));
        let pair_ratio = if max > 0.0 { min / max } else { 1.0 };

        println!("slug={slug} fill_shares_total={total:.4} pair_ratio={pair_ratio:.3}");
        for (token, s, c) in shares {
            println!("  token_id={token} fills={c} shares={s:.4}");
        }
    }

    Ok(())
}
