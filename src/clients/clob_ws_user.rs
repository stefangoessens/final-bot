use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;
use tokio::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::config::AppConfig;
use crate::error::{BotError, BotResult};
use crate::persistence::LogEvent;
use crate::state::state_manager::{AppEvent, FillEvent, UserWsUpdate};

const USER_CHANNEL: &str = "user";
const PING_INTERVAL_MS: u64 = 10_000;
const BACKOFF_MIN_MS: u64 = 500;
const BACKOFF_MAX_MS: u64 = 20_000;
const BACKOFF_MULTIPLIER: f64 = 1.7;
const MAX_RAW_LOG_BYTES: usize = 8 * 1024;
const REDACT_PLACEHOLDER: &str = "[REDACTED]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserOrderUpdateType {
    Placement,
    Update,
    Cancellation,
    Rejection,
}

#[derive(Debug, Clone)]
pub struct UserWsLoop {
    url: String,
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    markets: Vec<String>,
    user_order_tx: Option<Sender<UserOrderUpdate>>,
}

#[derive(Debug, Clone)]
pub struct UserOrderUpdate {
    pub order_id: String,
    pub token_id: String,
    pub price: Option<f64>,
    pub original_size: Option<f64>,
    pub size_matched: Option<f64>,
    pub ts_ms: i64,
    pub update_type: UserOrderUpdateType,
}

impl UserWsLoop {
    pub fn from_config(
        cfg: &AppConfig,
        markets: Vec<String>,
        user_order_tx: Option<Sender<UserOrderUpdate>>,
    ) -> BotResult<Self> {
        let api_key = cfg
            .keys
            .clob_api_key
            .as_deref()
            .unwrap_or_default()
            .to_string();
        let api_secret = cfg
            .keys
            .clob_api_secret
            .as_deref()
            .unwrap_or_default()
            .to_string();
        let api_passphrase = cfg
            .keys
            .clob_api_passphrase
            .as_deref()
            .unwrap_or_default()
            .to_string();

        if api_key.is_empty() {
            return Err(BotError::Config(
                "missing required key: PMMB_KEYS__CLOB_API_KEY".to_string(),
            ));
        }
        if api_secret.is_empty() {
            return Err(BotError::Config(
                "missing required key: PMMB_KEYS__CLOB_API_SECRET".to_string(),
            ));
        }
        if api_passphrase.is_empty() {
            return Err(BotError::Config(
                "missing required key: PMMB_KEYS__CLOB_API_PASSPHRASE".to_string(),
            ));
        }

        Ok(Self::new(
            cfg.endpoints.clob_ws_user_url.clone(),
            api_key,
            api_secret,
            api_passphrase,
            markets,
            user_order_tx,
        ))
    }

    pub fn new(
        url: String,
        api_key: String,
        api_secret: String,
        api_passphrase: String,
        markets: Vec<String>,
        user_order_tx: Option<Sender<UserOrderUpdate>>,
    ) -> Self {
        Self {
            url,
            api_key,
            api_secret,
            api_passphrase,
            markets,
            user_order_tx,
        }
    }

    pub async fn run(
        self,
        tx_events: Sender<AppEvent>,
        log_tx: Option<Sender<LogEvent>>,
    ) -> BotResult<()> {
        let user_order_tx = self.user_order_tx.clone();
        self.run_with_order_updates(tx_events, user_order_tx, log_tx).await
    }

    pub async fn run_with_order_updates(
        self,
        tx_events: Sender<AppEvent>,
        tx_order_updates: Option<Sender<UserOrderUpdate>>,
        log_tx: Option<Sender<LogEvent>>,
    ) -> BotResult<()> {
        let mut backoff = Backoff::new(BACKOFF_MIN_MS, BACKOFF_MAX_MS, BACKOFF_MULTIPLIER);

        loop {
            tracing::info!(target: "clob_ws_user", url = %self.url, "connecting");
            match connect_async(&self.url).await {
                Ok((ws_stream, _)) => {
                    tracing::info!(target: "clob_ws_user", url = %self.url, "connected");
                    backoff.reset();

                    let order_tx = tx_order_updates.clone();
                    if let Err(err) =
                        self.run_session(ws_stream, &tx_events, order_tx, log_tx.clone()).await
                    {
                        tracing::warn!(
                            target: "clob_ws_user",
                            url = %self.url,
                            error = %err,
                            "session ended"
                        );
                    }

                    tracing::info!(target: "clob_ws_user", url = %self.url, "disconnected");
                }
                Err(err) => {
                    tracing::warn!(
                        target: "clob_ws_user",
                        url = %self.url,
                        error = %err,
                        "connect failed"
                    );
                }
            }

            let delay_ms = backoff.next_delay_ms();
            tracing::info!(
                target: "clob_ws_user",
                url = %self.url,
                backoff_ms = delay_ms,
                "reconnecting after backoff"
            );
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }

    async fn run_session(
        &self,
        ws_stream: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tx_events: &Sender<AppEvent>,
        tx_order_updates: Option<Sender<UserOrderUpdate>>,
        log_tx: Option<Sender<LogEvent>>,
    ) -> BotResult<()> {
        let (mut write, mut read) = ws_stream.split();
        let subscribe = self.subscribe_payload()?;
        write.send(Message::Text(subscribe.into())).await?;

        let mut next_ping = Instant::now() + Duration::from_millis(PING_INTERVAL_MS);

        loop {
            tokio::select! {
                msg = read.next() => {
                    let msg = match msg {
                        Some(Ok(msg)) => msg,
                        Some(Err(err)) => return Err(BotError::Other(format!("ws read error: {err}"))),
                        None => return Err(BotError::Other("ws stream closed".to_string())),
                    };

                    match msg {
                        Message::Text(text) => {
                            let ts_ms = now_ms();
                            log_raw_frame(&log_tx, "ws.user.raw", ts_ms, &text);
                            if text.eq_ignore_ascii_case("ping") {
                                write.send(Message::Text("PONG".to_string().into())).await?;
                            } else {
                                self.handle_text(
                                    &text,
                                    tx_events,
                                    tx_order_updates.as_ref(),
                                    log_tx.as_ref(),
                                )
                                .await?;
                            }
                        }
                        Message::Binary(_) => {
                            // Ignore binary frames for now.
                        }
                        Message::Ping(payload) => {
                            write.send(Message::Pong(payload)).await?;
                        }
                        Message::Pong(_) => {}
                        Message::Close(_) => {
                            return Err(BotError::Other("ws close received".to_string()));
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(next_ping) => {
                    next_ping = Instant::now() + Duration::from_millis(PING_INTERVAL_MS);
                    let _ = write.send(Message::Text("PING".to_string().into())).await;
                }
            }
        }
    }

    async fn handle_text(
        &self,
        text: &str,
        tx_events: &Sender<AppEvent>,
        tx_order_updates: Option<&Sender<UserOrderUpdate>>,
        log_tx: Option<&Sender<LogEvent>>,
    ) -> BotResult<()> {
        let mut heartbeat_ts = None;

        match parse_fill_event(text) {
            Ok(Some(fill)) => {
                heartbeat_ts = Some(fill.ts_ms);
                log_fill_event(log_tx, &fill);
                tracing::info!(
                    target: "clob_ws_user",
                    token_id = %fill.token_id,
                    price = fill.price,
                    shares = fill.shares,
                    ts_ms = fill.ts_ms,
                    "fill"
                );
                if tx_events
                    .send(AppEvent::UserWsUpdate(UserWsUpdate::Fill(fill)))
                    .await
                    .is_err()
                {
                    return Err(BotError::Other("event channel closed".to_string()));
                }
            }
            Ok(None) => {
                let order_update = match parse_order_event(text) {
                    Ok(update) => update,
                    Err(err) => {
                        tracing::warn!(target: "clob_ws_user", error = %err, "order parse error");
                        None
                    }
                };

                if let Some(update) = order_update {
                    heartbeat_ts = Some(update.ts_ms);
                    log_user_order_update(log_tx, &update);
                    if let Some(tx_orders) = tx_order_updates {
                        if tx_orders.send(update).await.is_err() {
                            tracing::warn!(
                                target: "clob_ws_user",
                                "order update channel closed"
                            );
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!(target: "clob_ws_user", error = %err, "parse error");
            }
        }

        if heartbeat_ts.is_none() {
            heartbeat_ts = extract_event_timestamp_ms(text).unwrap_or(None);
        }

        if let Some(ts_ms) = heartbeat_ts {
            let _ = tx_events
                .send(AppEvent::UserWsUpdate(UserWsUpdate::Heartbeat { ts_ms }))
                .await;
        }

        Ok(())
    }

    fn subscribe_payload(&self) -> BotResult<String> {
        let auth = AuthPayload {
            api_key: &self.api_key,
            secret: &self.api_secret,
            passphrase: &self.api_passphrase,
        };

        let payload = SubscribePayload {
            channel: USER_CHANNEL,
            auth,
            markets: if self.markets.is_empty() {
                None
            } else {
                Some(&self.markets)
            },
            custom_feature_enabled: None,
        };

        serde_json::to_string(&payload)
            .map_err(|e| BotError::Other(format!("failed to serialize subscribe payload: {e}")))
    }
}

#[derive(Debug, Serialize)]
struct SubscribePayload<'a> {
    #[serde(rename = "type")]
    channel: &'a str,
    auth: AuthPayload<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    markets: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_feature_enabled: Option<bool>,
}

#[derive(Debug, Serialize)]
struct AuthPayload<'a> {
    // Docs show apiKey in Data Feeds/WSS Quickstart; WSS Auth uses apikey.
    #[serde(rename = "apiKey")]
    api_key: &'a str,
    secret: &'a str,
    passphrase: &'a str,
}

#[derive(Debug)]
struct Backoff {
    current_ms: u64,
    min_ms: u64,
    max_ms: u64,
    factor: f64,
}

impl Backoff {
    fn new(min_ms: u64, max_ms: u64, factor: f64) -> Self {
        Self {
            current_ms: min_ms,
            min_ms,
            max_ms,
            factor,
        }
    }

    fn reset(&mut self) {
        self.current_ms = self.min_ms;
    }

    fn next_delay_ms(&mut self) -> u64 {
        let delay = self.current_ms;
        let next = (self.current_ms as f64 * self.factor).round() as u64;
        self.current_ms = next.clamp(self.min_ms, self.max_ms);
        delay
    }
}

fn parse_fill_event(text: &str) -> BotResult<Option<FillEvent>> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| BotError::Other(format!("invalid json: {e}")))?;

    let event_type = value
        .get("event_type")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("type").and_then(|v| v.as_str()))
        .unwrap_or_default()
        .to_lowercase();

    if event_type != "trade" {
        return Ok(None);
    }

    let token_id = value
        .get("asset_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BotError::Other("trade missing asset_id".to_string()))?;

    let price = value
        .get("price")
        .and_then(value_to_string)
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or_else(|| BotError::Other("trade missing price".to_string()))?;

    let shares = value
        .get("size")
        .and_then(value_to_string)
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or_else(|| BotError::Other("trade missing size".to_string()))?;

    let ts_ms = extract_timestamp_from_value(&value).unwrap_or_else(now_ms);

    Ok(Some(FillEvent {
        token_id: token_id.to_string(),
        price,
        shares,
        ts_ms,
    }))
}

fn parse_order_event(text: &str) -> BotResult<Option<UserOrderUpdate>> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| BotError::Other(format!("invalid json: {e}")))?;

    let event_type = value
        .get("event_type")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_lowercase();

    let raw_update_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let update_type = raw_update_type.to_ascii_uppercase();
    let status = value
        .get("status")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("order_status").and_then(|v| v.as_str()))
        .unwrap_or("");
    let status = status.to_ascii_uppercase();
    let is_rejection = matches!(update_type.as_str(), "REJECTION" | "REJECTED")
        || matches!(status.as_str(), "REJECTION" | "REJECTED" | "REJECT")
        || matches!(event_type.as_str(), "rejection" | "rejected" | "reject");
    let is_order_type = matches!(
        update_type.as_str(),
        "PLACEMENT" | "UPDATE" | "CANCELLATION" | "REJECTION" | "REJECTED"
    );

    if event_type != "order" && !is_order_type && !is_rejection {
        return Ok(None);
    }

    let order_id = match value
        .get("order_id")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("id").and_then(|v| v.as_str()))
    {
        Some(id) => id,
        None => return Ok(None),
    };
    let token_id = value
        .get("asset_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let price = value
        .get("price")
        .and_then(value_to_string)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite());
    let original_size = value
        .get("original_size")
        .and_then(value_to_string)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite());
    let size_matched = value
        .get("size_matched")
        .and_then(value_to_string)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite());
    let ts_ms = extract_timestamp_from_value(&value).unwrap_or_else(now_ms);

    let update_type = if is_rejection {
        UserOrderUpdateType::Rejection
    } else {
        match update_type.as_str() {
            "PLACEMENT" => UserOrderUpdateType::Placement,
            "UPDATE" => UserOrderUpdateType::Update,
            "CANCELLATION" => UserOrderUpdateType::Cancellation,
            "" if event_type == "order" => UserOrderUpdateType::Update,
            _ => return Ok(None),
        }
    };

    Ok(Some(UserOrderUpdate {
        order_id: order_id.to_string(),
        token_id: token_id.to_string(),
        price,
        original_size,
        size_matched,
        ts_ms,
        update_type,
    }))
}

fn extract_event_timestamp_ms(text: &str) -> BotResult<Option<i64>> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| BotError::Other(format!("invalid json: {e}")))?;
    Ok(extract_timestamp_from_value(&value))
}

fn extract_timestamp_from_value(value: &Value) -> Option<i64> {
    for key in ["timestamp", "last_update", "matchtime"] {
        if let Some(raw) = value.get(key).and_then(value_to_string) {
            if let Some(ts_ms) = parse_timestamp_ms(&raw) {
                return Some(ts_ms);
            }
        }
    }
    None
}

fn parse_timestamp_ms(raw: &str) -> Option<i64> {
    let parsed = raw.parse::<i64>().ok()?;
    if parsed < 1_000_000_000_000 {
        Some(parsed.saturating_mul(1_000))
    } else {
        Some(parsed)
    }
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
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

fn log_fill_event(log_tx: Option<&Sender<LogEvent>>, fill: &FillEvent) {
    let Some(tx) = log_tx else {
        return;
    };
    let payload = json!({
        "token_id": fill.token_id,
        "price": fill.price,
        "shares": fill.shares,
    });
    let _ = tx.try_send(LogEvent {
        ts_ms: fill.ts_ms,
        event: "ws.user.fill".to_string(),
        payload,
    });
}

fn log_user_order_update(log_tx: Option<&Sender<LogEvent>>, update: &UserOrderUpdate) {
    let Some(tx) = log_tx else {
        return;
    };
    let update_type = match update.update_type {
        UserOrderUpdateType::Placement => "PLACEMENT",
        UserOrderUpdateType::Update => "UPDATE",
        UserOrderUpdateType::Cancellation => "CANCELLATION",
        UserOrderUpdateType::Rejection => "REJECTION",
    };
    let payload = json!({
        "order_id": update.order_id,
        "token_id": update.token_id,
        "price": update.price,
        "original_size": update.original_size,
        "size_matched": update.size_matched,
        "update_type": update_type,
    });
    let _ = tx.try_send(LogEvent {
        ts_ms: update.ts_ms,
        event: "ws.user.order_update".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_trade_message_into_fill_event() {
        let raw = r#"{
            "asset_id": "52114319501245915516055106046884209969926127482827954674443846427813813222426",
            "event_type": "trade",
            "price": "0.57",
            "size": "10",
            "timestamp": "1672290701"
        }"#;

        let fill = parse_fill_event(raw).unwrap().unwrap();
        assert_eq!(
            fill.token_id,
            "52114319501245915516055106046884209969926127482827954674443846427813813222426"
        );
        assert!((fill.price - 0.57).abs() < 1e-12);
        assert!((fill.shares - 10.0).abs() < 1e-12);
        assert_eq!(fill.ts_ms, 1_672_290_701_000);
    }

    #[test]
    fn parse_order_message_placement() {
        let raw = r#"{
            "asset_id": "52114319501245915516055106046884209969926127482827954674443846427813813222426",
            "associate_trades": null,
            "event_type": "order",
            "id": "0xff354cd7ca7539dfa9c28d90943ab5779a4eac34b9b37a757d7b32bdfb11790b",
            "market": "0xbd31dc8a20211944f6b70f31557f1001557b59905b7738480ca09bd4532f84af",
            "order_owner": "9180014b-33c8-9240-a14b-bdca11c0a465",
            "original_size": "10",
            "outcome": "YES",
            "owner": "9180014b-33c8-9240-a14b-bdca11c0a465",
            "price": "0.57",
            "side": "SELL",
            "size_matched": "0",
            "timestamp": "1672290687",
            "type": "PLACEMENT"
        }"#;

        let update = parse_order_event(raw).unwrap().unwrap();
        assert_eq!(
            update.order_id,
            "0xff354cd7ca7539dfa9c28d90943ab5779a4eac34b9b37a757d7b32bdfb11790b"
        );
        assert_eq!(
            update.token_id,
            "52114319501245915516055106046884209969926127482827954674443846427813813222426"
        );
        assert_eq!(update.update_type, UserOrderUpdateType::Placement);
        assert_eq!(update.price, Some(0.57));
        assert_eq!(update.original_size, Some(10.0));
        assert_eq!(update.size_matched, Some(0.0));
        assert_eq!(update.ts_ms, 1_672_290_687_000);
    }

    #[test]
    fn parse_order_message_update() {
        let raw = r#"{
            "asset_id": "52114319501245915516055106046884209969926127482827954674443846427813813222426",
            "associate_trades": ["28c4d2eb-bbea-40e7-a9f0-b2fdb56b2c2e"],
            "event_type": "order",
            "id": "0xff354cd7ca7539dfa9c28d90943ab5779a4eac34b9b37a757d7b32bdfb11790b",
            "market": "0xbd31dc8a20211944f6b70f31557f1001557b59905b7738480ca09bd4532f84af",
            "order_owner": "9180014b-33c8-9240-a14b-bdca11c0a465",
            "original_size": "10",
            "outcome": "YES",
            "owner": "9180014b-33c8-9240-a14b-bdca11c0a465",
            "price": "0.57",
            "side": "SELL",
            "size_matched": "3",
            "timestamp": "1672290687",
            "type": "UPDATE"
        }"#;

        let update = parse_order_event(raw).unwrap().unwrap();
        assert_eq!(update.update_type, UserOrderUpdateType::Update);
        assert_eq!(update.size_matched, Some(3.0));
        assert_eq!(update.original_size, Some(10.0));
        assert_eq!(update.ts_ms, 1_672_290_687_000);
    }

    #[test]
    fn parse_order_message_cancellation() {
        let raw = r#"{
            "asset_id": "52114319501245915516055106046884209969926127482827954674443846427813813222426",
            "associate_trades": null,
            "event_type": "order",
            "id": "0xff354cd7ca7539dfa9c28d90943ab5779a4eac34b9b37a757d7b32bdfb11790b",
            "market": "0xbd31dc8a20211944f6b70f31557f1001557b59905b7738480ca09bd4532f84af",
            "order_owner": "9180014b-33c8-9240-a14b-bdca11c0a465",
            "original_size": "10",
            "outcome": "YES",
            "owner": "9180014b-33c8-9240-a14b-bdca11c0a465",
            "price": "0.57",
            "side": "SELL",
            "size_matched": "0",
            "timestamp": "1672290687",
            "type": "CANCELLATION"
        }"#;

        let update = parse_order_event(raw).unwrap().unwrap();
        assert_eq!(update.update_type, UserOrderUpdateType::Cancellation);
        assert_eq!(update.size_matched, Some(0.0));
    }

    #[test]
    fn parse_order_message_rejection_fixture() {
        // Synthetic fixture; real reject schema needs capture; update fixture when we record a live frame.
        let raw = include_str!("../../fixtures/user_ws_reject.json");
        let update = parse_order_event(raw).unwrap().unwrap();
        assert_eq!(update.update_type, UserOrderUpdateType::Rejection);
        assert_eq!(update.order_id, "0xreject");
        assert_eq!(update.token_id, "token-reject");
        assert_eq!(update.price, Some(0.57));
        assert_eq!(update.original_size, Some(10.0));
        assert_eq!(update.size_matched, Some(0.0));
    }

    #[test]
    fn parse_order_message_partial_update_missing_sizes() {
        let raw = r#"{
            "asset_id": "52114319501245915516055106046884209969926127482827954674443846427813813222426",
            "event_type": "order",
            "id": "0xff354cd7ca7539dfa9c28d90943ab5779a4eac34b9b37a757d7b32bdfb11790b",
            "timestamp": "1672290687",
            "type": "UPDATE"
        }"#;

        let update = parse_order_event(raw).unwrap().unwrap();
        assert_eq!(update.update_type, UserOrderUpdateType::Update);
        assert_eq!(update.price, None);
        assert_eq!(update.original_size, None);
        assert_eq!(update.size_matched, None);
        assert_eq!(update.ts_ms, 1_672_290_687_000);
    }
}
