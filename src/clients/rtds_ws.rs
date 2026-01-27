use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::error::{BotError, BotResult};
use crate::persistence::LogEvent;
use crate::state::state_manager::{AppEvent, RTDSSource, RTDSUpdate};

const DEFAULT_RTDS_URL: &str = "wss://ws-live-data.polymarket.com";
const TOPIC_BINANCE: &str = "crypto_prices";
const TOPIC_CHAINLINK: &str = "crypto_prices_chainlink";
const BINANCE_SYMBOL: &str = "btcusdt";
const CHAINLINK_SYMBOL: &str = "btc/usd";
const MAX_RAW_LOG_BYTES: usize = 8 * 1024;
const REDACT_PLACEHOLDER: &str = "[REDACTED]";
const CHAINLINK_WATCHDOG_POLL_MS: u64 = 250;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Debug, Clone)]
pub struct RTDSLoop {
    url: String,
    backoff_min: Duration,
    backoff_max: Duration,
    chainlink_stale_ms: i64,
}

impl RTDSLoop {
    pub fn new(url: String, chainlink_stale_ms: i64) -> Self {
        let url = if url.trim().is_empty() {
            DEFAULT_RTDS_URL.to_string()
        } else {
            url
        };
        Self {
            url,
            backoff_min: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            chainlink_stale_ms,
        }
    }

    pub async fn run(
        self,
        tx_events: Sender<AppEvent>,
        log_tx: Option<Sender<LogEvent>>,
    ) -> BotResult<()> {
        let mut backoff = self.backoff_min;
        loop {
            tracing::info!(target: "rtds_ws", url = %self.url, "connecting");

            match self.connect_and_run(&tx_events, log_tx.clone()).await {
                Ok(()) => {
                    tracing::warn!(target: "rtds_ws", "disconnected");
                    backoff = self.backoff_min;
                }
                Err(err) => {
                    tracing::warn!(target: "rtds_ws", error = %err, "connection error");
                }
            }

            if tx_events.is_closed() {
                tracing::info!(target: "rtds_ws", "event channel closed; stopping");
                return Ok(());
            }

            tracing::info!(
                target: "rtds_ws",
                backoff_ms = backoff.as_millis(),
                "reconnecting after backoff"
            );
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(self.backoff_max);
        }
    }

    async fn connect_and_run(
        &self,
        tx_events: &Sender<AppEvent>,
        log_tx: Option<Sender<LogEvent>>,
    ) -> BotResult<()> {
        let (mut ws, _) = connect_async(&self.url)
            .await
            .map_err(|e| BotError::Other(format!("rtds connect failed: {e}")))?;

        tracing::info!(target: "rtds_ws", url = %self.url, "connected");

        let subscribe_msg = build_subscribe_message();
        ws.send(Message::Text(subscribe_msg.into()))
            .await
            .map_err(|e| BotError::Other(format!("rtds subscribe failed: {e}")))?;

        tracing::info!(
            target: "rtds_ws",
            binance_topic = TOPIC_BINANCE,
            binance_symbol = BINANCE_SYMBOL,
            chainlink_topic = TOPIC_CHAINLINK,
            chainlink_symbol = CHAINLINK_SYMBOL,
            "subscribed to RTDS topics (binance + chainlink)"
        );

        read_loop(ws, tx_events, log_tx, self.chainlink_stale_ms).await
    }
}

async fn read_loop(
    mut ws: WsStream,
    tx_events: &Sender<AppEvent>,
    log_tx: Option<Sender<LogEvent>>,
    chainlink_stale_ms: i64,
) -> BotResult<()> {
    let connected_ms = now_ms();
    let mut last_chainlink_ms: Option<i64> = None;
    let mut chainlink_stale_logged = false;
    let mut watchdog = tokio::time::interval(Duration::from_millis(CHAINLINK_WATCHDOG_POLL_MS));
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            msg = ws.next() => {
                let Some(msg) = msg else { break };
                match msg {
                    Ok(Message::Text(text)) => {
                        let ts_ms = now_ms();
                        log_raw_frame(&log_tx, "ws.rtds.raw", ts_ms, &text);
                        if let Some(update) = parse_rtds_update(&text) {
                            if update.source == RTDSSource::ChainlinkBtcUsd {
                                last_chainlink_ms = Some(update.ts_ms);
                                let age_ms = ts_ms.saturating_sub(update.ts_ms);
                                if chainlink_stale_logged && age_ms <= chainlink_stale_ms {
                                    tracing::info!(
                                        target: "rtds_ws",
                                        age_ms,
                                        "Chainlink BTC/USD feed recovered"
                                    );
                                    chainlink_stale_logged = false;
                                }
                            }
                            log_rtds_update(&log_tx, &update);
                            if tx_events.send(AppEvent::RTDSUpdate(update)).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Ok(Message::Binary(bin)) => match String::from_utf8(bin.to_vec()) {
                        Ok(text) => {
                            let ts_ms = now_ms();
                            log_raw_frame(&log_tx, "ws.rtds.raw", ts_ms, &text);
                            if let Some(update) = parse_rtds_update(&text) {
                                if update.source == RTDSSource::ChainlinkBtcUsd {
                                    last_chainlink_ms = Some(update.ts_ms);
                                    let age_ms = ts_ms.saturating_sub(update.ts_ms);
                                    if chainlink_stale_logged && age_ms <= chainlink_stale_ms {
                                        tracing::info!(
                                            target: "rtds_ws",
                                            age_ms,
                                            "Chainlink BTC/USD feed recovered"
                                        );
                                        chainlink_stale_logged = false;
                                    }
                                }
                                log_rtds_update(&log_tx, &update);
                                if tx_events.send(AppEvent::RTDSUpdate(update)).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                        Err(err) => {
                            tracing::debug!(
                                target: "rtds_ws",
                                error = %err,
                                "non-utf8 binary frame"
                            );
                        }
                    },
                    Ok(Message::Ping(payload)) => {
                        let _ = ws.send(Message::Pong(payload)).await;
                    }
                    Ok(Message::Pong(_)) => {}
                    Ok(Message::Close(frame)) => {
                        tracing::warn!(target: "rtds_ws", ?frame, "server closed");
                        return Ok(());
                    }
                    Err(err) => {
                        return Err(BotError::Other(format!("rtds websocket error: {err}")));
                    }
                    _ => {}
                }
            }
            _ = watchdog.tick() => {
                let now = now_ms();
                let anchor_ms = last_chainlink_ms.unwrap_or(connected_ms);
                let age_ms = now.saturating_sub(anchor_ms);
                if age_ms > chainlink_stale_ms && !chainlink_stale_logged {
                    tracing::error!(
                        target: "rtds_ws",
                        age_ms,
                        threshold_ms = chainlink_stale_ms,
                        "no Chainlink BTC/USD updates within threshold; quoting remains paused"
                    );
                    chainlink_stale_logged = true;
                }
            }
        }
    }
    Ok(())
}

fn build_subscribe_message() -> String {
    serde_json::json!({
        "action": "subscribe",
        "subscriptions": [
            {
                "topic": TOPIC_BINANCE,
                "type": "update",
                "filters": BINANCE_SYMBOL
            },
            {
                "topic": TOPIC_CHAINLINK,
                "type": "*",
                "filters": { "symbol": CHAINLINK_SYMBOL }
            }
        ]
    })
    .to_string()
}

fn parse_rtds_update(text: &str) -> Option<RTDSUpdate> {
    let value: Value = serde_json::from_str(text).ok()?;
    let topic = value.get("topic")?.as_str()?.to_ascii_lowercase();
    let payload = value.get("payload")?;
    let symbol = payload.get("symbol")?.as_str()?.to_ascii_lowercase();
    let price = parse_f64(payload.get("value")?)?;
    let ts_ms = payload
        .get("timestamp")
        .and_then(parse_i64)
        .or_else(|| value.get("timestamp").and_then(parse_i64))
        .unwrap_or_else(now_ms);

    let source = match (topic.as_str(), symbol.as_str()) {
        (TOPIC_BINANCE, BINANCE_SYMBOL) => RTDSSource::BinanceBtcUsdt,
        (TOPIC_CHAINLINK, CHAINLINK_SYMBOL) => RTDSSource::ChainlinkBtcUsd,
        _ => return None,
    };

    Some(RTDSUpdate {
        source,
        price,
        ts_ms,
    })
}

fn parse_i64(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    if let Some(n) = value.as_u64() {
        return i64::try_from(n).ok();
    }
    if let Some(n) = value.as_f64() {
        return Some(n as i64);
    }
    if let Some(s) = value.as_str() {
        return s.parse::<i64>().ok();
    }
    None
}

fn parse_f64(value: &Value) -> Option<f64> {
    if let Some(n) = value.as_f64() {
        return Some(n);
    }
    if let Some(n) = value.as_i64() {
        return Some(n as f64);
    }
    if let Some(n) = value.as_u64() {
        return Some(n as f64);
    }
    if let Some(s) = value.as_str() {
        return s.parse::<f64>().ok();
    }
    None
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

fn log_rtds_update(log_tx: &Option<Sender<LogEvent>>, update: &RTDSUpdate) {
    let Some(tx) = log_tx else {
        return;
    };
    let source = match update.source {
        RTDSSource::BinanceBtcUsdt => "binance_btcusdt",
        RTDSSource::ChainlinkBtcUsd => "chainlink_btcusd",
    };
    let payload = json!({
        "source": source,
        "price": update.price,
    });
    let _ = tx.try_send(LogEvent {
        ts_ms: update.ts_ms,
        event: "ws.rtds.update".to_string(),
        payload,
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binance_btcusdt_update() {
        let msg = r#"{"topic":"crypto_prices","type":"update","timestamp":1753314088421,"payload":{"symbol":"btcusdt","timestamp":1753314088395,"value":67234.5}}"#;
        let update = parse_rtds_update(msg).expect("parsed update");
        assert_eq!(update.source, RTDSSource::BinanceBtcUsdt);
        assert!((update.price - 67234.5).abs() < 1e-9);
        assert_eq!(update.ts_ms, 1753314088395);
    }

    #[test]
    fn subscribe_message_uses_chainlink_filter_object() {
        let msg = build_subscribe_message();
        let value: Value = serde_json::from_str(&msg).expect("valid json");
        let subs = value
            .get("subscriptions")
            .and_then(|v| v.as_array())
            .expect("subscriptions array");

        let mut saw_chainlink = false;
        let mut saw_binance = false;
        for sub in subs {
            let topic = sub.get("topic").and_then(|v| v.as_str()).unwrap_or_default();
            match topic {
                TOPIC_CHAINLINK => {
                    saw_chainlink = true;
                    let filters = sub.get("filters").expect("chainlink filters");
                    assert!(filters.is_object(), "chainlink filters must be object");
                    let symbol = filters
                        .get("symbol")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert_eq!(symbol, CHAINLINK_SYMBOL);
                }
                TOPIC_BINANCE => {
                    saw_binance = true;
                    let filters = sub.get("filters").and_then(|v| v.as_str());
                    assert_eq!(filters, Some(BINANCE_SYMBOL));
                }
                _ => {}
            }
        }

        assert!(saw_chainlink, "missing chainlink subscription");
        assert!(saw_binance, "missing binance subscription");
    }
}
