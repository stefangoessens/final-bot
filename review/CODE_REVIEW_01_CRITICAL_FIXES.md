# Code Review — Critical Fixes (must-do before production)

This file focuses on **highest-risk correctness and uptime issues** with concrete code patches.

---

## 1) Fix taker completion order construction (CRITICAL)

### Problem
- File: `src/clients/clob_rest.rs`
- Function: `ClobRestClient::build_completion_order`

Current code builds a BUY **market order** with:

```rust
.amount(Amount::shares(shares_dec))
```

Polymarket docs indicate **market BUY amount is in dollars** (SELL amount is shares).

So `Amount::shares(...)` for BUY is either rejected or semantically wrong.

### Fix (recommended): use a marketable limit with explicit worst-price cap

You already have a limit-order builder that sets `price` and `size` (shares). A “market order” on Polymarket is effectively a marketable limit + `order_type = FOK/FAK`.

**Patch:** Replace the market-order builder with a limit-order builder, keeping `order_type` = FOK/FAK and `price = p_max`.

#### Suggested patch (drop-in replacement)

```diff
diff --git a/src/clients/clob_rest.rs b/src/clients/clob_rest.rs
@@
     pub async fn build_completion_order(
         &self,
         token_id: &str,
         shares: f64,
         p_max: f64,
         order_type: CompletionOrderType,
     ) -> BotResult<SignedOrder> {
@@
-        let signable = inner
-            .client
-            .market_order()
-            .token_id(token_id)
-            .amount(Amount::shares(shares_dec))
-            .side(Side::Buy)
-            .order_type(map_completion_order_type(order_type))
-            .price(price_dec)
-            .build()
-            .await
-            .map_err(|e| BotError::Sdk(format!("build completion order: {e}")))?;
+        // Marketable limit (shares) with explicit worst-price cap.
+        // We use FOK/FAK so the order cannot sit on the book.
+        let signable = inner
+            .client
+            .limit_order()
+            .token_id(token_id)
+            .price(price_dec) // hard cap (worst price)
+            .size(shares_dec) // buy exactly N shares (or fail / partial if FAK)
+            .side(Side::Buy)
+            .order_type(map_completion_order_type(order_type)) // FOK/FAK
+            .post_only(false)
+            .build()
+            .await
+            .map_err(|e| BotError::Sdk(format!("build completion order: {e}")))?;
@@
         let signed = inner
             .client
             .sign_order(signable)
             .await
             .map_err(|e| BotError::Sdk(format!("sign completion order: {e}")))?;
```

**Acceptance criteria**
- The completion order request uses **size in shares** and `price = p_max`.
- The request sets `order_type` to `FOK` or `FAK`.
- Post-only is not set (or explicitly false).
- Unit test (suggested): validate the JSON payload the SDK would send, if SDK exposes it; otherwise do an integration test against test env.

---

## 2) Add RTDS keepalive PING (likely uptime issue)

### Problem
- File: `src/clients/rtds_ws.rs`
- RTDS docs recommend periodic `PING` to keep the connection alive.
- Current code handles websocket Ping/Pong frames but does not send a text `PING`.

### Fix: split stream and send `Message::Text("PING")` on interval

Here’s a realistic patch pattern.

```rust
use futures_util::{SinkExt, StreamExt};
use tokio::time::{interval, Duration};
use tokio_tungstenite::tungstenite::Message;

// inside your connection/session function, after you have `ws`:
let (mut ws_write, mut ws_read) = ws.split();
let mut ping = interval(Duration::from_secs(5));

loop {
    tokio::select! {
        _ = ping.tick() => {
            // Text PING keepalive (RTDS docs)
            if ws_write.send(Message::Text("PING".to_string())).await.is_err() {
                break;
            }
        }
        msg = ws_read.next() => {
            let Some(msg) = msg else { break; };
            let msg = msg?;
            match msg {
                Message::Text(txt) => {
                    if txt == "PONG" {
                        // optional: mark freshness / ignore
                        continue;
                    }
                    // existing JSON parsing path...
                }
                Message::Ping(payload) => {
                    let _ = ws_write.send(Message::Pong(payload)).await;
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    }
}
```

**Acceptance criteria**
- RTDS connection stays alive for hours without forced disconnects.
- Health `last_chainlink_ms` and/or `last_binance_ms` stays fresh under normal conditions.

---

## 3) Gate quoting until tick size is known (avoid startup rejects)

### Problem
- File: `src/state/state_manager.rs` + `src/strategy/quote_engine.rs`
- Quote engine falls back to `DEFAULT_TICK_SIZE` when tick size is unknown.
- Early in a market, you can place invalid prices and get rejects/backoff.

### Fix: add a block reason when tick sizes are missing

In `StateManager::recompute` (where you compute `block_reason`), add:

```rust
let tick_ok = out.up.tick_size > 0.0 && out.down.tick_size > 0.0;
let block_reason = if !tick_ok {
    Some("tick_size_unknown".to_string())
} else if !health.user_ws_fresh {
    Some("user_ws_stale".to_string())
} else if !health.market_ws_fresh {
    Some("market_ws_stale".to_string())
} else {
    None
};
```

And keep `quoting_enabled = alpha_ok && block_reason.is_none()` as-is.

**Acceptance criteria**
- No order placement attempts occur before tick sizes are seeded.
- Log should show `tick_size_unknown` briefly at startup then clear.

---

## 4) Add an “inventory desync watchdog” (safe correctness hardening)

### Problem
Inventory is updated from WS fill events. If you ever miss a fill, you can quote with the wrong skew and eat a directional move.

**Do not** blindly overwrite inventory from Data API on a timer — Data API snapshots can lag, causing you to “rewind” inventory.

### Fix: compare vs Data API; on mismatch -> cancel and shutdown

Create `src/inventory/desync_watchdog.rs`:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{watch, mpsc};
use tokio::time::{interval, Duration};

use crate::clients::data_api::DataApiClient;
use crate::state::market_state::MarketState;

pub struct DesyncWatchdogConfig {
    pub interval_s: u64,
    pub max_abs_shares_diff: f64, // e.g. 0.05
}

pub fn spawn_desync_watchdog(
    cfg: DesyncWatchdogConfig,
    mut rx: watch::Receiver<HashMap<String, MarketState>>,
    data: DataApiClient,
    data_api_user: String,
    fatal_tx: watch::Sender<Option<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut t = interval(Duration::from_secs(cfg.interval_s));
        loop {
            t.tick().await;

            let latest = rx.borrow().clone();
            if latest.is_empty() { continue; }

            // Build condition_id -> (slug, up_token, down_token, local_up, local_down)
            let mut meta = Vec::with_capacity(latest.len());
            for state in latest.values() {
                meta.push((
                    state.identity.condition_id.clone(),
                    state.identity.slug.clone(),
                    state.identity.token_up.clone(),
                    state.identity.token_down.clone(),
                    state.inventory.up.shares,
                    state.inventory.down.shares,
                ));
            }

            let conds: Vec<String> = meta.iter().map(|(c,_,_,_,_,_)| c.clone()).collect();
            let Ok(positions) = data.fetch_positions(&data_api_user, &conds).await else {
                // if Data API is down, don't hard fail here; rely on WS + health gating
                continue;
            };

            // condition_id -> token_id -> shares (authoritative snapshot)
            let mut snap: HashMap<(String,String), f64> = HashMap::new();
            for p in positions {
                snap.insert((p.condition_id.clone(), p.asset.clone()), p.size);
            }

            for (cond, slug, up_t, down_t, local_up, local_down) in meta {
                let api_up = *snap.get(&(cond.clone(), up_t)).unwrap_or(&0.0);
                let api_down = *snap.get(&(cond.clone(), down_t)).unwrap_or(&0.0);

                if (api_up - local_up).abs() > cfg.max_abs_shares_diff
                    || (api_down - local_down).abs() > cfg.max_abs_shares_diff
                {
                    let _ = fatal_tx.send(Some(format!(
                        "inventory_desync slug={slug} api_up={api_up:.4} local_up={local_up:.4} api_down={api_down:.4} local_down={local_down:.4}"
                    )));
                    break;
                }
            }
        }
    })
}
```

Then wire it in `main.rs` (after you build `DataApiClient` and `rx_quote_inventory` / `fatal_tx` exist).

**Acceptance criteria**
- If WS drops fills silently, watchdog triggers shutdown within `interval_s`.
- Normal operation: watchdog does nothing.

---

## 5) Don’t backoff on expected post-only rejects (reduce self-inflicted downtime)

### Problem
`OrderManager` applies a posting backoff on any failures. Some failures are **expected** in volatile conditions (post-only would-cross rejects). Those should *not* pause quoting.

### Fix: classify “expected” rejects and exclude them from backoff

In `src/execution/order_manager.rs`, wherever you compute `failures_count`, ignore rejected errors matching post-only / would-cross patterns.

Example helper:

```rust
fn is_expected_reject(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("post-only") || e.contains("post only") || e.contains("would cross") || e.contains("cross")
}
```

Then in your post result processing, count only unexpected ones.

**Acceptance criteria**
- Frequent post-only rejects don’t trigger multi-second backoff pauses.
- Truly bad errors (auth, nonce, invalid tick, 500s) still backoff.

---

## Summary checklist (minimum before production)
- [ ] Fix completion order (must).
- [ ] Add RTDS keepalive ping (likely).
- [ ] Gate quoting on tick size known (strongly recommended).
- [ ] Add desync watchdog (recommended).
- [ ] Classify expected rejects (recommended).
