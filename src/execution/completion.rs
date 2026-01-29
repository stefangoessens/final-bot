use std::collections::HashMap;

use futures_util::future::BoxFuture;
use polymarket_client_sdk::clob::types::Side;
use serde_json::json;
use tokio::sync::mpsc::Receiver;

use crate::clients::clob_rest::{
    CancelResult, ClobRestClient, OrderId, OrderRequest, PostOrdersResult,
};
use crate::config::CompletionConfig;
use crate::error::BotResult;
use crate::persistence::LogEvent;
use crate::strategy::engine::CompletionCommand;

const COMPLETION_RATE_LIMIT_MS: i64 = 1_000;

trait CompletionRestClient: Send + Sync {
    fn cancel_orders(&self, order_ids: Vec<OrderId>)
        -> BoxFuture<'static, BotResult<CancelResult>>;
    fn build_completion_order(
        &self,
        token_id: String,
        side: Side,
        shares: f64,
        p_max: f64,
        order_type: crate::config::CompletionOrderType,
        now_ms: i64,
    ) -> BoxFuture<'static, BotResult<OrderRequest>>;
    fn post_orders_result(
        &self,
        orders: Vec<OrderRequest>,
    ) -> BoxFuture<'static, BotResult<PostOrdersResult>>;
}

impl CompletionRestClient for ClobRestClient {
    fn cancel_orders(
        &self,
        order_ids: Vec<OrderId>,
    ) -> BoxFuture<'static, BotResult<CancelResult>> {
        let client = self.clone();
        Box::pin(async move { client.cancel_orders(order_ids).await })
    }

    fn build_completion_order(
        &self,
        token_id: String,
        side: Side,
        shares: f64,
        p_max: f64,
        order_type: crate::config::CompletionOrderType,
        now_ms: i64,
    ) -> BoxFuture<'static, BotResult<OrderRequest>> {
        let client = self.clone();
        Box::pin(async move {
            client
                .build_completion_order(&token_id, side, shares, p_max, order_type, now_ms)
                .await
        })
    }

    fn post_orders_result(
        &self,
        orders: Vec<OrderRequest>,
    ) -> BoxFuture<'static, BotResult<PostOrdersResult>> {
        let client = self.clone();
        Box::pin(async move { client.post_orders_result(orders).await })
    }
}

pub struct CompletionExecutor {
    completion_cfg: CompletionConfig,
    rest: std::sync::Arc<dyn CompletionRestClient>,
    rate_limiter: CompletionRateLimiter,
}

impl CompletionExecutor {
    pub fn new(completion_cfg: CompletionConfig, rest: ClobRestClient) -> Self {
        Self::with_client(completion_cfg, std::sync::Arc::new(rest))
    }

    fn with_client(
        completion_cfg: CompletionConfig,
        rest: std::sync::Arc<dyn CompletionRestClient>,
    ) -> Self {
        Self {
            completion_cfg,
            rest,
            rate_limiter: CompletionRateLimiter::default(),
        }
    }

    pub async fn run(
        mut self,
        mut rx_completion: Receiver<CompletionCommand>,
        log_tx: Option<tokio::sync::mpsc::Sender<LogEvent>>,
        dry_run: bool,
    ) {
        while let Some(cmd) = rx_completion.recv().await {
            let now_ms = if cmd.now_ms > 0 { cmd.now_ms } else { now_ms() };
            if !self.completion_cfg.enabled {
                tracing::warn!(
                    target: "completion_executor",
                    slug = %cmd.slug,
                    "completion disabled; ignoring command"
                );
                continue;
            }

            if !self.rate_limiter.allows(&cmd.slug, now_ms) {
                tracing::info!(
                    target: "completion_executor",
                    slug = %cmd.slug,
                    token_id = %cmd.token_id,
                    side = ?cmd.side,
                    p_max = cmd.p_max,
                    shares = cmd.shares,
                    order_type = ?cmd.order_type,
                    "completion suppressed by rate limit"
                );
                continue;
            }

            if dry_run {
                tracing::info!(
                    target: "completion_executor",
                    slug = %cmd.slug,
                    token_id = %cmd.token_id,
                    side = ?cmd.side,
                    p_max = cmd.p_max,
                    shares = cmd.shares,
                    order_type = ?cmd.order_type,
                    "dry_run enabled; skipping completion order"
                );
                continue;
            }

            if let Err(err) = self.cancel_sweep(&cmd).await {
                tracing::warn!(
                    target: "completion_executor",
                    slug = %cmd.slug,
                    condition_id = %cmd.condition_id,
                    token_id = %cmd.token_id,
                    error = %err,
                    "pre-completion cancel sweep failed"
                );
            }

            let order = match self
                .rest
                .build_completion_order(
                    cmd.token_id.clone(),
                    cmd.side,
                    cmd.shares,
                    cmd.p_max,
                    cmd.order_type,
                    now_ms,
                )
                .await
            {
                Ok(order) => order,
                Err(err) => {
                    tracing::warn!(
                        target: "completion_executor",
                        slug = %cmd.slug,
                        token_id = %cmd.token_id,
                        error = %err,
                        "failed to build completion order"
                    );
                    continue;
                }
            };

            let result: PostOrdersResult = match self.rest.post_orders_result(vec![order]).await {
                Ok(result) => result,
                Err(err) => {
                    tracing::warn!(
                        target: "completion_executor",
                        slug = %cmd.slug,
                        token_id = %cmd.token_id,
                        error = %err,
                        "completion post_orders failed"
                    );
                    continue;
                }
            };

            log_completion_result(&log_tx, &cmd, now_ms, &result);
            if result.accepted.is_empty() && result.rejected.is_empty() {
                tracing::warn!(
                    target: "completion_executor",
                    slug = %cmd.slug,
                    token_id = %cmd.token_id,
                    "completion post_orders returned empty result"
                );
            }

            for accepted in &result.accepted {
                tracing::info!(
                    target: "completion_executor",
                    slug = %cmd.slug,
                    token_id = %cmd.token_id,
                    side = ?cmd.side,
                    p_max = cmd.p_max,
                    shares = cmd.shares,
                    order_type = ?cmd.order_type,
                    order_id = %accepted.order_id,
                    "completion order accepted"
                );
            }

            for rejected in &result.rejected {
                tracing::warn!(
                    target: "completion_executor",
                    slug = %cmd.slug,
                    token_id = %cmd.token_id,
                    side = ?cmd.side,
                    p_max = cmd.p_max,
                    shares = cmd.shares,
                    order_type = ?cmd.order_type,
                    error = %rejected.error,
                    "completion order rejected"
                );
            }
        }
    }

    async fn cancel_sweep(&self, cmd: &CompletionCommand) -> BotResult<()> {
        let mut order_ids: Vec<OrderId> = cmd
            .cancel_order_ids
            .iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        order_ids.sort();
        order_ids.dedup();
        if order_ids.is_empty() {
            return Ok(());
        }

        let res = self.rest.cancel_orders(order_ids).await?;
        if !res.failed.is_empty() {
            tracing::warn!(
                target: "completion_executor",
                slug = %cmd.slug,
                condition_id = %cmd.condition_id,
                token_id = %cmd.token_id,
                failed = res.failed.len(),
                "cancel sweep had failures before completion"
            );
        } else {
            tracing::info!(
                target: "completion_executor",
                slug = %cmd.slug,
                condition_id = %cmd.condition_id,
                token_id = %cmd.token_id,
                canceled = res.canceled.len(),
                "cancel sweep completed before completion"
            );
        }

        Ok(())
    }
}

#[derive(Debug, Default)]
struct CompletionRateLimiter {
    last_sent_ms: HashMap<String, i64>,
}

impl CompletionRateLimiter {
    fn allows(&mut self, slug: &str, now_ms: i64) -> bool {
        if let Some(last) = self.last_sent_ms.get(slug) {
            if now_ms.saturating_sub(*last) < COMPLETION_RATE_LIMIT_MS {
                return false;
            }
        }
        self.last_sent_ms.insert(slug.to_string(), now_ms);
        true
    }
}

fn log_completion_result(
    log_tx: &Option<tokio::sync::mpsc::Sender<LogEvent>>,
    cmd: &CompletionCommand,
    ts_ms: i64,
    result: &PostOrdersResult,
) {
    let Some(tx) = log_tx else {
        return;
    };
    let accepted: Vec<_> = result
        .accepted
        .iter()
        .map(|item| json!({ "order_id": item.order_id, "token_id": item.token_id }))
        .collect();
    let rejected: Vec<_> = result
        .rejected
        .iter()
        .map(|item| json!({ "error": item.error, "token_id": item.token_id }))
        .collect();
    let payload = json!({
        "slug": cmd.slug,
        "token_id": cmd.token_id,
        "side": format!("{:?}", cmd.side),
        "shares": cmd.shares,
        "p_max": cmd.p_max,
        "order_type": format!("{:?}", cmd.order_type),
        "accepted": accepted,
        "rejected": rejected,
    });
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: "execution.completion".to_string(),
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
    use std::sync::{Arc, Mutex};

    use polymarket_client_sdk::auth::ApiKey;
    use polymarket_client_sdk::clob::types::{Order, OrderType as SdkOrderType, SignedOrder};
    use polymarket_client_sdk::types::{Signature, U256};
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    use crate::config::CompletionOrderType;
    use polymarket_client_sdk::clob::types::Side;

    #[test]
    fn rate_limit_suppresses_within_window_per_slug() {
        let mut limiter = CompletionRateLimiter::default();

        let base = 1_000_i64;
        assert!(limiter.allows("slug-a", base));
        assert!(
            !limiter.allows("slug-a", base + (COMPLETION_RATE_LIMIT_MS as i64 - 1)),
            "expected suppression within window"
        );
        assert!(limiter.allows("slug-b", base + 100));
        assert!(limiter.allows("slug-a", base + COMPLETION_RATE_LIMIT_MS as i64));
    }

    #[derive(Clone)]
    struct MockRest {
        cancel_result: Arc<Mutex<Result<CancelResult, String>>>,
        calls: Arc<Mutex<Vec<&'static str>>>,
        seen_cancel_ids: Arc<Mutex<Vec<String>>>,
    }

    impl MockRest {
        fn new() -> Self {
            Self {
                cancel_result: Arc::new(Mutex::new(Ok(CancelResult::default()))),
                calls: Arc::new(Mutex::new(Vec::new())),
                seen_cancel_ids: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn set_cancel_result(&self, res: Result<CancelResult, String>) {
            *self.cancel_result.lock().expect("lock cancel_result") = res;
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().expect("lock calls").clone()
        }
    }

    impl CompletionRestClient for MockRest {
        fn cancel_orders(
            &self,
            order_ids: Vec<OrderId>,
        ) -> BoxFuture<'static, BotResult<CancelResult>> {
            let calls = self.calls.clone();
            let seen_cancel_ids = self.seen_cancel_ids.clone();
            let cancel_result = self.cancel_result.clone();
            Box::pin(async move {
                calls.lock().expect("lock calls").push("cancel_orders");
                *seen_cancel_ids.lock().expect("lock seen_cancel_ids") = order_ids;
                match cancel_result.lock().expect("lock cancel_result").clone() {
                    Ok(res) => Ok(res),
                    Err(err) => Err(crate::error::BotError::Other(err)),
                }
            })
        }

        fn build_completion_order(
            &self,
            _token_id: String,
            _side: Side,
            _shares: f64,
            _p_max: f64,
            _order_type: CompletionOrderType,
            _now_ms: i64,
        ) -> BoxFuture<'static, BotResult<OrderRequest>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                calls
                    .lock()
                    .expect("lock calls")
                    .push("build_completion_order");
                Ok(SignedOrder::builder()
                    .order(Order::default())
                    .signature(Signature::new(U256::ZERO, U256::ZERO, false))
                    .order_type(SdkOrderType::FOK)
                    .owner(ApiKey::nil())
                    .post_only(false)
                    .build())
            })
        }

        fn post_orders_result(
            &self,
            _orders: Vec<OrderRequest>,
        ) -> BoxFuture<'static, BotResult<PostOrdersResult>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                calls.lock().expect("lock calls").push("post_orders_result");
                Ok(PostOrdersResult::default())
            })
        }
    }

    #[tokio::test]
    async fn completion_cancels_active_orders_before_posting() {
        let rest = Arc::new(MockRest::new());

        let completion_cfg = CompletionConfig::default();
        let executor = CompletionExecutor::with_client(completion_cfg, rest.clone());

        let (tx, rx) = mpsc::channel(1);
        let handle = tokio::spawn(executor.run(rx, None, false));

        tx.send(CompletionCommand {
            slug: "btc-15m-test".to_string(),
            condition_id: "cond".to_string(),
            token_id: "token".to_string(),
            cancel_order_ids: vec!["o1".to_string(), "o2".to_string()],
            side: Side::Buy,
            shares: 1.0,
            p_max: 0.5,
            order_type: CompletionOrderType::Fok,
            now_ms: 1,
        })
        .await
        .expect("send completion command");
        drop(tx);

        timeout(Duration::from_millis(200), handle)
            .await
            .expect("executor join timeout")
            .expect("executor task failed");

        assert_eq!(
            rest.calls(),
            vec![
                "cancel_orders",
                "build_completion_order",
                "post_orders_result"
            ]
        );
        assert_eq!(
            &*rest.seen_cancel_ids.lock().expect("lock seen_cancel_ids"),
            &vec!["o1".to_string(), "o2".to_string()]
        );
    }

    #[tokio::test]
    async fn completion_proceeds_when_cancel_sweep_fails() {
        let rest = Arc::new(MockRest::new());
        rest.set_cancel_result(Err("cancel failed".to_string()));

        let completion_cfg = CompletionConfig::default();
        let executor = CompletionExecutor::with_client(completion_cfg, rest.clone());

        let (tx, rx) = mpsc::channel(1);
        let handle = tokio::spawn(executor.run(rx, None, false));

        tx.send(CompletionCommand {
            slug: "btc-15m-test".to_string(),
            condition_id: "cond".to_string(),
            token_id: "token".to_string(),
            cancel_order_ids: vec!["o1".to_string()],
            side: Side::Buy,
            shares: 1.0,
            p_max: 0.5,
            order_type: CompletionOrderType::Fok,
            now_ms: 1,
        })
        .await
        .expect("send completion command");
        drop(tx);

        timeout(Duration::from_millis(200), handle)
            .await
            .expect("executor join timeout")
            .expect("executor task failed");

        assert_eq!(
            rest.calls(),
            vec![
                "cancel_orders",
                "build_completion_order",
                "post_orders_result"
            ]
        );
    }

    #[tokio::test]
    async fn completion_skips_cancel_when_no_active_orders() {
        let rest = Arc::new(MockRest::new());

        let completion_cfg = CompletionConfig::default();
        let executor = CompletionExecutor::with_client(completion_cfg, rest.clone());

        let (tx, rx) = mpsc::channel(1);
        let handle = tokio::spawn(executor.run(rx, None, false));

        tx.send(CompletionCommand {
            slug: "btc-15m-test".to_string(),
            condition_id: "cond".to_string(),
            token_id: "token".to_string(),
            cancel_order_ids: Vec::new(),
            side: Side::Buy,
            shares: 1.0,
            p_max: 0.5,
            order_type: CompletionOrderType::Fok,
            now_ms: 1,
        })
        .await
        .expect("send completion command");
        drop(tx);

        timeout(Duration::from_millis(200), handle)
            .await
            .expect("executor join timeout")
            .expect("executor task failed");

        assert_eq!(
            rest.calls(),
            vec!["build_completion_order", "post_orders_result"]
        );
    }
}
