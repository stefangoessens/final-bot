use std::sync::atomic::{AtomicUsize, Ordering};

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
use crate::state::state_manager::{AppEvent, FillEvent, OrderSide, UserWsUpdate};

const USER_CHANNEL: &str = "user";
const PING_INTERVAL_MS: u64 = 10_000;
const BACKOFF_MIN_MS: u64 = 500;
const BACKOFF_MAX_MS: u64 = 20_000;
const BACKOFF_MULTIPLIER: f64 = 1.7;
const MAX_RAW_LOG_BYTES: usize = 8 * 1024;
// Log 1 out of N inbound raw WS frames (before redaction) to reduce CPU.
// Set to 1 to disable sampling and log every frame.
const RAW_LOG_SAMPLE_EVERY: u64 = 16;
const REDACT_PLACEHOLDER: &str = "[REDACTED]";

static ORDER_UPDATE_DROPPED: AtomicUsize = AtomicUsize::new(0);

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
    pub side: Option<OrderSide>,
    pub price: Option<f64>,
    pub original_size: Option<f64>,
    pub size_matched: Option<f64>,
    pub ts_ms: i64,
    pub update_type: UserOrderUpdateType,
}

impl UserWsLoop {
    #[allow(dead_code)]
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
        self.run_with_order_updates(tx_events, user_order_tx, log_tx)
            .await
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
                    if let Err(err) = self
                        .run_session(ws_stream, &tx_events, order_tx, log_tx.clone())
                        .await
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
        let mut raw_frame_counter: u64 = 0;
        let mut fill_tracker = FillTracker::default();

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
                            if log_tx.is_some()
                                && (RAW_LOG_SAMPLE_EVERY <= 1
                                    || raw_frame_counter.is_multiple_of(RAW_LOG_SAMPLE_EVERY))
                            {
                                log_raw_frame(&log_tx, "ws.user.raw", ts_ms, &text);
                            }
                            raw_frame_counter = raw_frame_counter.wrapping_add(1);
                            if text.eq_ignore_ascii_case("ping") {
                                write.send(Message::Text("PONG".to_string().into())).await?;
                            } else {
                                self.handle_text(
                                    &text,
                                    tx_events,
                                    tx_order_updates.as_ref(),
                                    log_tx.as_ref(),
                                    &mut fill_tracker,
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
                        Message::Pong(_) => {
                            let _ = tx_events
                                .send(AppEvent::UserWsUpdate(UserWsUpdate::Heartbeat {
                                    ts_ms: now_ms(),
                                }))
                                .await;
                        }
                        Message::Close(_) => {
                            return Err(BotError::Other("ws close received".to_string()));
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(next_ping) => {
                    next_ping = Instant::now() + Duration::from_millis(PING_INTERVAL_MS);
                    let _ = write.send(Message::Ping(Vec::new().into())).await;
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
        fill_tracker: &mut FillTracker,
    ) -> BotResult<()> {
        let mut heartbeat_ts = None;

        let order_update = match parse_order_event(text) {
            Ok(update) => update,
            Err(err) => {
                tracing::warn!(target: "clob_ws_user", error = %err, "order parse error");
                None
            }
        };

        if let Some(update) = order_update {
            heartbeat_ts = Some(update.ts_ms);

            if let Some(fill) = fill_tracker.apply(&update) {
                log_fill_event(log_tx, &fill);
                tracing::info!(
                    target: "clob_ws_user",
                    token_id = %fill.token_id,
                    side = ?fill.side,
                    price = fill.price,
                    shares = fill.shares,
                    ts_ms = fill.ts_ms,
                    "fill"
                );
                if let Err(err) = tx_events
                    .send(AppEvent::UserWsUpdate(UserWsUpdate::Fill(fill)))
                    .await
                {
                    tracing::warn!(
                        target: "clob_ws_user",
                        error = %err,
                        "state manager channel closed; dropping fill update"
                    );
                }
            }

            log_user_order_update(log_tx, &update);
            if let Some(tx_orders) = tx_order_updates {
                match tx_orders.try_send(update) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(update)) => {
                        let count = ORDER_UPDATE_DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
                        tracing::debug!(
                            target: "clob_ws_user",
                            order_id = %update.order_id,
                            token_id = %update.token_id,
                            dropped = count,
                            "order update channel full; dropping update"
                        );
                        if count.is_multiple_of(100) {
                            tracing::warn!(
                                target: "clob_ws_user",
                                dropped = count,
                                "order update channel full; dropping updates"
                            );
                        }
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        tracing::warn!(
                            target: "clob_ws_user",
                            "order update channel closed"
                        );
                    }
                }
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

#[allow(dead_code)] // retained for schema regression tests; fill accounting uses order updates
fn parse_trade_fills(text: &str, maker_addresses: &[String]) -> BotResult<Vec<FillEvent>> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| BotError::Other(format!("invalid json: {e}")))?;

    let event_type = value
        .get("event_type")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("type").and_then(|v| v.as_str()))
        .unwrap_or_default()
        .to_lowercase();

    if event_type != "trade" {
        return Ok(Vec::new());
    }

    let ts_ms = extract_timestamp_from_value(&value).unwrap_or_else(now_ms);

    // Trade payload `side` denotes the taker/market-order direction (BUY/SELL). Since we attribute
    // fills by matching our `maker_orders`, our maker-side fill direction is the opposite.
    let maker_side = match value.get("side").and_then(|v| v.as_str()).map(str::trim) {
        Some(raw) if raw.eq_ignore_ascii_case("BUY") => OrderSide::Sell,
        Some(raw) if raw.eq_ignore_ascii_case("SELL") => OrderSide::Buy,
        Some(other) => {
            return Err(BotError::Other(format!(
                "trade event missing/invalid side: {other:?}"
            )));
        }
        None => {
            return Err(BotError::Other(
                "trade event missing/invalid side: <missing>".to_string(),
            ));
        }
    };

    let Some(orders) = value.get("maker_orders").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };

    if maker_addresses.is_empty() {
        return Ok(Vec::new());
    }

    let mut fills = Vec::new();
    for order in orders {
        let maker_address = match order.get("maker_address").and_then(|v| v.as_str()) {
            Some(addr) => addr.trim(),
            None => continue,
        };
        if maker_address.is_empty() {
            continue;
        }
        if !maker_addresses
            .iter()
            .any(|addr| addr.eq_ignore_ascii_case(maker_address))
        {
            continue;
        }

        let token_id = match order.get("asset_id").and_then(|v| v.as_str()) {
            Some(id) if !id.trim().is_empty() => id.trim(),
            _ => continue,
        };
        let price = match order
            .get("price")
            .and_then(value_to_string)
            .and_then(|s| s.parse::<f64>().ok())
        {
            Some(v) => v,
            None => continue,
        };
        let shares = match order
            .get("matched_amount")
            .and_then(value_to_string)
            .and_then(|s| s.parse::<f64>().ok())
        {
            Some(v) => v,
            None => continue,
        };
        if !shares.is_finite() || shares <= 0.0 {
            continue;
        }

        fills.push(FillEvent {
            token_id: token_id.to_string(),
            side: maker_side,
            price,
            shares,
            ts_ms,
        });
    }

    Ok(fills)
}

#[derive(Debug, Default)]
struct FillTracker {
    by_order_id: std::collections::HashMap<String, FillTrackerEntry>,
}

#[derive(Debug, Clone)]
struct FillTrackerEntry {
    token_id: String,
    side: Option<OrderSide>,
    price: Option<f64>,
    matched: f64,
}

impl FillTracker {
    fn apply(&mut self, update: &UserOrderUpdate) -> Option<FillEvent> {
        let order_id = update.order_id.trim();
        let token_id = update.token_id.trim();
        if order_id.is_empty() || token_id.is_empty() {
            return None;
        }

        if matches!(
            update.update_type,
            UserOrderUpdateType::Cancellation | UserOrderUpdateType::Rejection
        ) {
            self.by_order_id.remove(order_id);
            return None;
        }

        let entry = self
            .by_order_id
            .entry(order_id.to_string())
            .or_insert_with(|| {
                // On reconnect, Polymarket may replay an order snapshot with a non-zero
                // cumulative `size_matched` (even if the event type looks like PLACEMENT).
                // Seed from the provided cumulative value to avoid double-counting past fills.
                let initial_matched = update
                    .size_matched
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .unwrap_or(0.0);

                FillTrackerEntry {
                    token_id: token_id.to_string(),
                    side: update.side,
                    price: update.price,
                    matched: initial_matched,
                }
            });

        if entry.token_id != token_id {
            entry.token_id = token_id.to_string();
        }
        if update.side.is_some() {
            entry.side = update.side;
        }
        if update.price.is_some() {
            entry.price = update.price;
        }

        let matched = update.size_matched.filter(|v| v.is_finite() && *v >= 0.0)?;
        let prev = entry.matched;
        if matched <= prev + 1e-12 {
            entry.matched = matched.max(prev);
            return None;
        }
        entry.matched = matched;

        let side = entry.side?;
        let price = entry.price.unwrap_or(0.0);
        if side == OrderSide::Buy && (price <= 0.0 || !price.is_finite()) {
            return None;
        }

        Some(FillEvent {
            token_id: entry.token_id.clone(),
            side,
            price,
            shares: matched - prev,
            ts_ms: update.ts_ms,
        })
    }
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
    let token_id = value.get("asset_id").and_then(|v| v.as_str()).unwrap_or("");
    let side = value.get("side").and_then(|v| v.as_str()).and_then(|raw| {
        let raw = raw.trim();
        if raw.eq_ignore_ascii_case("BUY") {
            Some(OrderSide::Buy)
        } else if raw.eq_ignore_ascii_case("SELL") {
            Some(OrderSide::Sell)
        } else {
            None
        }
    });
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
        side,
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

fn log_fill_event(log_tx: Option<&Sender<LogEvent>>, fill: &FillEvent) {
    let Some(tx) = log_tx else {
        return;
    };
    let side = match fill.side {
        OrderSide::Buy => "BUY",
        OrderSide::Sell => "SELL",
    };
    let payload = json!({
        "token_id": fill.token_id,
        "side": side,
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
    use tokio::time::{timeout, Duration};

    #[test]
    fn parse_trade_message_attributes_only_our_maker_orders() {
        let maker = "0x63db2184575e93ab59dcd167b2fa6013369561cf".to_string();
        let raw = r#"{
	            "event_type": "trade",
	            "side": "BUY",
	            "maker_orders": [
	              {
	                "asset_id": "token",
	                "maker_address": "0x63dB2184575e93Ab59DCd167b2Fa6013369561cf",
                "matched_amount": "0.5",
                "price": "0.41"
              },
              {
                "asset_id": "other",
                "maker_address": "0xdeadbeef",
                "matched_amount": "999",
                "price": "0.99"
              }
            ],
            "timestamp": "1672290701"
        }"#;

        let fills = parse_trade_fills(raw, &[maker]).unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].token_id, "token");
        assert_eq!(fills[0].side, OrderSide::Sell);
        assert!((fills[0].price - 0.41).abs() < 1e-12);
        assert!((fills[0].shares - 0.5).abs() < 1e-12);
        assert_eq!(fills[0].ts_ms, 1_672_290_701_000);
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

    #[tokio::test]
    async fn handle_text_does_not_block_when_order_update_channel_full() {
        let loop_ = UserWsLoop::new(
            "ws://127.0.0.1".to_string(),
            "api_key".to_string(),
            "api_secret".to_string(),
            "api_passphrase".to_string(),
            Vec::new(),
            None,
        );

        let (tx_events, _rx_events) = tokio::sync::mpsc::channel::<AppEvent>(8);
        let (tx_orders, _rx_orders) = tokio::sync::mpsc::channel::<UserOrderUpdate>(1);

        // Fill the channel so a send() would have to await.
        tx_orders
            .try_send(UserOrderUpdate {
                order_id: "order".to_string(),
                token_id: "token".to_string(),
                side: None,
                price: None,
                original_size: None,
                size_matched: None,
                ts_ms: 0,
                update_type: UserOrderUpdateType::Update,
            })
            .expect("fill");

        let raw = r#"{
            "asset_id": "token",
            "event_type": "order",
            "id": "order-2",
            "timestamp": "1672290687",
            "type": "UPDATE"
        }"#;

        let mut tracker = FillTracker::default();
        timeout(
            Duration::from_millis(50),
            loop_.handle_text(raw, &tx_events, Some(&tx_orders), None, &mut tracker),
        )
        .await
        .expect("handle_text should not block")
        .expect("handle_text ok");
    }

    #[tokio::test]
    async fn handle_text_emits_fill_from_order_update_delta() {
        let loop_ = UserWsLoop::new(
            "ws://127.0.0.1".to_string(),
            "api_key".to_string(),
            "api_secret".to_string(),
            "api_passphrase".to_string(),
            Vec::new(),
            None,
        );

        let (tx_events, mut rx_events) = tokio::sync::mpsc::channel::<AppEvent>(8);

        let placement = r#"{
            "asset_id": "token",
            "event_type": "order",
            "id": "order-1",
            "timestamp": "1672290701",
            "type": "PLACEMENT",
            "side": "BUY",
            "price": "0.41",
            "original_size": "1.0",
            "size_matched": "0"
        }"#;
        let update = r#"{
            "asset_id": "token",
            "event_type": "order",
            "id": "order-1",
            "timestamp": "1672290702",
            "type": "UPDATE",
            "side": "BUY",
            "size_matched": "0.5"
        }"#;

        let mut tracker = FillTracker::default();

        loop_
            .handle_text(placement, &tx_events, None, None, &mut tracker)
            .await
            .expect("handle_text ok");

        // Placement emits a heartbeat but no fill.
        let first = timeout(Duration::from_millis(50), rx_events.recv())
            .await
            .expect("heartbeat should arrive")
            .expect("channel open");
        match first {
            AppEvent::UserWsUpdate(UserWsUpdate::Heartbeat { ts_ms }) => {
                assert_eq!(ts_ms, 1_672_290_701_000);
            }
            other => panic!("expected Heartbeat update, got {other:?}"),
        }

        loop_
            .handle_text(update, &tx_events, None, None, &mut tracker)
            .await
            .expect("handle_text ok");

        // Update emits a fill then a heartbeat.
        let second = timeout(Duration::from_millis(50), rx_events.recv())
            .await
            .expect("fill event should arrive")
            .expect("channel open");
        match second {
            AppEvent::UserWsUpdate(UserWsUpdate::Fill(fill)) => {
                assert_eq!(fill.token_id, "token");
                assert_eq!(fill.side, OrderSide::Buy);
                assert!((fill.price - 0.41).abs() < 1e-12);
                assert!((fill.shares - 0.5).abs() < 1e-12);
                assert_eq!(fill.ts_ms, 1_672_290_702_000);
            }
            other => panic!("expected Fill update, got {other:?}"),
        }

        let third = timeout(Duration::from_millis(50), rx_events.recv())
            .await
            .expect("heartbeat should arrive")
            .expect("channel open");
        match third {
            AppEvent::UserWsUpdate(UserWsUpdate::Heartbeat { ts_ms }) => {
                assert_eq!(ts_ms, 1_672_290_702_000);
            }
            other => panic!("expected Heartbeat update, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_text_does_not_double_count_fill_when_first_seen_is_update() {
        let loop_ = UserWsLoop::new(
            "ws://127.0.0.1".to_string(),
            "api_key".to_string(),
            "api_secret".to_string(),
            "api_passphrase".to_string(),
            Vec::new(),
            None,
        );

        let (tx_events, mut rx_events) = tokio::sync::mpsc::channel::<AppEvent>(8);

        // Simulate a reconnect: the first message we see for an existing order is an UPDATE
        // with a non-zero cumulative matched amount.
        let update = r#"{
            "asset_id": "token",
            "event_type": "order",
            "id": "order-1",
            "timestamp": "1672290702",
            "type": "UPDATE",
            "side": "BUY",
            "price": "0.41",
            "original_size": "1.0",
            "size_matched": "0.5"
        }"#;

        let mut tracker = FillTracker::default();

        loop_
            .handle_text(update, &tx_events, None, None, &mut tracker)
            .await
            .expect("handle_text ok");

        // We should only get a heartbeat; the cumulative size_matched must not be emitted as a fill.
        let first = timeout(Duration::from_millis(50), rx_events.recv())
            .await
            .expect("heartbeat should arrive")
            .expect("channel open");
        match first {
            AppEvent::UserWsUpdate(UserWsUpdate::Heartbeat { ts_ms }) => {
                assert_eq!(ts_ms, 1_672_290_702_000);
            }
            other => panic!("expected Heartbeat update, got {other:?}"),
        }

        let no_fill = timeout(Duration::from_millis(50), rx_events.recv()).await;
        assert!(no_fill.is_err(), "unexpected extra event: {no_fill:?}");
    }

    #[tokio::test]
    async fn handle_text_does_not_double_count_fill_when_first_seen_is_placement_with_nonzero_matched(
    ) {
        let loop_ = UserWsLoop::new(
            "ws://127.0.0.1".to_string(),
            "api_key".to_string(),
            "api_secret".to_string(),
            "api_passphrase".to_string(),
            Vec::new(),
            None,
        );

        let (tx_events, mut rx_events) = tokio::sync::mpsc::channel::<AppEvent>(8);

        // Some reconnect implementations can replay a snapshot-like PLACEMENT message for
        // an existing order with non-zero size_matched.
        let placement = r#"{
            "asset_id": "token",
            "event_type": "order",
            "id": "order-1",
            "timestamp": "1672290702",
            "type": "PLACEMENT",
            "side": "BUY",
            "price": "0.41",
            "original_size": "1.0",
            "size_matched": "0.5"
        }"#;

        let mut tracker = FillTracker::default();

        loop_
            .handle_text(placement, &tx_events, None, None, &mut tracker)
            .await
            .expect("handle_text ok");

        // We should only get a heartbeat; the cumulative size_matched must not be emitted as a fill.
        let first = timeout(Duration::from_millis(50), rx_events.recv())
            .await
            .expect("heartbeat should arrive")
            .expect("channel open");
        match first {
            AppEvent::UserWsUpdate(UserWsUpdate::Heartbeat { ts_ms }) => {
                assert_eq!(ts_ms, 1_672_290_702_000);
            }
            other => panic!("expected Heartbeat update, got {other:?}"),
        }

        let no_fill = timeout(Duration::from_millis(50), rx_events.recv()).await;
        assert!(no_fill.is_err(), "unexpected extra event: {no_fill:?}");
    }
}
