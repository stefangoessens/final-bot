use std::collections::{HashMap, HashSet};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::persistence::LogEvent;
use crate::state::state_manager::{AppEvent, MarketWsUpdate};

const MAX_RAW_LOG_BYTES: usize = 8 * 1024;
const REDACT_PLACEHOLDER: &str = "[REDACTED]";

#[derive(Debug, Clone)]
pub enum MarketWsCommand {
    Subscribe(Vec<String>),
    Unsubscribe(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct MarketWsLoop {
    ws_url: String,
    custom_feature_enabled: bool,
}

impl MarketWsLoop {
    pub fn new(ws_url: String) -> Self {
        Self {
            ws_url,
            custom_feature_enabled: true,
        }
    }

    pub async fn run(
        self,
        mut rx_cmd: Receiver<MarketWsCommand>,
        tx_events: Sender<AppEvent>,
        log_tx: Option<Sender<LogEvent>>,
    ) {
        let mut subscribed: HashSet<String> = HashSet::new();
        let mut last_best: HashMap<String, BestQuote> = HashMap::new();
        let mut backoff = Backoff::new(250, 10_000);

        loop {
            tracing::info!(
                target: "clob_ws_market",
                url = %self.ws_url,
                "market ws connecting"
            );

            match connect_async(&self.ws_url).await {
                Ok((ws, _)) => {
                    tracing::info!(target: "clob_ws_market", "market ws connected");
                    backoff.reset();
                    let (mut write, mut read) = ws.split();

                    if !subscribed.is_empty() {
                        if let Err(err) = send_initial_subscribe(
                            &mut write,
                            &subscribed,
                            self.custom_feature_enabled,
                        )
                        .await
                        {
                            tracing::warn!(
                                target: "clob_ws_market",
                                error = %err,
                                "market ws failed to send initial subscribe"
                            );
                        }
                    }

                    loop {
                        tokio::select! {
                            cmd = rx_cmd.recv() => {
                                match cmd {
                                    Some(cmd) => {
                                        if let Err(err) = handle_command(
                                            cmd,
                                            &mut subscribed,
                                            &mut write,
                                            self.custom_feature_enabled,
                                        ).await {
                                            tracing::warn!(
                                                target: "clob_ws_market",
                                                error = %err,
                                                "market ws command send failed"
                                            );
                                            break;
                                        }
                                    }
                                    None => {
                                        tracing::info!(target: "clob_ws_market", "command channel closed");
                                        return;
                                    }
                                }
                            }
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Text(text))) => {
                                        let now_ms = now_ms();
                                        log_raw_frame(&log_tx, "ws.market.raw", now_ms, &text);
                                        match parse_updates(&text, &mut last_best, now_ms) {
                                            Ok(updates) => {
                                                for update in updates {
                                                    log_market_update(&log_tx, &update);
                                                    if tx_events.send(AppEvent::MarketWsUpdate(update)).await.is_err() {
                                                        return;
                                                    }
                                                }
                                            }
                                            Err(err) => {
                                                tracing::warn!(
                                                    target: "clob_ws_market",
                                                    error = %err,
                                                    "market ws parse error"
                                                );
                                            }
                                        }
                                    }
                                    Some(Ok(Message::Ping(payload))) => {
                                        if let Err(err) = write.send(Message::Pong(payload)).await {
                                            tracing::warn!(
                                                target: "clob_ws_market",
                                                error = %err,
                                                "market ws pong send failed"
                                            );
                                            break;
                                        }
                                    }
                                    Some(Ok(Message::Close(frame))) => {
                                        tracing::warn!(
                                            target: "clob_ws_market",
                                            ?frame,
                                            "market ws closed"
                                        );
                                        break;
                                    }
                                    Some(Ok(_)) => {}
                                    Some(Err(err)) => {
                                        tracing::warn!(
                                            target: "clob_ws_market",
                                            error = %err,
                                            "market ws read error"
                                        );
                                        break;
                                    }
                                    None => {
                                        tracing::warn!(target: "clob_ws_market", "market ws stream ended");
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        target: "clob_ws_market",
                        error = %err,
                        "market ws connect failed"
                    );
                }
            }

            let delay_ms = backoff.next_delay();
            tracing::info!(
                target: "clob_ws_market",
                backoff_ms = delay_ms,
                "market ws reconnecting with backoff"
            );

            let sleep = tokio::time::sleep(Duration::from_millis(delay_ms));
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    cmd = rx_cmd.recv() => {
                        match cmd {
                            Some(cmd) => apply_command_to_set(cmd, &mut subscribed),
                            None => return,
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct BestQuote {
    best_bid: Option<f64>,
    best_ask: Option<f64>,
}

fn apply_command_to_set(cmd: MarketWsCommand, subscribed: &mut HashSet<String>) {
    match cmd {
        MarketWsCommand::Subscribe(ids) => {
            for id in ids {
                subscribed.insert(id);
            }
        }
        MarketWsCommand::Unsubscribe(ids) => {
            for id in ids {
                subscribed.remove(&id);
            }
        }
    }
}

async fn handle_command<W>(
    cmd: MarketWsCommand,
    subscribed: &mut HashSet<String>,
    write: &mut W,
    custom_feature_enabled: bool,
) -> Result<(), String>
where
    W: futures_util::Sink<Message> + Unpin,
    W::Error: std::fmt::Display,
{
    match cmd {
        MarketWsCommand::Subscribe(ids) => {
            let mut new_ids = Vec::new();
            for id in ids {
                if subscribed.insert(id.clone()) {
                    new_ids.push(id);
                }
            }
            if !new_ids.is_empty() {
                send_operation(write, "subscribe", new_ids, custom_feature_enabled).await?;
            }
        }
        MarketWsCommand::Unsubscribe(ids) => {
            let mut remove_ids = Vec::new();
            for id in ids {
                if subscribed.remove(&id) {
                    remove_ids.push(id);
                }
            }
            if !remove_ids.is_empty() {
                send_operation(write, "unsubscribe", remove_ids, custom_feature_enabled).await?;
            }
        }
    }
    Ok(())
}

async fn send_initial_subscribe<W>(
    write: &mut W,
    assets: &HashSet<String>,
    custom_feature_enabled: bool,
) -> Result<(), String>
where
    W: futures_util::Sink<Message> + Unpin,
    W::Error: std::fmt::Display,
{
    if assets.is_empty() {
        return Ok(());
    }
    let assets_ids: Vec<String> = assets.iter().cloned().collect();
    let payload = serde_json::json!({
        // Data Feeds docs show type: "market" while WSS overview says "MARKET".
        // Use "market" per the Data Feeds example.
        "type": "market",
        "assets_ids": assets_ids,
        "custom_feature_enabled": custom_feature_enabled,
    });
    write
        .send(Message::Text(payload.to_string().into()))
        .await
        .map_err(|e| format!("initial subscribe send failed: {e}"))?;
    Ok(())
}

async fn send_operation<W>(
    write: &mut W,
    operation: &str,
    assets_ids: Vec<String>,
    custom_feature_enabled: bool,
) -> Result<(), String>
where
    W: futures_util::Sink<Message> + Unpin,
    W::Error: std::fmt::Display,
{
    let payload = serde_json::json!({
        "operation": operation,
        "assets_ids": assets_ids,
        "custom_feature_enabled": custom_feature_enabled,
    });
    write
        .send(Message::Text(payload.to_string().into()))
        .await
        .map_err(|e| format!("operation send failed: {e}"))?;
    Ok(())
}

fn parse_updates(
    text: &str,
    last_best: &mut HashMap<String, BestQuote>,
    now_ms: i64,
) -> Result<Vec<MarketWsUpdate>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("invalid json: {e}"))?;
    let event_type = match value.get("event_type").and_then(|v| v.as_str()) {
        Some(event) => event,
        None => return Ok(Vec::new()),
    };

    match event_type {
        "best_bid_ask" => parse_best_bid_ask(&value, last_best, now_ms),
        "tick_size_change" => parse_tick_size_change(&value, last_best, now_ms),
        "price_change" => parse_price_change(&value, last_best, now_ms),
        _ => Ok(Vec::new()),
    }
}

fn parse_best_bid_ask(
    value: &Value,
    last_best: &mut HashMap<String, BestQuote>,
    now_ms: i64,
) -> Result<Vec<MarketWsUpdate>, String> {
    let asset_id = match value.get("asset_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return Ok(Vec::new()),
    };
    let best_bid = value.get("best_bid").and_then(parse_f64);
    let best_ask = value.get("best_ask").and_then(parse_f64);
    let ts_ms = value.get("timestamp").and_then(parse_i64).unwrap_or(now_ms);

    let entry = last_best.entry(asset_id.clone()).or_default();
    if best_bid.is_some() {
        entry.best_bid = best_bid;
    }
    if best_ask.is_some() {
        entry.best_ask = best_ask;
    }

    if entry.best_bid.is_none() && entry.best_ask.is_none() {
        return Ok(Vec::new());
    }

    Ok(vec![MarketWsUpdate {
        token_id: asset_id,
        best_bid: entry.best_bid,
        best_ask: entry.best_ask,
        tick_size: None,
        ts_ms,
    }])
}

fn parse_tick_size_change(
    value: &Value,
    last_best: &mut HashMap<String, BestQuote>,
    now_ms: i64,
) -> Result<Vec<MarketWsUpdate>, String> {
    let asset_id = match value.get("asset_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return Ok(Vec::new()),
    };
    let tick_size = value.get("new_tick_size").and_then(parse_f64);
    let ts_ms = value.get("timestamp").and_then(parse_i64).unwrap_or(now_ms);
    let best = last_best.get(&asset_id).copied().unwrap_or_default();

    Ok(vec![MarketWsUpdate {
        token_id: asset_id,
        best_bid: best.best_bid,
        best_ask: best.best_ask,
        tick_size,
        ts_ms,
    }])
}

fn parse_price_change(
    value: &Value,
    last_best: &mut HashMap<String, BestQuote>,
    now_ms: i64,
) -> Result<Vec<MarketWsUpdate>, String> {
    let ts_ms = value.get("timestamp").and_then(parse_i64).unwrap_or(now_ms);
    let changes = match value.get("price_changes").and_then(|v| v.as_array()) {
        Some(changes) => changes,
        None => return Ok(Vec::new()),
    };

    let mut updates = Vec::new();
    for change in changes {
        let asset_id = match change.get("asset_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        let best_bid = change.get("best_bid").and_then(parse_f64);
        let best_ask = change.get("best_ask").and_then(parse_f64);
        if best_bid.is_none() && best_ask.is_none() {
            continue;
        }

        let entry = last_best.entry(asset_id.clone()).or_default();
        if best_bid.is_some() {
            entry.best_bid = best_bid;
        }
        if best_ask.is_some() {
            entry.best_ask = best_ask;
        }

        updates.push(MarketWsUpdate {
            token_id: asset_id,
            best_bid: entry.best_bid,
            best_ask: entry.best_ask,
            tick_size: None,
            ts_ms,
        });
    }

    Ok(updates)
}

fn parse_f64(value: &Value) -> Option<f64> {
    match value {
        Value::String(s) => s.parse::<f64>().ok(),
        Value::Number(num) => num.as_f64(),
        _ => None,
    }
}

fn parse_i64(value: &Value) -> Option<i64> {
    match value {
        Value::String(s) => s.parse::<i64>().ok(),
        Value::Number(num) => num.as_i64().or_else(|| num.as_f64().map(|v| v as i64)),
        _ => None,
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn log_raw_frame(
    log_tx: &Option<Sender<LogEvent>>,
    event: &str,
    ts_ms: i64,
    text: &str,
) {
    let Some(tx) = log_tx else {
        return;
    };
    let redacted = redact_frame(text);
    let payload = json!({
        "frame": truncate_utf8(&redacted, MAX_RAW_LOG_BYTES),
    });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: event.to_string(),
        payload,
    });
}

fn log_market_update(log_tx: &Option<Sender<LogEvent>>, update: &MarketWsUpdate) {
    let Some(tx) = log_tx else {
        return;
    };
    let payload = json!({
        "token_id": update.token_id,
        "best_bid": update.best_bid,
        "best_ask": update.best_ask,
        "tick_size": update.tick_size,
    });
    let _ = tx.try_send(LogEvent {
        ts_ms: update.ts_ms,
        event: "ws.market.update".to_string(),
        payload,
    });
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn redact_frame(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(mut value) => {
            redact_json_value(&mut value);
            value.to_string()
        }
        Err(_) => redact_non_json(text),
    }
}

fn redact_json_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *val = Value::String(REDACT_PLACEHOLDER.to_string());
                } else {
                    redact_json_value(val);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_json_value(item);
            }
        }
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    matches!(
        normalized.as_str(),
        "secret"
            | "passphrase"
            | "apikey"
            | "authorization"
            | "auth"
            | "signature"
            | "sig"
            | "token"
            | "owner"
            | "wallet"
            | "address"
            | "from"
            | "to"
            | "funder"
    )
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn redact_non_json(text: &str) -> String {
    let mut bytes = text.as_bytes().to_vec();
    for key in ["apikey", "api_key", "secret", "passphrase"] {
        bytes = redact_bytes(&bytes, key.as_bytes());
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn redact_bytes(input: &[u8], key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if match_key_at(input, i, key) {
            let before = i.checked_sub(1).and_then(|idx| input.get(idx).copied());
            let after = input.get(i + key.len()).copied();
            if is_word_char(before) || is_word_char(after) {
                out.push(input[i]);
                i += 1;
                continue;
            }
            out.extend_from_slice(&input[i..i + key.len()]);
            let mut j = i + key.len();
            while j < input.len() && input[j].is_ascii_whitespace() {
                out.push(input[j]);
                j += 1;
            }
            if j < input.len() && (input[j] == b':' || input[j] == b'=') {
                out.push(input[j]);
                j += 1;
                while j < input.len() && input[j].is_ascii_whitespace() {
                    out.push(input[j]);
                    j += 1;
                }
                if j < input.len() && (input[j] == b'"' || input[j] == b'\'') {
                    let quote = input[j];
                    out.push(quote);
                    j += 1;
                    out.extend_from_slice(REDACT_PLACEHOLDER.as_bytes());
                    while j < input.len() && input[j] != quote {
                        j += 1;
                    }
                    if j < input.len() {
                        out.push(quote);
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                out.extend_from_slice(REDACT_PLACEHOLDER.as_bytes());
                while j < input.len() {
                    let c = input[j];
                    if matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b',' | b'&') {
                        break;
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
            i += key.len();
            continue;
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn match_key_at(input: &[u8], pos: usize, key: &[u8]) -> bool {
    if pos + key.len() > input.len() {
        return false;
    }
    for (offset, key_byte) in key.iter().enumerate() {
        if !input[pos + offset].eq_ignore_ascii_case(key_byte) {
            return false;
        }
    }
    true
}

fn is_word_char(byte: Option<u8>) -> bool {
    match byte {
        Some(b) => b.is_ascii_alphanumeric() || b == b'_',
        None => false,
    }
}

#[derive(Debug)]
struct Backoff {
    base_ms: u64,
    max_ms: u64,
    current_ms: u64,
}

impl Backoff {
    fn new(base_ms: u64, max_ms: u64) -> Self {
        Self {
            base_ms,
            max_ms,
            current_ms: base_ms,
        }
    }

    fn reset(&mut self) {
        self.current_ms = self.base_ms;
    }

    fn next_delay(&mut self) -> u64 {
        let delay = self.current_ms;
        self.current_ms = (self.current_ms * 2).min(self.max_ms);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_best_bid_ask_message() {
        let sample = r#"{
          "event_type": "best_bid_ask",
          "market": "0x0005c0d312de0be897668695bae9f32b624b4a1ae8b140c49f08447fcc74f442",
          "asset_id": "85354956062430465315924116860125388538595433819574542752031640332592237464430",
          "best_bid": "0.73",
          "best_ask": "0.77",
          "spread": "0.04",
          "timestamp": "1766789469958"
        }"#;

        let mut last_best = HashMap::new();
        let updates = parse_updates(sample, &mut last_best, 0).expect("parse should succeed");
        assert_eq!(updates.len(), 1);
        let update = &updates[0];
        assert_eq!(
            update.token_id,
            "85354956062430465315924116860125388538595433819574542752031640332592237464430"
        );
        assert_eq!(update.best_bid, Some(0.73));
        assert_eq!(update.best_ask, Some(0.77));
        assert_eq!(update.tick_size, None);
        assert_eq!(update.ts_ms, 1_766_789_469_958);
    }
}
