use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::json;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::clients::clob_rest::{CancelResult, ClobRestClient, OrderId, OrderRequest};
use crate::clients::clob_ws_user::{UserOrderUpdate, UserOrderUpdateType};
use crate::config::TradingConfig;
use crate::error::{BotError, BotResult};
use crate::execution::batch;
use crate::persistence::LogEvent;
use crate::state::order_state::{LiveOrder, OrderStatus};
use crate::strategy::engine::ExecCommand;
use crate::strategy::DesiredOrder;

const EPS: f64 = 1e-12;
const BACKOFF_BASE_MS: u64 = 250;
const BACKOFF_MAX_MS: u64 = 5_000;

pub trait OrderRestClient: Send + Sync {
    fn build_signed_orders(
        &self,
        desired: Vec<DesiredOrder>,
        now_ms: i64,
    ) -> BoxFuture<'static, BotResult<Vec<OrderRequest>>>;
    fn post_orders(&self, orders: Vec<OrderRequest>) -> BoxFuture<'static, BotResult<Vec<OrderId>>>;
    fn cancel_orders(
        &self,
        order_ids: Vec<OrderId>,
    ) -> BoxFuture<'static, BotResult<CancelResult>>;
}

impl OrderRestClient for ClobRestClient {
    fn build_signed_orders(
        &self,
        desired: Vec<DesiredOrder>,
        now_ms: i64,
    ) -> BoxFuture<'static, BotResult<Vec<OrderRequest>>> {
        let client = self.clone();
        Box::pin(async move { client.build_signed_orders(desired, now_ms).await })
    }

    fn post_orders(&self, orders: Vec<OrderRequest>) -> BoxFuture<'static, BotResult<Vec<OrderId>>> {
        let client = self.clone();
        Box::pin(async move { client.post_orders(orders).await })
    }

    fn cancel_orders(
        &self,
        order_ids: Vec<OrderId>,
    ) -> BoxFuture<'static, BotResult<CancelResult>> {
        let client = self.clone();
        Box::pin(async move { client.cancel_orders(order_ids).await })
    }
}

#[derive(Debug, Clone)]
struct CancelAction {
    token_id: String,
    level: usize,
    order_id: String,
}

#[derive(Debug, Default)]
struct DiffPlan {
    cancels: Vec<CancelAction>,
    posts: Vec<DesiredOrder>,
}

impl DiffPlan {
    fn is_empty(&self) -> bool {
        self.cancels.is_empty() && self.posts.is_empty()
    }
}

#[derive(Default)]
struct MarketOrderCache {
    live: HashMap<(String, usize), LiveOrder>,
    last_update_ms: HashMap<(String, usize), i64>,
    tick_size: HashMap<String, f64>,
    backoff_until_ms: i64,
    backoff_ms: u64,
}

impl MarketOrderCache {
    fn update_tick_cache(&mut self, desired: &[DesiredOrder], ladder_step_ticks: u64) {
        if desired.len() < 2 || ladder_step_ticks == 0 {
            return;
        }

        let mut by_token: HashMap<String, Vec<&DesiredOrder>> = HashMap::new();
        for order in desired {
            by_token
                .entry(order.token_id.clone())
                .or_default()
                .push(order);
        }

        for (token_id, mut orders) in by_token {
            if orders.len() < 2 {
                continue;
            }
            orders.sort_by_key(|order| order.level);
            let first = orders[0];
            let second = orders[1];
            let level_diff = second.level.saturating_sub(first.level);
            if level_diff == 0 {
                continue;
            }
            let step_ticks = ladder_step_ticks.saturating_mul(level_diff as u64);
            if step_ticks == 0 {
                continue;
            }

            let price_diff = (first.price - second.price).abs();
            if price_diff <= 0.0 {
                continue;
            }
            let tick = price_diff / step_ticks as f64;
            if tick.is_finite() && tick > 0.0 {
                self.tick_size.insert(token_id, tick);
            }
        }
    }

    fn apply_dry_run(&mut self, plan: &DiffPlan, now_ms: i64) {
        for cancel in &plan.cancels {
            let key = (cancel.token_id.clone(), cancel.level);
            self.live.remove(&key);
            self.last_update_ms.remove(&key);
        }

        for post in &plan.posts {
            let key = (post.token_id.clone(), post.level);
            let order_id = format!("dry-{}-{}-{}", post.token_id, post.level, now_ms);
            self.live.insert(
                key.clone(),
                LiveOrder {
                    order_id,
                    token_id: post.token_id.clone(),
                    level: post.level,
                    price: post.price,
                    size: post.size,
                    remaining: post.size,
                    status: OrderStatus::Open,
                    last_update_ms: now_ms,
                },
            );
            self.last_update_ms.insert(key, now_ms);
        }
    }

    fn find_key_by_order_id(&self, order_id: &str) -> Option<(String, usize)> {
        self.live
            .iter()
            .find_map(|(key, order)| {
                if order.order_id == order_id {
                    Some(key.clone())
                } else {
                    None
                }
            })
    }
}

pub struct OrderManager {
    cfg: TradingConfig,
    rest: Arc<dyn OrderRestClient>,
    markets: HashMap<String, MarketOrderCache>,
    owner: Option<String>,
    user_orders: Option<Receiver<UserOrderUpdate>>,
}

impl OrderManager {
    pub fn new(
        cfg: TradingConfig,
        rest: ClobRestClient,
        user_orders: Receiver<UserOrderUpdate>,
    ) -> Self {
        Self::with_client(cfg, Arc::new(rest), user_orders)
    }

    pub fn with_client(
        cfg: TradingConfig,
        rest: Arc<dyn OrderRestClient>,
        user_orders: Receiver<UserOrderUpdate>,
    ) -> Self {
        Self {
            cfg,
            rest,
            markets: HashMap::new(),
            owner: default_owner_from_env(),
            user_orders: Some(user_orders),
        }
    }

    pub async fn run(
        mut self,
        mut rx_exec: Receiver<ExecCommand>,
        log_tx: Option<Sender<LogEvent>>,
    ) {
        let mut user_orders = self.user_orders.take();
        loop {
            tokio::select! {
                Some(cmd) = rx_exec.recv() => {
                    self.handle_command_with_logger(cmd, log_tx.as_ref()).await;
                }
                update = async {
                    match &mut user_orders {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                }, if user_orders.is_some() => {
                    match update {
                        Some(update) => self.handle_user_order_update(update),
                        None => {
                            user_orders = None;
                        }
                    }
                }
                else => break,
            }
        }
    }

    async fn handle_command_with_logger(
        &mut self,
        cmd: ExecCommand,
        log_tx: Option<&Sender<LogEvent>>,
    ) {
        let now_ms = now_ms();
        let cache = self.markets.entry(cmd.slug.clone()).or_default();

        if cache.backoff_until_ms > now_ms {
            tracing::warn!(
                target: "order_manager",
                slug = %cmd.slug,
                backoff_until_ms = cache.backoff_until_ms,
                "rest backoff active; skipping execution"
            );
            return;
        }

        cache.update_tick_cache(&cmd.desired, self.cfg.ladder_step_ticks);

        let plan = diff_orders(
            &cmd.desired,
            &cache.live,
            &cache.tick_size,
            &self.cfg,
            now_ms,
            &cache.last_update_ms,
        );

        if plan.is_empty() {
            return;
        }

        log_plan(&cmd.slug, &plan, &cache.live);

        if self.cfg.dry_run {
            cache.apply_dry_run(&plan, now_ms);
            return;
        }

        if self.owner.as_deref().unwrap_or_default().is_empty() {
            tracing::warn!(
                target: "order_manager",
                slug = %cmd.slug,
                "owner api key missing; order placement may fail"
            );
        }

        match execute_plan(
            &*self.rest,
            &cmd.slug,
            cache,
            plan,
            now_ms,
            log_tx,
        )
        .await
        {
            Ok(()) => reset_backoff(cache),
            Err(err) => apply_backoff(cache, &cmd.slug, now_ms, err),
        }
    }

    fn handle_user_order_update(&mut self, update: UserOrderUpdate) {
        for (slug, cache) in self.markets.iter_mut() {
            if apply_user_order_update(cache, &update, slug) {
                return;
            }
        }

        tracing::debug!(
            target: "order_manager",
            order_id = %update.order_id,
            token_id = %update.token_id,
            "user order update did not match live cache"
        );
    }
}

async fn execute_plan(
    rest: &dyn OrderRestClient,
    slug: &str,
    cache: &mut MarketOrderCache,
    plan: DiffPlan,
    now_ms: i64,
    log_tx: Option<&Sender<LogEvent>>,
) -> BotResult<()> {
    let cancel_batches = batch::chunk(plan.cancels.clone(), batch::MAX_BATCH_ORDERS);
    let mut canceled_ids: HashSet<String> = HashSet::new();
    let mut failed_ids: HashSet<String> = HashSet::new();

    for cancel_batch in cancel_batches {
        let ids: Vec<OrderId> = cancel_batch
            .iter()
            .map(|cancel| cancel.order_id.clone())
            .collect();
        log_cancel_batch(log_tx, slug, &ids, now_ms);
        let result = rest.cancel_orders(ids).await?;

        for id in result.canceled {
            canceled_ids.insert(id);
        }
        for id in result.failed {
            failed_ids.insert(id);
        }
    }

    if !canceled_ids.is_empty() || !failed_ids.is_empty() {
        log_cancel_result(
            log_tx,
            slug,
            &canceled_ids,
            &failed_ids,
            now_ms,
        );
    }

    if !failed_ids.is_empty() {
        tracing::warn!(
            target: "order_manager",
            slug = %slug,
            failed = failed_ids.len(),
            "some cancel requests failed"
        );
    }

    for cancel in &plan.cancels {
        if canceled_ids.contains(&cancel.order_id) {
            let key = (cancel.token_id.clone(), cancel.level);
            cache.live.remove(&key);
            cache.last_update_ms.remove(&key);
            tracing::info!(
                target: "order_manager",
                slug = %slug,
                token_id = %cancel.token_id,
                level = cancel.level,
                order_id = %cancel.order_id,
                action = "cancel_sent",
                "cancel acknowledged"
            );
        }
    }

    let mut posts = plan.posts;
    posts.retain(|post| !cache.live.contains_key(&(post.token_id.clone(), post.level)));

    if posts.is_empty() {
        return Ok(());
    }

    let post_batches = batch::chunk(posts, batch::MAX_BATCH_ORDERS);

    for post_batch in post_batches {
        log_post_batch(log_tx, slug, &post_batch, now_ms);
        let signed = rest
            .build_signed_orders(post_batch.clone(), now_ms)
            .await?;
        let order_ids = rest.post_orders(signed).await?;
        if order_ids.len() != post_batch.len() {
            return Err(BotError::Other(format!(
                "post_orders returned {} ids for {} orders",
                order_ids.len(),
                post_batch.len()
            )));
        }

        log_post_result(log_tx, slug, &order_ids, now_ms);
        for (post, order_id) in post_batch.into_iter().zip(order_ids.into_iter()) {
            let key = (post.token_id.clone(), post.level);
            cache.live.insert(
                key.clone(),
                LiveOrder {
                order_id: order_id.clone(),
                token_id: post.token_id.clone(),
                level: post.level,
                price: post.price,
                size: post.size,
                    remaining: post.size,
                    status: OrderStatus::Open,
                    last_update_ms: now_ms,
                },
            );
            cache.last_update_ms.insert(key, now_ms);
            tracing::info!(
                target: "order_manager",
                slug = %slug,
                token_id = %post.token_id,
                level = post.level,
                price = post.price,
                size = post.size,
                order_id = %order_id,
                action = "post_sent",
                "order posted"
            );
        }
    }

    Ok(())
}

fn apply_user_order_update(
    cache: &mut MarketOrderCache,
    update: &UserOrderUpdate,
    slug: &str,
) -> bool {
    let key = match cache.find_key_by_order_id(&update.order_id) {
        Some(key) => key,
        None => return false,
    };

    if let Some(mut live) = cache.live.remove(&key) {
        if matches!(update.update_type, UserOrderUpdateType::Cancellation) {
            cache.last_update_ms.remove(&key);
            tracing::info!(
                target: "order_manager",
                slug = %slug,
                token_id = %live.token_id,
                level = live.level,
                order_id = %live.order_id,
                action = "user_cancel",
                "order canceled via user update"
            );
            return true;
        }

        let new_size = if update.original_size.is_finite() && update.original_size > 0.0 {
            update.original_size
        } else {
            live.size
        };
        let matched = if update.size_matched.is_finite() && update.size_matched >= 0.0 {
            update.size_matched
        } else {
            0.0
        };
        let remaining = if new_size.is_finite() && new_size > 0.0 {
            (new_size - matched).max(0.0)
        } else {
            live.remaining
        };
        let price = if update.price.is_finite() && update.price > 0.0 {
            update.price
        } else {
            live.price
        };

        live.size = new_size;
        live.remaining = remaining;
        live.price = price;
        live.last_update_ms = update.ts_ms;
        live.status = if remaining <= 0.0 {
            OrderStatus::Filled
        } else if matched > 0.0 {
            OrderStatus::PartiallyFilled
        } else {
            OrderStatus::Open
        };

        if remaining <= 0.0 {
            cache.last_update_ms.remove(&key);
            tracing::info!(
                target: "order_manager",
                slug = %slug,
                token_id = %live.token_id,
                level = live.level,
                order_id = %live.order_id,
                matched,
                action = "user_filled",
                "order fully filled via user update"
            );
            return true;
        }

        tracing::info!(
            target: "order_manager",
            slug = %slug,
            token_id = %live.token_id,
            level = live.level,
            order_id = %live.order_id,
            remaining,
            matched,
            action = "user_update",
            "order updated via user update"
        );
        cache.live.insert(key, live);
        return true;
    }

    false
}

fn apply_backoff(cache: &mut MarketOrderCache, slug: &str, now_ms: i64, err: BotError) {
    let next = if cache.backoff_ms == 0 {
        BACKOFF_BASE_MS
    } else {
        (cache.backoff_ms * 2).min(BACKOFF_MAX_MS)
    };
    cache.backoff_ms = next;
    cache.backoff_until_ms = now_ms + next as i64;

    tracing::warn!(
        target: "order_manager",
        slug = %slug,
        backoff_ms = next,
        backoff_until_ms = cache.backoff_until_ms,
        error = %err,
        "rest error; backing off"
    );
}

fn reset_backoff(cache: &mut MarketOrderCache) {
    cache.backoff_ms = 0;
    cache.backoff_until_ms = 0;
}

fn default_owner_from_env() -> Option<String> {
    std::env::var("PMMB_KEYS__CLOB_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn diff_orders(
    desired: &[DesiredOrder],
    live: &HashMap<(String, usize), LiveOrder>,
    tick_size: &HashMap<String, f64>,
    cfg: &TradingConfig,
    now_ms: i64,
    last_update_ms: &HashMap<(String, usize), i64>,
) -> DiffPlan {
    let mut desired_map: HashMap<(String, usize), DesiredOrder> = HashMap::new();
    for order in desired {
        desired_map.insert((order.token_id.clone(), order.level), order.clone());
    }

    let mut plan = DiffPlan::default();
    let mut cancel_ids: HashSet<String> = HashSet::new();
    let mut post_keys: HashSet<(String, usize)> = HashSet::new();

    for (key, desired_order) in &desired_map {
        match live.get(key) {
            None => {
                if post_keys.insert(key.clone()) {
                    plan.posts.push(desired_order.clone());
                }
            }
            Some(live_order) => {
                let tick = tick_size
                    .get(&desired_order.token_id)
                    .copied()
                    .unwrap_or(0.0);
                if !needs_update(desired_order, live_order, tick, cfg) {
                    continue;
                }

                if is_debounced(
                    key,
                    live_order,
                    last_update_ms,
                    now_ms,
                    cfg.min_update_interval_ms,
                ) {
                    continue;
                }

                if cancel_ids.insert(live_order.order_id.clone()) {
                    plan.cancels.push(CancelAction {
                        token_id: live_order.token_id.clone(),
                        level: live_order.level,
                        order_id: live_order.order_id.clone(),
                    });
                }
                if post_keys.insert(key.clone()) {
                    plan.posts.push(desired_order.clone());
                }
            }
        }
    }

    for (key, live_order) in live {
        if desired_map.contains_key(key) {
            continue;
        }
        if cancel_ids.insert(live_order.order_id.clone()) {
            plan.cancels.push(CancelAction {
                token_id: live_order.token_id.clone(),
                level: live_order.level,
                order_id: live_order.order_id.clone(),
            });
        }
    }

    plan
}

fn needs_update(desired: &DesiredOrder, live: &LiveOrder, tick: f64, cfg: &TradingConfig) -> bool {
    let price_update = if tick > 0.0 && desired.price.is_finite() && live.price.is_finite() {
        let delta = (desired.price - live.price).abs();
        let threshold = cfg.reprice_min_ticks as f64 * tick;
        if threshold <= 0.0 {
            delta > EPS
        } else {
            delta + EPS >= threshold
        }
    } else {
        false
    };

    let live_size = effective_live_size(live);
    let size_update = if live_size > 0.0 && live_size.is_finite() && desired.size.is_finite() {
        let pct = (desired.size - live_size).abs() / live_size;
        if cfg.resize_min_pct <= 0.0 {
            pct > EPS
        } else {
            pct + EPS >= cfg.resize_min_pct
        }
    } else {
        true
    };

    price_update || size_update
}

fn is_debounced(
    key: &(String, usize),
    live: &LiveOrder,
    last_update_ms: &HashMap<(String, usize), i64>,
    now_ms: i64,
    min_update_interval_ms: u64,
) -> bool {
    if min_update_interval_ms == 0 {
        return false;
    }
    let last = last_update_ms
        .get(key)
        .copied()
        .unwrap_or(live.last_update_ms);
    if last <= 0 {
        return false;
    }
    now_ms.saturating_sub(last) < min_update_interval_ms as i64
}

fn effective_live_size(live: &LiveOrder) -> f64 {
    if live.remaining.is_finite() && live.remaining > 0.0 {
        live.remaining
    } else if live.size.is_finite() && live.size > 0.0 {
        live.size
    } else {
        0.0
    }
}

fn log_plan(slug: &str, plan: &DiffPlan, live: &HashMap<(String, usize), LiveOrder>) {
    let cancel_batches = batch::chunk(
        plan.cancels.iter().collect::<Vec<_>>(),
        batch::MAX_BATCH_ORDERS,
    );
    for (batch_idx, batch) in cancel_batches.into_iter().enumerate() {
        tracing::debug!(
            target: "order_manager",
            slug = %slug,
            batch_idx,
            batch_len = batch.len(),
            action = "cancel_batch",
            "planned cancel batch"
        );
        for cancel in batch {
            let key = (cancel.token_id.clone(), cancel.level);
            let (price, size) = live
                .get(&key)
                .map(|order| (order.price, effective_live_size(order)))
                .unwrap_or((0.0, 0.0));
            tracing::info!(
                target: "order_manager",
                slug = %slug,
                token_id = %cancel.token_id,
                level = cancel.level,
                price,
                size,
                order_id = %cancel.order_id,
                action = "cancel",
                "planned cancel"
            );
        }
    }

    let post_batches = batch::chunk(
        plan.posts.iter().collect::<Vec<_>>(),
        batch::MAX_BATCH_ORDERS,
    );
    for (batch_idx, batch) in post_batches.into_iter().enumerate() {
        tracing::debug!(
            target: "order_manager",
            slug = %slug,
            batch_idx,
            batch_len = batch.len(),
            action = "post_batch",
            "planned post batch"
        );
        for post in batch {
            tracing::info!(
                target: "order_manager",
                slug = %slug,
                token_id = %post.token_id,
                level = post.level,
                price = post.price,
                size = post.size,
                order_id = "",
                action = "post",
                "planned post"
            );
        }
    }
}

fn log_cancel_batch(
    log_tx: Option<&Sender<LogEvent>>,
    slug: &str,
    order_ids: &[OrderId],
    ts_ms: i64,
) {
    let Some(tx) = log_tx else {
        return;
    };
    let payload = json!({
        "slug": slug,
        "order_ids": order_ids,
        "count": order_ids.len(),
    });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: "order.cancel_batch".to_string(),
        payload,
    });
}

fn log_cancel_result(
    log_tx: Option<&Sender<LogEvent>>,
    slug: &str,
    canceled: &HashSet<String>,
    failed: &HashSet<String>,
    ts_ms: i64,
) {
    let Some(tx) = log_tx else {
        return;
    };
    let payload = json!({
        "slug": slug,
        "canceled": canceled.iter().collect::<Vec<_>>(),
        "failed": failed.iter().collect::<Vec<_>>(),
    });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: "order.cancel_result".to_string(),
        payload,
    });
}

fn log_post_batch(
    log_tx: Option<&Sender<LogEvent>>,
    slug: &str,
    posts: &[DesiredOrder],
    ts_ms: i64,
) {
    let Some(tx) = log_tx else {
        return;
    };
    let orders: Vec<_> = posts
        .iter()
        .map(|post| {
            json!({
                "token_id": post.token_id,
                "level": post.level,
                "price": post.price,
                "size": post.size,
                "post_only": post.post_only,
            })
        })
        .collect();
    let payload = json!({
        "slug": slug,
        "count": posts.len(),
        "orders": orders,
    });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: "order.post_batch".to_string(),
        payload,
    });
}

fn log_post_result(
    log_tx: Option<&Sender<LogEvent>>,
    slug: &str,
    order_ids: &[OrderId],
    ts_ms: i64,
) {
    let Some(tx) = log_tx else {
        return;
    };
    let payload = json!({
        "slug": slug,
        "order_ids": order_ids,
        "count": order_ids.len(),
    });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: "order.post_result".to_string(),
        payload,
    });
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use tokio::sync::mpsc;

    use crate::strategy::TimeInForce;

    #[derive(Clone, Default)]
    struct MockRestClient {
        calls: Arc<Mutex<Vec<&'static str>>>,
        next_cancel: Arc<Mutex<Vec<CancelResult>>>,
        next_post: Arc<Mutex<Vec<Vec<OrderId>>>>,
    }

    impl MockRestClient {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                next_cancel: Arc::new(Mutex::new(Vec::new())),
                next_post: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn push_cancel_result(&self, result: CancelResult) {
            self.next_cancel.lock().unwrap().push(result);
        }

        fn push_post_result(&self, result: Vec<OrderId>) {
            self.next_post.lock().unwrap().push(result);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl OrderRestClient for MockRestClient {
        fn build_signed_orders(
            &self,
            _desired: Vec<DesiredOrder>,
            _now_ms: i64,
        ) -> BoxFuture<'static, BotResult<Vec<OrderRequest>>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                calls.lock().unwrap().push("sign");
                Ok(Vec::new())
            })
        }

        fn post_orders(
            &self,
            _orders: Vec<OrderRequest>,
        ) -> BoxFuture<'static, BotResult<Vec<OrderId>>> {
            let calls = self.calls.clone();
            let next_post = self.next_post.clone();
            Box::pin(async move {
                calls.lock().unwrap().push("post");
                let result = next_post
                    .lock()
                    .unwrap()
                    .pop()
                    .unwrap_or_else(Vec::new);
                Ok(result)
            })
        }

        fn cancel_orders(
            &self,
            _order_ids: Vec<OrderId>,
        ) -> BoxFuture<'static, BotResult<CancelResult>> {
            let calls = self.calls.clone();
            let next_cancel = self.next_cancel.clone();
            Box::pin(async move {
                calls.lock().unwrap().push("cancel");
                let result = next_cancel
                    .lock()
                    .unwrap()
                    .pop()
                    .unwrap_or_default();
                Ok(result)
            })
        }
    }

    #[test]
    fn price_change_over_threshold_triggers_replace() {
        let cfg = TradingConfig {
            reprice_min_ticks: 2,
            resize_min_pct: 1.0,
            ..TradingConfig::default()
        };

        let desired = vec![DesiredOrder {
            token_id: "token".to_string(),
            level: 0,
            price: 0.52,
            size: 10.0,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        let mut live = HashMap::new();
        live.insert(
            ("token".to_string(), 0),
            LiveOrder {
                order_id: "order-1".to_string(),
                token_id: "token".to_string(),
                level: 0,
                price: 0.50,
                size: 10.0,
                remaining: 10.0,
                status: OrderStatus::Open,
                last_update_ms: 0,
            },
        );

        let mut ticks = HashMap::new();
        ticks.insert("token".to_string(), 0.01);

        let plan = diff_orders(&desired, &live, &ticks, &cfg, 1_000, &HashMap::new());

        assert_eq!(plan.cancels.len(), 1);
        assert_eq!(plan.cancels[0].order_id, "order-1");
        assert_eq!(plan.posts.len(), 1);
        assert!((plan.posts[0].price - 0.52).abs() < 1e-12);
    }

    #[test]
    fn small_size_change_skips_replace() {
        let cfg = TradingConfig {
            reprice_min_ticks: 1,
            resize_min_pct: 0.10,
            ..TradingConfig::default()
        };

        let desired = vec![DesiredOrder {
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 10.5,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        let mut live = HashMap::new();
        live.insert(
            ("token".to_string(), 0),
            LiveOrder {
                order_id: "order-2".to_string(),
                token_id: "token".to_string(),
                level: 0,
                price: 0.50,
                size: 10.0,
                remaining: 10.0,
                status: OrderStatus::Open,
                last_update_ms: 0,
            },
        );

        let mut ticks = HashMap::new();
        ticks.insert("token".to_string(), 0.01);

        let plan = diff_orders(&desired, &live, &ticks, &cfg, 1_000, &HashMap::new());

        assert!(plan.cancels.is_empty());
        assert!(plan.posts.is_empty());
    }

    #[tokio::test]
    async fn cancel_happens_before_post() {
        let mut cfg = TradingConfig::default();
        cfg.dry_run = false;
        cfg.resize_min_pct = 0.0;
        cfg.min_update_interval_ms = 0;

        let mock = MockRestClient::new();
        let (_tx_user_orders, rx_user_orders) = mpsc::channel(1);
        mock.push_cancel_result(CancelResult {
            canceled: vec!["order-old".to_string()],
            failed: Vec::new(),
        });
        mock.push_post_result(vec!["order-new".to_string()]);

        let mut manager = OrderManager::with_client(cfg, Arc::new(mock.clone()), rx_user_orders);
        let slug = "btc-15m-test".to_string();
        let cache = manager.markets.entry(slug.clone()).or_default();
        cache.live.insert(
            ("token".to_string(), 0),
            LiveOrder {
                order_id: "order-old".to_string(),
                token_id: "token".to_string(),
                level: 0,
                price: 0.50,
                size: 10.0,
                remaining: 10.0,
                status: OrderStatus::Open,
                last_update_ms: 0,
            },
        );

        let desired = vec![DesiredOrder {
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 12.0,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        manager
            .handle_command_with_logger(
                ExecCommand { slug, desired },
                None,
            )
            .await;

        let calls = mock.calls();
        assert_eq!(calls, vec!["cancel", "sign", "post"]);
    }

    #[tokio::test]
    async fn cache_updates_from_post_response() {
        let mut cfg = TradingConfig::default();
        cfg.dry_run = false;
        cfg.min_update_interval_ms = 0;

        let mock = MockRestClient::new();
        let (_tx_user_orders, rx_user_orders) = mpsc::channel(1);
        mock.push_post_result(vec!["order-123".to_string()]);

        let mut manager = OrderManager::with_client(cfg, Arc::new(mock), rx_user_orders);
        let slug = "btc-15m-test".to_string();

        let desired = vec![DesiredOrder {
            token_id: "token".to_string(),
            level: 0,
            price: 0.42,
            size: 5.0,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        manager
            .handle_command_with_logger(
                ExecCommand {
                    slug: slug.clone(),
                    desired,
                },
                None,
            )
            .await;

        let cache = manager.markets.get(&slug).expect("cache exists");
        let live = cache
            .live
            .get(&("token".to_string(), 0))
            .expect("order stored");
        assert_eq!(live.order_id, "order-123");
        assert_eq!(live.price, 0.42);
        assert_eq!(live.size, 5.0);
    }

    #[test]
    fn user_update_replenish_after_partial_fill() {
        let mut cfg = TradingConfig::default();
        cfg.resize_min_pct = 0.05;
        cfg.min_update_interval_ms = 0;

        let mut cache = MarketOrderCache::default();
        cache.live.insert(
            ("token".to_string(), 0),
            LiveOrder {
                order_id: "order-1".to_string(),
                token_id: "token".to_string(),
                level: 0,
                price: 0.50,
                size: 10.0,
                remaining: 10.0,
                status: OrderStatus::Open,
                last_update_ms: 0,
            },
        );

        let update = UserOrderUpdate {
            order_id: "order-1".to_string(),
            token_id: "token".to_string(),
            price: 0.50,
            original_size: 10.0,
            size_matched: 5.0,
            ts_ms: 1_000,
            update_type: UserOrderUpdateType::Update,
        };

        assert!(apply_user_order_update(&mut cache, &update, "btc-15m-test"));

        let desired = vec![DesiredOrder {
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 10.0,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        let mut ticks = HashMap::new();
        ticks.insert("token".to_string(), 0.01);

        let plan = diff_orders(
            &desired,
            &cache.live,
            &ticks,
            &cfg,
            2_000,
            &cache.last_update_ms,
        );

        assert_eq!(plan.cancels.len(), 1);
        assert_eq!(plan.posts.len(), 1);
    }
}
