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

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Debug, Clone)]
pub struct RTDSLoop {
    url: String,
    backoff_min: Duration,
    backoff_max: Duration,
}

impl RTDSLoop {
    pub fn new(url: String) -> Self {
        let url = if url.trim().is_empty() {
            DEFAULT_RTDS_URL.to_string()
        } else {
            url
        };
        Self {
            url,
            backoff_min: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
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

        read_loop(ws, tx_events, log_tx).await
    }
}

async fn read_loop(
    mut ws: WsStream,
    tx_events: &Sender<AppEvent>,
    log_tx: Option<Sender<LogEvent>>,
) -> BotResult<()> {
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                let ts_ms = now_ms();
                log_raw_frame(&log_tx, "ws.rtds.raw", ts_ms, &text);
                if let Some(update) = parse_rtds_update(&text) {
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
    Ok(())
}

fn build_subscribe_message() -> String {
    let chainlink_filter = serde_json::json!({ "symbol": CHAINLINK_SYMBOL }).to_string();
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
                "filters": chainlink_filter
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
    let payload = json!({
        "frame": truncate_utf8(text, MAX_RAW_LOG_BYTES),
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
}
