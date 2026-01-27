use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::json;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::clients::clob_rest::{
    CancelResult, ClobRestClient, OrderId, OrderRequest, PostOrdersResult,
};
use crate::clients::clob_ws_user::{UserOrderUpdate, UserOrderUpdateType};
use crate::config::TradingConfig;
use crate::error::{BotError, BotResult};
use crate::execution::batch;
use crate::persistence::LogEvent;
use crate::state::order_state::{LiveOrder, OrderStatus};
use crate::state::state_manager::{AppEvent, OrderUpdate};
use crate::strategy::engine::ExecCommand;
use crate::strategy::DesiredOrder;
use crate::time::BTC_15M_INTERVAL_S;

const EPS: f64 = 1e-12;
const BACKOFF_BASE_MS: u64 = 250;
const BACKOFF_MAX_MS: u64 = 5_000;
const UNKNOWN_USER_UPDATE_WARN_INTERVAL_MS: i64 = 30_000;
const UNKNOWN_USER_UPDATE_CANCEL_INTERVAL_MS: i64 = 10_000;

#[derive(Debug, Default)]
struct UnknownUserUpdateLimiter {
    last_warn_ms_by_token: HashMap<String, i64>,
    last_cancel_ms_by_order: HashMap<String, i64>,
}

impl UnknownUserUpdateLimiter {
    fn should_warn(&mut self, token_id: &str, now_ms: i64) -> bool {
        if token_id.is_empty() {
            return false;
        }
        if let Some(last) = self.last_warn_ms_by_token.get(token_id) {
            if now_ms.saturating_sub(*last) < UNKNOWN_USER_UPDATE_WARN_INTERVAL_MS {
                return false;
            }
        }
        self.last_warn_ms_by_token
            .insert(token_id.to_string(), now_ms);
        true
    }

    fn should_cancel(&mut self, order_id: &str, now_ms: i64) -> bool {
        if order_id.is_empty() {
            return false;
        }
        if let Some(last) = self.last_cancel_ms_by_order.get(order_id) {
            if now_ms.saturating_sub(*last) < UNKNOWN_USER_UPDATE_CANCEL_INTERVAL_MS {
                return false;
            }
        }
        self.last_cancel_ms_by_order
            .insert(order_id.to_string(), now_ms);
        true
    }
}

trait OrderRestClient: Send + Sync {
    fn build_signed_orders(
        &self,
        desired: Vec<DesiredOrder>,
        now_ms: i64,
    ) -> BoxFuture<'static, BotResult<Vec<OrderRequest>>>;
    fn post_orders_partial(
        &self,
        orders: Vec<OrderRequest>,
    ) -> BoxFuture<'static, BotResult<PostOutcome>>;
    fn cancel_orders(&self, order_ids: Vec<OrderId>)
        -> BoxFuture<'static, BotResult<CancelResult>>;
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

    fn post_orders_partial(
        &self,
        orders: Vec<OrderRequest>,
    ) -> BoxFuture<'static, BotResult<PostOutcome>> {
        let client = self.clone();
        Box::pin(async move {
            let expected = orders.len();
            let result: PostOrdersResult = client.post_orders_result(orders).await?;
            let mut results = Vec::with_capacity(expected);
            results.resize_with(expected, || PostResult {
                order_id: None,
                error: Some("missing response".to_string()),
            });

            apply_post_orders_result(&mut results, expected, result);
            Ok(PostOutcome { results })
        })
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
    order_id_index: HashMap<String, (String, usize)>,
    tracked_tokens: HashSet<String>,
    tick_size: HashMap<String, f64>,
    backoff_until_ms: i64,
    backoff_ms: u64,
}

impl MarketOrderCache {
    fn insert_live(&mut self, order: LiveOrder) {
        let key = (order.token_id.clone(), order.level);
        self.order_id_index
            .insert(order.order_id.clone(), key.clone());
        self.last_update_ms
            .insert(key.clone(), order.last_update_ms);
        self.tracked_tokens.insert(order.token_id.clone());
        self.live.insert(key, order);
    }

    fn clear(&mut self) {
        self.live.clear();
        self.last_update_ms.clear();
        self.order_id_index.clear();
        self.tracked_tokens.clear();
    }

    fn remove_live_by_key(&mut self, key: &(String, usize)) -> Option<LiveOrder> {
        let live = self.live.remove(key);
        if let Some(order) = &live {
            self.order_id_index.remove(&order.order_id);
        }
        self.last_update_ms.remove(key);
        live
    }

    fn key_for_order_id(&self, order_id: &str) -> Option<(String, usize)> {
        self.order_id_index.get(order_id).cloned()
    }

    fn tracks_token(&self, token_id: &str) -> bool {
        !token_id.is_empty() && self.tracked_tokens.contains(token_id)
    }

    fn update_tick_cache(&mut self, desired: &[DesiredOrder], ladder_step_ticks: u64) {
        for order in desired {
            self.tracked_tokens.insert(order.token_id.clone());
        }

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
            self.remove_live_by_key(&key);
        }

        for post in &plan.posts {
            let order_id = format!("dry-{}-{}-{}", post.token_id, post.level, now_ms);
            self.insert_live(LiveOrder {
                order_id,
                token_id: post.token_id.clone(),
                level: post.level,
                price: post.price,
                size: post.size,
                remaining: post.size,
                status: OrderStatus::Open,
                last_update_ms: now_ms,
            });
        }
    }
}

pub struct OrderManager {
    cfg: TradingConfig,
    rest: Arc<dyn OrderRestClient>,
    markets: HashMap<String, MarketOrderCache>,
    owner: Option<String>,
    user_orders: Option<Receiver<UserOrderUpdate>>,
    unknown_update_limiter: UnknownUserUpdateLimiter,
}

impl OrderManager {
    pub fn new(
        cfg: TradingConfig,
        rest: ClobRestClient,
        user_orders: Receiver<UserOrderUpdate>,
    ) -> Self {
        Self::with_client(cfg, Arc::new(rest), user_orders)
    }

    fn with_client(
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
            unknown_update_limiter: UnknownUserUpdateLimiter::default(),
        }
    }

    pub fn seed_live_orders(&mut self, slug: &str, orders: Vec<LiveOrder>) {
        let cache = self.markets.entry(slug.to_string()).or_default();
        cache.clear();
        for order in orders {
            cache.insert_live(order);
        }
        tracing::info!(
            target: "order_manager",
            slug = %slug,
            live_orders = cache.live.len(),
            "seeded live orders from reconciliation"
        );
    }

    pub async fn run(
        mut self,
        mut rx_exec: Receiver<ExecCommand>,
        tx_events: Sender<AppEvent>,
        log_tx: Option<Sender<LogEvent>>,
    ) {
        let mut user_orders = self.user_orders.take();
        loop {
            tokio::select! {
                Some(cmd) = rx_exec.recv() => {
                    self.handle_command_with_logger(cmd, Some(&tx_events), log_tx.as_ref()).await;
                }
                update = async {
                    match &mut user_orders {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                }, if user_orders.is_some() => {
                    match update {
                        Some(update) => self.handle_user_order_update(update, Some(&tx_events)),
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
        tx_events: Option<&Sender<AppEvent>>,
        log_tx: Option<&Sender<LogEvent>>,
    ) {
        let now_ms = now_ms();
        let cache = self.markets.entry(cmd.slug.clone()).or_default();
        let post_backoff_active = cache.backoff_until_ms > now_ms;
        let cutoff_ts_ms = cutoff_ts_from_slug(&cmd.slug, self.cfg.cutoff_seconds);
        let cutoff_active = cutoff_ts_ms.map(|cutoff| now_ms >= cutoff).unwrap_or(true);

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
            emit_dry_run_updates(tx_events, &cmd.slug, &plan, now_ms);
            return;
        }

        if self.owner.as_deref().unwrap_or_default().is_empty() {
            tracing::warn!(
                target: "order_manager",
                slug = %cmd.slug,
                "owner api key missing; order placement may fail"
            );
        }

        if !plan.cancels.is_empty() {
            if let Err(err) = execute_cancels(
                &*self.rest,
                &cmd.slug,
                cache,
                &plan,
                now_ms,
                tx_events,
                log_tx,
            )
            .await
            {
                apply_backoff(cache, &cmd.slug, now_ms, err);
                return;
            }
        }

        let mut posts = plan.posts;
        posts.retain(|post| {
            !cache
                .live
                .contains_key(&(post.token_id.clone(), post.level))
        });

        if posts.is_empty() {
            return;
        }

        if post_backoff_active || cutoff_active {
            tracing::warn!(
                target: "order_manager",
                slug = %cmd.slug,
                post_count = posts.len(),
                backoff_active = post_backoff_active,
                backoff_until_ms = cache.backoff_until_ms,
                cutoff_active = cutoff_active,
                cutoff_ts_ms = cutoff_ts_ms.unwrap_or_default(),
                "post suppressed due to backoff/cutoff"
            );
            return;
        }

        match execute_posts(
            &*self.rest,
            &cmd.slug,
            cache,
            posts,
            now_ms,
            tx_events,
            log_tx,
        )
        .await
        {
            Ok(outcome) => {
                if outcome.failure_count() > 0 {
                    apply_backoff(
                        cache,
                        &cmd.slug,
                        now_ms,
                        BotError::Other(format!(
                            "partial post failures: {}",
                            outcome.failure_count()
                        )),
                    );
                } else {
                    reset_backoff(cache);
                }
            }
            Err(err) => apply_backoff(cache, &cmd.slug, now_ms, err),
        }
    }

    fn handle_user_order_update(
        &mut self,
        update: UserOrderUpdate,
        tx_events: Option<&Sender<AppEvent>>,
    ) {
        let mut tracked_slug: Option<String> = None;

        for (slug, cache) in self.markets.iter_mut() {
            if tracked_slug.is_none() && cache.tracks_token(&update.token_id) {
                tracked_slug = Some(slug.clone());
            }

            if let Some(order_update) = apply_user_order_update(cache, &update, slug) {
                emit_order_update(tx_events, order_update);
                return;
            }
        }

        let Some(slug) = tracked_slug else {
            tracing::debug!(
                target: "order_manager",
                order_id = %update.order_id,
                token_id = %update.token_id,
                update_type = ?update.update_type,
                "ignoring user order update for untracked token"
            );
            return;
        };

        let now_ms = if update.ts_ms > 0 {
            update.ts_ms
        } else {
            now_ms()
        };

        let is_potentially_live = matches!(
            update.update_type,
            UserOrderUpdateType::Placement | UserOrderUpdateType::Update
        );

        if is_potentially_live
            && self
                .unknown_update_limiter
                .should_warn(&update.token_id, now_ms)
        {
            tracing::warn!(
                target: "order_manager",
                slug = %slug,
                order_id = %update.order_id,
                token_id = %update.token_id,
                update_type = ?update.update_type,
                price = update.price.unwrap_or_default(),
                original_size = update.original_size.unwrap_or_default(),
                size_matched = update.size_matched.unwrap_or_default(),
                "user order update did not match live cache (possible desync)"
            );
        } else if !is_potentially_live {
            tracing::debug!(
                target: "order_manager",
                slug = %slug,
                order_id = %update.order_id,
                token_id = %update.token_id,
                update_type = ?update.update_type,
                "user order update did not match live cache"
            );
        }

        if self.cfg.dry_run {
            return;
        }

        if !is_potentially_live {
            return;
        }

        if !self
            .unknown_update_limiter
            .should_cancel(&update.order_id, now_ms)
        {
            return;
        }

        let rest = Arc::clone(&self.rest);
        let slug_for_log = slug.clone();
        let token_id = update.token_id.clone();
        let order_id = update.order_id.clone();

        tokio::spawn(async move {
            match rest.cancel_orders(vec![order_id.clone()]).await {
                Ok(result) => {
                    if result.canceled.iter().any(|id| id == &order_id) {
                        tracing::warn!(
                            target: "order_manager",
                            slug = %slug_for_log,
                            token_id = %token_id,
                            order_id = %order_id,
                            action = "cancel_unknown_order",
                            "canceled unknown order from user update"
                        );
                    } else if result.failed.iter().any(|id| id == &order_id) {
                        tracing::warn!(
                            target: "order_manager",
                            slug = %slug_for_log,
                            token_id = %token_id,
                            order_id = %order_id,
                            action = "cancel_unknown_order_failed",
                            "failed to cancel unknown order from user update"
                        );
                    } else {
                        tracing::info!(
                            target: "order_manager",
                            slug = %slug_for_log,
                            token_id = %token_id,
                            order_id = %order_id,
                            action = "cancel_unknown_order_noop",
                            "cancel attempt returned no status for unknown order"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        target: "order_manager",
                        slug = %slug_for_log,
                        token_id = %token_id,
                        order_id = %order_id,
                        error = %err,
                        action = "cancel_unknown_order_error",
                        "cancel attempt errored for unknown order"
                    );
                }
            }
        });
    }
}

async fn execute_cancels(
    rest: &dyn OrderRestClient,
    slug: &str,
    cache: &mut MarketOrderCache,
    plan: &DiffPlan,
    now_ms: i64,
    tx_events: Option<&Sender<AppEvent>>,
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
        log_cancel_result(log_tx, slug, &canceled_ids, &failed_ids, now_ms);
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
            cache.remove_live_by_key(&key);
            tracing::info!(
                target: "order_manager",
                slug = %slug,
                token_id = %cancel.token_id,
                level = cancel.level,
                order_id = %cancel.order_id,
                action = "cancel_sent",
                "cancel acknowledged"
            );
            emit_order_update(
                tx_events,
                OrderUpdate::Remove {
                    slug: slug.to_string(),
                    token_id: cancel.token_id.clone(),
                    level: cancel.level,
                    order_id: cancel.order_id.clone(),
                    ts_ms: now_ms,
                },
            );
        }
    }

    Ok(())
}

async fn execute_posts(
    rest: &dyn OrderRestClient,
    slug: &str,
    cache: &mut MarketOrderCache,
    posts: Vec<DesiredOrder>,
    now_ms: i64,
    tx_events: Option<&Sender<AppEvent>>,
    log_tx: Option<&Sender<LogEvent>>,
) -> BotResult<PostOutcome> {
    let post_batches = batch::chunk(posts, batch::MAX_BATCH_ORDERS);
    let mut all_results: Vec<PostResult> = Vec::new();

    for post_batch in post_batches {
        log_post_batch(log_tx, slug, &post_batch, now_ms);
        let signed = rest.build_signed_orders(post_batch.clone(), now_ms).await?;
        let outcome = rest.post_orders_partial(signed).await?;

        if outcome.results.len() != post_batch.len() {
            return Err(BotError::Other(format!(
                "post_orders returned {} results for {} orders",
                outcome.results.len(),
                post_batch.len()
            )));
        }

        log_post_result(log_tx, slug, &outcome, now_ms);

        for (post, result) in post_batch.into_iter().zip(outcome.results.iter()) {
            if let Some(order_id) = result.order_id.as_ref() {
                let live = LiveOrder {
                    order_id: order_id.clone(),
                    token_id: post.token_id.clone(),
                    level: post.level,
                    price: post.price,
                    size: post.size,
                    remaining: post.size,
                    status: OrderStatus::Open,
                    last_update_ms: now_ms,
                };
                cache.insert_live(live.clone());
                emit_order_update(
                    tx_events,
                    OrderUpdate::Upsert {
                        slug: slug.to_string(),
                        order: live,
                    },
                );
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
            } else {
                tracing::warn!(
                    target: "order_manager",
                    slug = %slug,
                    token_id = %post.token_id,
                    level = post.level,
                    price = post.price,
                    size = post.size,
                    error = result.error.as_deref().unwrap_or("unknown"),
                    action = "post_failed",
                    "order post failed"
                );
            }
        }

        all_results.extend(outcome.results.into_iter());
    }

    Ok(PostOutcome {
        results: all_results,
    })
}

fn apply_user_order_update(
    cache: &mut MarketOrderCache,
    update: &UserOrderUpdate,
    slug: &str,
) -> Option<OrderUpdate> {
    let key = cache.key_for_order_id(&update.order_id)?;

    if let Some(mut live) = cache.remove_live_by_key(&key) {
        if matches!(update.update_type, UserOrderUpdateType::Cancellation) {
            tracing::info!(
                target: "order_manager",
                slug = %slug,
                token_id = %live.token_id,
                level = live.level,
                order_id = %live.order_id,
                action = "user_cancel",
                "order canceled via user update"
            );
            return Some(OrderUpdate::Remove {
                slug: slug.to_string(),
                token_id: live.token_id.clone(),
                level: live.level,
                order_id: live.order_id.clone(),
                ts_ms: update.ts_ms,
            });
        }
        if matches!(update.update_type, UserOrderUpdateType::Rejection) {
            tracing::info!(
                target: "order_manager",
                slug = %slug,
                token_id = %live.token_id,
                level = live.level,
                order_id = %live.order_id,
                action = "user_reject",
                "order rejected via user update"
            );
            return Some(OrderUpdate::Remove {
                slug: slug.to_string(),
                token_id: live.token_id.clone(),
                level: live.level,
                order_id: live.order_id.clone(),
                ts_ms: update.ts_ms,
            });
        }

        let mut new_size = live.size;
        let mut matched = (live.size - live.remaining).max(0.0);
        let mut size_updated = false;
        let mut matched_updated = false;

        if let Some(size) = update.original_size.filter(|v| v.is_finite() && *v > 0.0) {
            new_size = size;
            size_updated = true;
        }

        if let Some(size) = update.size_matched.filter(|v| v.is_finite() && *v >= 0.0) {
            matched = size;
            matched_updated = true;
        }

        let remaining = if size_updated || matched_updated {
            (new_size - matched).max(0.0)
        } else {
            live.remaining
        };

        let price = update
            .price
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(live.price);

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
            return Some(OrderUpdate::Remove {
                slug: slug.to_string(),
                token_id: live.token_id.clone(),
                level: live.level,
                order_id: live.order_id.clone(),
                ts_ms: update.ts_ms,
            });
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
        cache.insert_live(live.clone());
        return Some(OrderUpdate::Upsert {
            slug: slug.to_string(),
            order: live,
        });
    }

    None
}

fn emit_dry_run_updates(
    tx_events: Option<&Sender<AppEvent>>,
    slug: &str,
    plan: &DiffPlan,
    now_ms: i64,
) {
    for cancel in &plan.cancels {
        emit_order_update(
            tx_events,
            OrderUpdate::Remove {
                slug: slug.to_string(),
                token_id: cancel.token_id.clone(),
                level: cancel.level,
                order_id: cancel.order_id.clone(),
                ts_ms: now_ms,
            },
        );
    }

    for post in &plan.posts {
        let order_id = format!("dry-{}-{}-{}", post.token_id, post.level, now_ms);
        emit_order_update(
            tx_events,
            OrderUpdate::Upsert {
                slug: slug.to_string(),
                order: LiveOrder {
                    order_id,
                    token_id: post.token_id.clone(),
                    level: post.level,
                    price: post.price,
                    size: post.size,
                    remaining: post.size,
                    status: OrderStatus::Open,
                    last_update_ms: now_ms,
                },
            },
        );
    }
}

// W7.15: push live order snapshots to StateManager (RewardEngine consumes level0 order_ids).
fn emit_order_update(tx_events: Option<&Sender<AppEvent>>, update: OrderUpdate) {
    let Some(tx) = tx_events else {
        return;
    };
    if let Err(err) = tx.try_send(AppEvent::OrderUpdate(update)) {
        match err {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                tracing::debug!(
                    target: "order_manager",
                    "state manager channel full; dropping order update"
                );
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                tracing::warn!(
                    target: "order_manager",
                    "state manager channel closed; dropping order update"
                );
            }
        }
    }
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
    outcome: &PostOutcome,
    ts_ms: i64,
) {
    let Some(tx) = log_tx else {
        return;
    };
    let order_ids: Vec<&OrderId> = outcome
        .results
        .iter()
        .filter_map(|result| result.order_id.as_ref())
        .collect();
    let failed: Vec<&str> = outcome
        .results
        .iter()
        .filter_map(|result| result.error.as_deref())
        .collect();
    let payload = json!({
        "slug": slug,
        "order_ids": order_ids,
        "count": order_ids.len(),
        "failed": failed,
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

fn cutoff_ts_from_slug(slug: &str, cutoff_seconds: i64) -> Option<i64> {
    let start_s = slug.rsplit('-').next()?.parse::<i64>().ok()?;
    let interval_s = BTC_15M_INTERVAL_S.max(0);
    let cutoff_s = start_s
        .saturating_add(interval_s)
        .saturating_sub(cutoff_seconds.max(0));
    Some(cutoff_s.saturating_mul(1_000))
}

fn apply_post_orders_result(results: &mut [PostResult], expected: usize, result: PostOrdersResult) {
    for accepted in result.accepted {
        if accepted.idx < expected {
            results[accepted.idx] = PostResult {
                order_id: Some(accepted.order_id),
                error: None,
            };
        } else {
            tracing::warn!(
                target: "order_manager",
                idx = accepted.idx,
                expected,
                order_id = %accepted.order_id,
                "post_orders returned accepted index out of range"
            );
        }
    }

    for rejected in result.rejected {
        if rejected.idx < expected {
            results[rejected.idx] = PostResult {
                order_id: rejected.order_id,
                error: Some(rejected.error),
            };
        } else {
            tracing::warn!(
                target: "order_manager",
                idx = rejected.idx,
                expected,
                order_id = rejected.order_id.as_deref().unwrap_or(""),
                error = %rejected.error,
                "post_orders returned rejected index out of range"
            );
        }
    }
}

fn is_expected_post_only_reject(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("post-only") || e.contains("post only") || e.contains("would cross")
}

#[derive(Debug, Clone, Default)]
struct PostOutcome {
    results: Vec<PostResult>,
}

impl PostOutcome {
    fn failure_count(&self) -> usize {
        self.results
            .iter()
            .filter(|result| {
                if result.order_id.is_some() && result.error.is_none() {
                    return false;
                }
                !matches!(
                    result.error.as_deref(),
                    Some(err) if is_expected_post_only_reject(err)
                )
            })
            .count()
    }
}

#[derive(Debug, Clone)]
struct PostResult {
    order_id: Option<OrderId>,
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    use crate::config::{CompletionConfig, InventoryConfig, RewardsConfig};
    use crate::state::market_state::{MarketIdentity, MarketState};
    use crate::state::state_manager::QuoteTick;
    use crate::strategy::engine::StrategyEngine;
    use crate::strategy::quote_engine::build_desired_orders;
    use crate::strategy::TimeInForce;

    fn future_slug() -> String {
        let start_s = now_ms() / 1_000 + BTC_15M_INTERVAL_S;
        format!("btc-updown-15m-{start_s}")
    }

    #[derive(Clone, Default)]
    struct MockRestClient {
        calls: Arc<Mutex<Vec<&'static str>>>,
        next_cancel: Arc<Mutex<Vec<CancelResult>>>,
        next_post: Arc<Mutex<Vec<Vec<PostResult>>>>,
    }

    #[derive(Clone)]
    struct ComplianceRestClient {
        expected: Arc<HashMap<String, (f64, f64)>>,
        min_ticks_from_ask: u64,
        last_batch_len: Arc<Mutex<usize>>,
        observed: Arc<Mutex<Vec<DesiredOrder>>>,
        counter: Arc<AtomicUsize>,
    }

    impl ComplianceRestClient {
        fn new(expected: HashMap<String, (f64, f64)>, min_ticks_from_ask: u64) -> Self {
            Self {
                expected: Arc::new(expected),
                min_ticks_from_ask,
                last_batch_len: Arc::new(Mutex::new(0)),
                observed: Arc::new(Mutex::new(Vec::new())),
                counter: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn observed(&self) -> Vec<DesiredOrder> {
            self.observed.lock().unwrap().clone()
        }
    }

    impl OrderRestClient for ComplianceRestClient {
        fn build_signed_orders(
            &self,
            desired: Vec<DesiredOrder>,
            _now_ms: i64,
        ) -> BoxFuture<'static, BotResult<Vec<OrderRequest>>> {
            let expected = self.expected.clone();
            let min_ticks_from_ask = self.min_ticks_from_ask;
            let observed = self.observed.clone();
            let last_batch_len = self.last_batch_len.clone();

            Box::pin(async move {
                *last_batch_len.lock().unwrap() = desired.len();
                observed.lock().unwrap().extend(desired.iter().cloned());

                for order in desired.iter() {
                    assert!(order.post_only, "maker-only violated: post_only=false");
                    assert_eq!(
                        order.tif,
                        TimeInForce::Gtc,
                        "maker-only violated: tif != GTC"
                    );
                    let (ask, tick) = expected
                        .get(&order.token_id)
                        .unwrap_or_else(|| panic!("missing ask/tick for {}", order.token_id));
                    let max_price = ask - min_ticks_from_ask as f64 * tick;
                    assert!(
                        order.price <= max_price + EPS,
                        "maker-only violated: price={} ask={} max_price={}",
                        order.price,
                        ask,
                        max_price
                    );
                }

                // Schema-specific fields are serialized by the SDK; we assert internal fields here.
                Ok(Vec::new())
            })
        }

        fn post_orders_partial(
            &self,
            _orders: Vec<OrderRequest>,
        ) -> BoxFuture<'static, BotResult<PostOutcome>> {
            let last_batch_len = self.last_batch_len.clone();
            let counter = self.counter.clone();
            Box::pin(async move {
                let len = *last_batch_len.lock().unwrap();
                let start = counter.fetch_add(len, Ordering::SeqCst);
                let results = (0..len)
                    .map(|idx| PostResult {
                        order_id: Some(format!("order-{}", start + idx)),
                        error: None,
                    })
                    .collect();
                Ok(PostOutcome { results })
            })
        }

        fn cancel_orders(
            &self,
            _order_ids: Vec<OrderId>,
        ) -> BoxFuture<'static, BotResult<CancelResult>> {
            Box::pin(async { Ok(CancelResult::default()) })
        }
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
            let results = result
                .into_iter()
                .map(|order_id| PostResult {
                    order_id: Some(order_id),
                    error: None,
                })
                .collect();
            self.next_post.lock().unwrap().push(results);
        }

        fn push_post_outcome(&self, results: Vec<PostResult>) {
            self.next_post.lock().unwrap().push(results);
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

        fn post_orders_partial(
            &self,
            _orders: Vec<OrderRequest>,
        ) -> BoxFuture<'static, BotResult<PostOutcome>> {
            let calls = self.calls.clone();
            let next_post = self.next_post.clone();
            Box::pin(async move {
                calls.lock().unwrap().push("post");
                let result = next_post.lock().unwrap().pop().unwrap_or_else(Vec::new);
                Ok(PostOutcome { results: result })
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
                let result = next_cancel.lock().unwrap().pop().unwrap_or_default();
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

    #[tokio::test]
    async fn maker_only_compliance_integration() {
        let now_ms = now_ms();
        let start_s = now_ms / 1_000 + BTC_15M_INTERVAL_S;
        let slug = format!("btc-updown-15m-{start_s}");

        let identity = MarketIdentity {
            slug: slug.clone(),
            interval_start_ts: start_s,
            interval_end_ts: start_s + BTC_15M_INTERVAL_S,
            condition_id: "cond".to_string(),
            token_up: "token_up".to_string(),
            token_down: "token_down".to_string(),
            active: true,
            closed: false,
            accepting_orders: true,
            restricted: false,
        };

        let mut state = MarketState::new(identity, now_ms + 1_000_000);
        state.up_book.tick_size = 0.01;
        state.down_book.tick_size = 0.01;
        state.up_book.best_bid = Some(0.49);
        state.up_book.best_ask = Some(0.50);
        state.down_book.best_bid = Some(0.48);
        state.down_book.best_ask = Some(0.49);
        state.quoting_enabled = true;
        state.inventory.up.shares = 0.0;
        state.inventory.down.shares = 0.0;
        state.alpha.target_total = 0.99;
        state.alpha.cap_up = 0.99;
        state.alpha.cap_down = 0.99;
        state.alpha.size_scalar = 1.0;

        let mut cfg = TradingConfig::default();
        cfg.dry_run = false;
        cfg.min_update_interval_ms = 0;
        cfg.ladder_levels = 1;
        cfg.base_improve_ticks = 1;
        cfg.max_improve_ticks = 1;
        cfg.min_ticks_from_ask = 1;

        let mut completion_cfg = CompletionConfig::default();
        completion_cfg.enabled = false;

        let preview = build_desired_orders(&state, &cfg, now_ms);
        assert!(
            !preview.is_empty(),
            "quote engine produced no orders for test state"
        );

        let (tx_quote, rx_quote) = mpsc::channel(1);
        let (tx_exec, mut rx_exec) = mpsc::channel(1);
        let (tx_completion, _rx_completion) = mpsc::channel(1);

        let engine = StrategyEngine::new(
            cfg.clone(),
            InventoryConfig::default(),
            completion_cfg,
            RewardsConfig::default(),
            None,
        );
        let handle = tokio::spawn(engine.run(rx_quote, tx_exec, tx_completion));

        tx_quote
            .send(QuoteTick {
                slug: slug.clone(),
                now_ms,
                state,
            })
            .await
            .expect("send quote tick");
        drop(tx_quote);

        let cmd = timeout(Duration::from_millis(200), rx_exec.recv())
            .await
            .expect("exec command timeout")
            .expect("exec command missing");
        assert!(
            !cmd.desired.is_empty(),
            "strategy produced no desired orders"
        );

        let expected = HashMap::from([
            ("token_up".to_string(), (0.50, 0.01)),
            ("token_down".to_string(), (0.49, 0.01)),
        ]);

        let rest = ComplianceRestClient::new(expected, cfg.min_ticks_from_ask);
        let (_tx_user_orders, rx_user_orders) = mpsc::channel(1);
        let mut manager = OrderManager::with_client(cfg, Arc::new(rest.clone()), rx_user_orders);

        manager.handle_command_with_logger(cmd, None, None).await;

        let observed = rest.observed();
        assert!(!observed.is_empty(), "no orders were posted");
        let tokens: HashSet<_> = observed.iter().map(|o| o.token_id.as_str()).collect();
        assert_eq!(tokens.len(), 2, "expected orders on both tokens");

        let _ = handle.await;
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
        let slug = future_slug();
        let cache = manager.markets.entry(slug.clone()).or_default();
        cache.insert_live(LiveOrder {
            order_id: "order-old".to_string(),
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 10.0,
            remaining: 10.0,
            status: OrderStatus::Open,
            last_update_ms: 0,
        });

        let desired = vec![DesiredOrder {
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 12.0,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        manager
            .handle_command_with_logger(ExecCommand { slug, desired }, None, None)
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
        let slug = future_slug();

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
        cache.insert_live(LiveOrder {
            order_id: "order-1".to_string(),
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 10.0,
            remaining: 10.0,
            status: OrderStatus::Open,
            last_update_ms: 0,
        });

        let update = UserOrderUpdate {
            order_id: "order-1".to_string(),
            token_id: "token".to_string(),
            price: Some(0.50),
            original_size: Some(10.0),
            size_matched: Some(5.0),
            ts_ms: 1_000,
            update_type: UserOrderUpdateType::Update,
        };

        assert!(apply_user_order_update(&mut cache, &update, &future_slug()).is_some());

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

    #[tokio::test]
    async fn cancels_bypass_post_backoff() {
        let mut cfg = TradingConfig::default();
        cfg.dry_run = false;
        cfg.min_update_interval_ms = 0;

        let mock = MockRestClient::new();
        let (_tx_user_orders, rx_user_orders) = mpsc::channel(1);
        mock.push_cancel_result(CancelResult {
            canceled: vec!["order-1".to_string()],
            failed: Vec::new(),
        });

        let mut manager = OrderManager::with_client(cfg, Arc::new(mock.clone()), rx_user_orders);
        let slug = future_slug();
        let cache = manager.markets.entry(slug.clone()).or_default();
        cache.insert_live(LiveOrder {
            order_id: "order-1".to_string(),
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 10.0,
            remaining: 10.0,
            status: OrderStatus::Open,
            last_update_ms: 0,
        });
        cache.backoff_until_ms = now_ms() + 10_000;
        cache.backoff_ms = 10_000;

        manager
            .handle_command_with_logger(
                ExecCommand {
                    slug,
                    desired: Vec::new(),
                },
                None,
                None,
            )
            .await;

        let calls = mock.calls();
        assert_eq!(calls, vec!["cancel"]);
    }

    #[test]
    fn expected_post_only_reject_classification_matches_common_phrases() {
        assert!(is_expected_post_only_reject(
            "post-only order would cross the book"
        ));
        assert!(is_expected_post_only_reject("Post only would cross"));
        assert!(is_expected_post_only_reject("ORDER WOULD CROSS"));
        assert!(!is_expected_post_only_reject("invalid signature"));
    }

    #[tokio::test]
    async fn expected_post_only_reject_does_not_trigger_post_backoff() {
        let cfg = TradingConfig {
            dry_run: false,
            min_update_interval_ms: 0,
            ..TradingConfig::default()
        };

        let mock = MockRestClient::new();
        // Note: MockRestClient pops LIFO, so push the second outcome first.
        mock.push_post_result(vec!["order-ok".to_string()]);
        mock.push_post_outcome(vec![PostResult {
            order_id: None,
            error: Some("post-only order would cross the book".to_string()),
        }]);

        let (_tx_user_orders, rx_user_orders) = mpsc::channel(1);
        let mut manager = OrderManager::with_client(cfg, Arc::new(mock.clone()), rx_user_orders);
        let slug = future_slug();

        let desired = vec![DesiredOrder {
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 10.0,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        manager
            .handle_command_with_logger(
                ExecCommand {
                    slug: slug.clone(),
                    desired: desired.clone(),
                },
                None,
                None,
            )
            .await;

        assert_eq!(manager.markets.get(&slug).unwrap().backoff_ms, 0);

        manager
            .handle_command_with_logger(ExecCommand { slug, desired }, None, None)
            .await;

        assert_eq!(mock.calls(), vec!["sign", "post", "sign", "post"]);
    }

    #[tokio::test]
    async fn unexpected_post_reject_triggers_post_backoff_and_suppresses_posts() {
        let cfg = TradingConfig {
            dry_run: false,
            min_update_interval_ms: 0,
            ..TradingConfig::default()
        };

        let mock = MockRestClient::new();
        mock.push_post_outcome(vec![PostResult {
            order_id: None,
            error: Some("invalid signature".to_string()),
        }]);

        let (_tx_user_orders, rx_user_orders) = mpsc::channel(1);
        let mut manager = OrderManager::with_client(cfg, Arc::new(mock.clone()), rx_user_orders);
        let slug = future_slug();
        manager.markets.entry(slug.clone()).or_default().backoff_ms = BACKOFF_MAX_MS / 2;

        let desired = vec![DesiredOrder {
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 10.0,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        manager
            .handle_command_with_logger(
                ExecCommand {
                    slug: slug.clone(),
                    desired: desired.clone(),
                },
                None,
                None,
            )
            .await;

        assert_eq!(
            manager.markets.get(&slug).unwrap().backoff_ms,
            BACKOFF_MAX_MS
        );

        manager
            .handle_command_with_logger(ExecCommand { slug, desired }, None, None)
            .await;

        assert_eq!(mock.calls(), vec!["sign", "post"]);
    }

    #[tokio::test]
    async fn cutoff_suppresses_posts_but_cancels() {
        let mut cfg = TradingConfig::default();
        cfg.dry_run = false;
        cfg.resize_min_pct = 0.0;
        cfg.min_update_interval_ms = 0;

        let now = now_ms();
        let start_s = now / 1_000 - BTC_15M_INTERVAL_S;
        let slug = format!("btc-updown-15m-{start_s}");

        let mock = MockRestClient::new();
        let (_tx_user_orders, rx_user_orders) = mpsc::channel(1);
        mock.push_cancel_result(CancelResult {
            canceled: vec!["order-1".to_string()],
            failed: Vec::new(),
        });
        mock.push_post_result(vec!["order-2".to_string()]);

        let mut manager = OrderManager::with_client(cfg, Arc::new(mock.clone()), rx_user_orders);
        let cache = manager.markets.entry(slug.clone()).or_default();
        cache.insert_live(LiveOrder {
            order_id: "order-1".to_string(),
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 10.0,
            remaining: 10.0,
            status: OrderStatus::Open,
            last_update_ms: 0,
        });

        let desired = vec![DesiredOrder {
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 12.0,
            post_only: true,
            tif: TimeInForce::Gtc,
        }];

        manager
            .handle_command_with_logger(ExecCommand { slug, desired }, None, None)
            .await;

        let calls = mock.calls();
        assert_eq!(calls, vec!["cancel"]);
    }

    #[test]
    fn order_id_index_resolves_updates() {
        let mut cache = MarketOrderCache::default();
        let order = LiveOrder {
            order_id: "order-1".to_string(),
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 10.0,
            remaining: 10.0,
            status: OrderStatus::Open,
            last_update_ms: 0,
        };

        cache.live.insert(("token".to_string(), 0), order.clone());
        let update = UserOrderUpdate {
            order_id: "order-1".to_string(),
            token_id: "token".to_string(),
            price: Some(0.50),
            original_size: Some(10.0),
            size_matched: Some(10.0),
            ts_ms: 1_000,
            update_type: UserOrderUpdateType::Cancellation,
        };

        assert!(apply_user_order_update(&mut cache, &update, &future_slug()).is_none());

        let mut cache = MarketOrderCache::default();
        cache.insert_live(order);
        assert!(apply_user_order_update(&mut cache, &update, &future_slug()).is_some());
        assert!(!cache.order_id_index.contains_key("order-1"));
    }

    #[tokio::test]
    async fn unknown_user_update_for_tracked_token_triggers_safe_cancel() {
        let mut cfg = TradingConfig::default();
        cfg.dry_run = false;

        let mock = MockRestClient::new();
        mock.push_cancel_result(CancelResult {
            canceled: vec!["order-unknown".to_string()],
            failed: Vec::new(),
        });

        let (_tx_user_orders, rx_user_orders) = mpsc::channel(1);
        let mut manager = OrderManager::with_client(cfg, Arc::new(mock.clone()), rx_user_orders);
        let slug = future_slug();
        manager
            .markets
            .entry(slug)
            .or_default()
            .insert_live(make_live("order-known", "token-tracked", 0, 0.50, 10.0));

        manager.handle_user_order_update(
            UserOrderUpdate {
                order_id: "order-unknown".to_string(),
                token_id: "token-tracked".to_string(),
                price: None,
                original_size: None,
                size_matched: None,
                ts_ms: now_ms(),
                update_type: UserOrderUpdateType::Placement,
            },
            None,
        );

        timeout(Duration::from_secs(1), async {
            loop {
                if mock.calls().contains(&"cancel") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected cancel attempt for unknown tracked order");
    }

    #[tokio::test]
    async fn unknown_user_update_for_untracked_token_does_not_cancel() {
        let mut cfg = TradingConfig::default();
        cfg.dry_run = false;

        let mock = MockRestClient::new();
        let (_tx_user_orders, rx_user_orders) = mpsc::channel(1);
        let mut manager = OrderManager::with_client(cfg, Arc::new(mock.clone()), rx_user_orders);
        let slug = future_slug();
        manager
            .markets
            .entry(slug)
            .or_default()
            .insert_live(make_live("order-known", "token-tracked", 0, 0.50, 10.0));

        manager.handle_user_order_update(
            UserOrderUpdate {
                order_id: "order-unknown".to_string(),
                token_id: "token-untracked".to_string(),
                price: None,
                original_size: None,
                size_matched: None,
                ts_ms: now_ms(),
                update_type: UserOrderUpdateType::Placement,
            },
            None,
        );

        // Give the scheduler a chance in case the implementation spawns tasks.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(
            !mock.calls().contains(&"cancel"),
            "unexpected cancel for untracked token"
        );
    }

    fn make_live(order_id: &str, token_id: &str, level: usize, price: f64, size: f64) -> LiveOrder {
        LiveOrder {
            order_id: order_id.to_string(),
            token_id: token_id.to_string(),
            level,
            price,
            size,
            remaining: size,
            status: OrderStatus::Open,
            last_update_ms: 0,
        }
    }

    fn make_desired(token_id: &str, level: usize, price: f64, size: f64) -> DesiredOrder {
        DesiredOrder {
            token_id: token_id.to_string(),
            level,
            price,
            size,
            post_only: true,
            tif: TimeInForce::Gtc,
        }
    }

    fn assert_cache_consistent(cache: &MarketOrderCache) {
        assert_eq!(cache.live.len(), cache.order_id_index.len());
        for (key, live) in &cache.live {
            let indexed = cache
                .order_id_index
                .get(&live.order_id)
                .expect("missing order_id_index entry");
            assert_eq!(indexed, key);
            assert!(cache.last_update_ms.contains_key(key));
        }
        for (order_id, key) in &cache.order_id_index {
            let live = cache.live.get(key).expect("index points to missing order");
            assert_eq!(&live.order_id, order_id);
        }
    }

    #[tokio::test]
    async fn race_cancel_ack_before_post_ack_user_cancel() {
        let mock = MockRestClient::new();
        mock.push_cancel_result(CancelResult {
            canceled: vec!["order-old".to_string()],
            failed: Vec::new(),
        });
        mock.push_post_result(vec!["order-new".to_string()]);

        let slug = future_slug();
        let mut cache = MarketOrderCache::default();
        cache.insert_live(make_live("order-old", "token-a", 0, 0.50, 10.0));

        let plan = DiffPlan {
            cancels: vec![CancelAction {
                token_id: "token-a".to_string(),
                level: 0,
                order_id: "order-old".to_string(),
            }],
            posts: Vec::new(),
        };

        execute_cancels(&mock, &slug, &mut cache, &plan, now_ms(), None, None)
            .await
            .expect("cancel ok");
        assert!(cache.key_for_order_id("order-old").is_none());

        let update = UserOrderUpdate {
            order_id: "order-old".to_string(),
            token_id: "token-a".to_string(),
            price: None,
            original_size: None,
            size_matched: None,
            ts_ms: now_ms(),
            update_type: UserOrderUpdateType::Cancellation,
        };
        assert!(apply_user_order_update(&mut cache, &update, &slug).is_none());

        let posts = vec![make_desired("token-b", 1, 0.42, 5.0)];
        execute_posts(&mock, &slug, &mut cache, posts, now_ms(), None, None)
            .await
            .expect("post ok");

        assert!(cache.key_for_order_id("order-old").is_none());
        assert!(cache.key_for_order_id("order-new").is_some());
        assert_cache_consistent(&cache);
    }

    #[tokio::test]
    async fn race_post_ack_before_cancel_ack_user_update() {
        let mock = MockRestClient::new();
        mock.push_post_result(vec!["order-new".to_string()]);
        mock.push_cancel_result(CancelResult {
            canceled: vec!["order-old".to_string()],
            failed: Vec::new(),
        });

        let slug = future_slug();
        let mut cache = MarketOrderCache::default();
        cache.insert_live(make_live("order-old", "token-a", 0, 0.50, 10.0));

        let posts = vec![make_desired("token-b", 1, 0.47, 6.0)];
        execute_posts(&mock, &slug, &mut cache, posts, now_ms(), None, None)
            .await
            .expect("post ok");

        let update = UserOrderUpdate {
            order_id: "order-new".to_string(),
            token_id: "token-b".to_string(),
            price: Some(0.47),
            original_size: Some(6.0),
            size_matched: Some(2.0),
            ts_ms: now_ms(),
            update_type: UserOrderUpdateType::Update,
        };
        assert!(apply_user_order_update(&mut cache, &update, &slug).is_some());
        assert_cache_consistent(&cache);

        let plan = DiffPlan {
            cancels: vec![CancelAction {
                token_id: "token-a".to_string(),
                level: 0,
                order_id: "order-old".to_string(),
            }],
            posts: Vec::new(),
        };
        execute_cancels(&mock, &slug, &mut cache, &plan, now_ms(), None, None)
            .await
            .expect("cancel ok");

        assert!(cache.key_for_order_id("order-old").is_none());
        assert!(cache.key_for_order_id("order-new").is_some());
        assert_cache_consistent(&cache);
    }

    #[tokio::test]
    async fn partial_post_accept_reject_keeps_cache() {
        let mock = MockRestClient::new();
        mock.push_post_outcome(vec![
            PostResult {
                order_id: Some("order-accept".to_string()),
                error: None,
            },
            PostResult {
                order_id: None,
                error: Some("rejected".to_string()),
            },
        ]);

        let slug = future_slug();
        let mut cache = MarketOrderCache::default();
        let posts = vec![
            make_desired("token-a", 0, 0.41, 3.0),
            make_desired("token-b", 1, 0.59, 4.0),
        ];

        let outcome = execute_posts(&mock, &slug, &mut cache, posts, now_ms(), None, None)
            .await
            .expect("post ok");
        assert_eq!(outcome.results.len(), 2);

        assert!(cache.key_for_order_id("order-accept").is_some());
        assert_eq!(cache.live.len(), 1);
        assert_cache_consistent(&cache);
    }

    #[test]
    fn user_rejection_removes_cached_order() {
        let mut cache = MarketOrderCache::default();
        cache.insert_live(LiveOrder {
            order_id: "order-1".to_string(),
            token_id: "token".to_string(),
            level: 0,
            price: 0.50,
            size: 10.0,
            remaining: 10.0,
            status: OrderStatus::Open,
            last_update_ms: 0,
        });

        let update = UserOrderUpdate {
            order_id: "order-1".to_string(),
            token_id: "token".to_string(),
            price: None,
            original_size: None,
            size_matched: None,
            ts_ms: 1_000,
            update_type: UserOrderUpdateType::Rejection,
        };

        assert!(apply_user_order_update(&mut cache, &update, &future_slug()).is_some());
        assert!(cache.live.is_empty());
        assert!(!cache.order_id_index.contains_key("order-1"));
    }
}
