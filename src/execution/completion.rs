use std::collections::HashMap;

use serde_json::json;
use tokio::sync::mpsc::Receiver;

use crate::clients::clob_rest::{ClobRestClient, PostOrdersResult};
use crate::config::CompletionConfig;
use crate::persistence::LogEvent;
use crate::strategy::engine::CompletionCommand;

const COMPLETION_RATE_LIMIT_MS: i64 = 1_000;

#[derive(Debug)]
pub struct CompletionExecutor {
    completion_cfg: CompletionConfig,
    rest: ClobRestClient,
    rate_limiter: CompletionRateLimiter,
}

impl CompletionExecutor {
    pub fn new(completion_cfg: CompletionConfig, rest: ClobRestClient) -> Self {
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
                    p_max = cmd.p_max,
                    shares = cmd.shares,
                    order_type = ?cmd.order_type,
                    "dry_run enabled; skipping completion order"
                );
                continue;
            }

            let order = match self
                .rest
                .build_completion_order(
                    &cmd.token_id,
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
                    p_max = cmd.p_max,
                    shares = cmd.shares,
                    order_type = ?cmd.order_type,
                    error = %rejected.error,
                    "completion order rejected"
                );
            }
        }
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
        assert!(limiter.allows(
            "slug-a",
            base + COMPLETION_RATE_LIMIT_MS as i64
        ));
    }
}
