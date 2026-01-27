use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;
use tokio::time::Instant;
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
const PING_INTERVAL_MS: u64 = 10_000;

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
    ws: WsStream,
    tx_events: &Sender<AppEvent>,
    log_tx: Option<Sender<LogEvent>>,
    chainlink_stale_ms: i64,
) -> BotResult<()> {
    let (mut write, mut read) = ws.split();
    let connected_ms = now_ms();
    let mut last_chainlink_ms: Option<i64> = None;
    let mut chainlink_stale_logged = false;
    let mut watchdog = tokio::time::interval(Duration::from_millis(CHAINLINK_WATCHDOG_POLL_MS));
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut next_ping = Instant::now() + Duration::from_millis(PING_INTERVAL_MS);

    loop {
        tokio::select! {
            msg = read.next() => {
                let Some(msg) = msg else { break };
                match msg {
                    Ok(Message::Text(text)) => {
                        if text.trim().eq_ignore_ascii_case("pong") {
                            continue;
                        }
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
                        let _ = write.send(Message::Pong(payload)).await;
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
            _ = tokio::time::sleep_until(next_ping) => {
                next_ping = Instant::now() + Duration::from_millis(PING_INTERVAL_MS);
                let _ = write.send(Message::Text("PING".to_string().into())).await;
            }
        }
    }
    Ok(())
}

fn build_subscribe_message() -> String {
    // RTDS expects `filters` to be a string; for Chainlink it is a JSON string like:
    // {"symbol":"btc/usd"}.
    let chainlink_filters = serde_json::json!({ "symbol": CHAINLINK_SYMBOL }).to_string();
    // Observed RTDS validation requires `filters` to be a JSON object/array string, not a plain
    // symbol string, so we encode Binance filters the same way.
    let binance_filters = serde_json::json!({ "symbol": BINANCE_SYMBOL }).to_string();
    serde_json::json!({
        "action": "subscribe",
        "subscriptions": [
            {
                "topic": TOPIC_BINANCE,
                "type": "update",
                "filters": binance_filters
            },
            {
                "topic": TOPIC_CHAINLINK,
                "type": "*",
                "filters": chainlink_filters
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

fn log_raw_frame(log_tx: &Option<Sender<LogEvent>>, event: &str, ts_ms: i64, text: &str) {
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

    async fn recv_with_advance(
        rx: &mut tokio::sync::mpsc::Receiver<String>,
        timeout: Duration,
    ) -> String {
        use tokio::sync::mpsc::error::TryRecvError;

        let step = Duration::from_millis(10);
        let mut waited = Duration::ZERO;
        loop {
            match rx.try_recv() {
                Ok(msg) => return msg,
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => panic!("server channel disconnected"),
            }

            if waited >= timeout {
                panic!("timed out waiting for server message");
            }

            tokio::time::advance(step).await;
            tokio::task::yield_now().await;
            waited += step;
        }
    }

    #[test]
    fn parses_binance_btcusdt_update() {
        let msg = r#"{"topic":"crypto_prices","type":"update","timestamp":1753314088421,"payload":{"symbol":"btcusdt","timestamp":1753314088395,"value":67234.5}}"#;
        let update = parse_rtds_update(msg).expect("parsed update");
        assert_eq!(update.source, RTDSSource::BinanceBtcUsdt);
        assert!((update.price - 67234.5).abs() < 1e-9);
        assert_eq!(update.ts_ms, 1753314088395);
    }

    #[test]
    fn subscribe_message_uses_chainlink_filter_string() {
        let msg = build_subscribe_message();
        let value: Value = serde_json::from_str(&msg).expect("valid json");
        let subs = value
            .get("subscriptions")
            .and_then(|v| v.as_array())
            .expect("subscriptions array");

        let mut saw_chainlink = false;
        let mut saw_binance = false;
        for sub in subs {
            let topic = sub
                .get("topic")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            match topic {
                TOPIC_CHAINLINK => {
                    saw_chainlink = true;
                    let filters = sub
                        .get("filters")
                        .and_then(|v| v.as_str())
                        .expect("chainlink filters string");
                    let parsed: Value =
                        serde_json::from_str(filters).expect("filters must be json string");
                    let symbol = parsed
                        .get("symbol")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert_eq!(symbol, CHAINLINK_SYMBOL);
                }
                TOPIC_BINANCE => {
                    saw_binance = true;
                    let filters = sub
                        .get("filters")
                        .and_then(|v| v.as_str())
                        .expect("binance filters string");
                    let parsed: Value =
                        serde_json::from_str(filters).expect("filters must be json string");
                    let symbol = parsed
                        .get("symbol")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert_eq!(symbol, BINANCE_SYMBOL);
                }
                _ => {}
            }
        }

        assert!(saw_chainlink, "missing chainlink subscription");
        assert!(saw_binance, "missing binance subscription");
    }

    #[tokio::test(start_paused = true)]
    async fn sends_text_ping_keepalives() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        let (tx_frames, mut rx_frames) = tokio::sync::mpsc::channel::<String>(8);

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("accept_async");

            while let Some(msg) = ws.next().await {
                let Ok(msg) = msg else { break };
                match msg {
                    Message::Text(text) => {
                        let text = text.to_string();
                        if tx_frames.send(text.clone()).await.is_err() {
                            break;
                        }
                        if text.eq_ignore_ascii_case("PING") {
                            let _ = ws.send(Message::Text("PONG".to_string().into())).await;
                            let _ = ws.close(None).await;
                            break;
                        }
                    }
                    Message::Ping(payload) => {
                        let _ = ws.send(Message::Pong(payload)).await;
                    }
                    _ => {}
                }
            }
        });

        let url = format!("ws://{}", addr);
        let client = RTDSLoop::new(url, 60_000);
        let (tx_events, _rx_events) = tokio::sync::mpsc::channel::<AppEvent>(8);
        let client_task =
            tokio::spawn(async move { client.connect_and_run(&tx_events, None).await });

        let subscribe = recv_with_advance(&mut rx_frames, Duration::from_secs(1)).await;
        assert!(subscribe.contains("\"action\":\"subscribe\""));

        tokio::time::advance(Duration::from_millis(PING_INTERVAL_MS)).await;
        tokio::task::yield_now().await;

        let ping = recv_with_advance(&mut rx_frames, Duration::from_secs(1)).await;
        assert!(
            ping.eq_ignore_ascii_case("PING"),
            "expected PING, got {ping:?}"
        );

        server.await.expect("server task");
        client_task.await.expect("client task").expect("client ok");
    }
}
