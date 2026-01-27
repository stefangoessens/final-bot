use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::{mpsc::Receiver, watch};
use tokio::time::{interval, Instant};

use crate::clients::data_api::{DataApiClient, DataApiPosition};
use crate::error::BotResult;
use crate::ops::shutdown::ShutdownTrigger;
use crate::state::state_manager::QuoteTick;

pub trait PositionsClient: Send + Sync {
    fn fetch_positions(
        &self,
        user: String,
        condition_ids: Vec<String>,
    ) -> BoxFuture<'static, BotResult<Vec<DataApiPosition>>>;
}

impl PositionsClient for DataApiClient {
    fn fetch_positions(
        &self,
        user: String,
        condition_ids: Vec<String>,
    ) -> BoxFuture<'static, BotResult<Vec<DataApiPosition>>> {
        let client = self.clone();
        Box::pin(async move { client.fetch_positions(&user, Some(&condition_ids)).await })
    }
}

#[derive(Debug, Clone)]
pub struct DesyncWatchdogConfig {
    pub interval_s: u64,
    /// How long a mismatch must persist before triggering fatal shutdown.
    pub mismatch_hold_s: u64,
    /// Absolute difference in shares beyond which we consider inventory "mismatched".
    pub max_abs_shares_diff: f64,
}

impl Default for DesyncWatchdogConfig {
    fn default() -> Self {
        Self {
            interval_s: 30,
            mismatch_hold_s: 60,
            max_abs_shares_diff: 0.05,
        }
    }
}

#[derive(Debug, Clone)]
struct MarketInventorySnapshot {
    slug: String,
    condition_id: String,
    token_up: String,
    token_down: String,
    local_up: f64,
    local_down: f64,
}

struct DesyncWatchdog {
    cfg: DesyncWatchdogConfig,
    mismatch_since: HashMap<String, Instant>,
}

impl DesyncWatchdog {
    fn new(cfg: DesyncWatchdogConfig) -> Self {
        Self {
            cfg,
            mismatch_since: HashMap::new(),
        }
    }

    fn check(
        &mut self,
        now: Instant,
        snapshots: &[MarketInventorySnapshot],
        positions: &[DataApiPosition],
    ) -> Option<String> {
        let mut api_shares: HashMap<(String, String), f64> = HashMap::new();
        for pos in positions {
            if !pos.size.is_finite() {
                continue;
            }
            *api_shares
                .entry((pos.condition_id.clone(), pos.asset.clone()))
                .or_insert(0.0) += pos.size;
        }

        let max_abs = self.cfg.max_abs_shares_diff.max(0.0);
        let hold = Duration::from_secs(self.cfg.mismatch_hold_s);

        for snapshot in snapshots {
            let local_up = normalize_shares(snapshot.local_up);
            let local_down = normalize_shares(snapshot.local_down);
            let api_up = api_shares
                .get(&(snapshot.condition_id.clone(), snapshot.token_up.clone()))
                .copied()
                .unwrap_or(0.0);
            let api_down = api_shares
                .get(&(snapshot.condition_id.clone(), snapshot.token_down.clone()))
                .copied()
                .unwrap_or(0.0);

            let diff_up = (api_up - local_up).abs();
            let diff_down = (api_down - local_down).abs();
            let mismatched = diff_up > max_abs || diff_down > max_abs;

            if !mismatched {
                if self.mismatch_since.remove(&snapshot.slug).is_some() {
                    tracing::debug!(
                        target: "desync_watchdog",
                        slug = %snapshot.slug,
                        "inventory mismatch resolved"
                    );
                }
                continue;
            }

            let since = self
                .mismatch_since
                .entry(snapshot.slug.clone())
                .or_insert_with(|| {
                    tracing::warn!(
                        target: "desync_watchdog",
                        slug = %snapshot.slug,
                        condition_id = %snapshot.condition_id,
                        api_up = api_up,
                        local_up = local_up,
                        api_down = api_down,
                        local_down = local_down,
                        diff_up = diff_up,
                        diff_down = diff_down,
                        hold_s = self.cfg.mismatch_hold_s,
                        max_abs_shares_diff = max_abs,
                        "inventory mismatch detected; starting hold timer"
                    );
                    now
                });

            if now.duration_since(*since) >= hold {
                return Some(format!(
                    "inventory_desync slug={} condition_id={} api_up={:.6} local_up={:.6} api_down={:.6} local_down={:.6} diff_up={:.6} diff_down={:.6}",
                    snapshot.slug, snapshot.condition_id, api_up, local_up, api_down, local_down, diff_up, diff_down
                ));
            }
        }

        None
    }
}

pub fn spawn_desync_watchdog(
    cfg: DesyncWatchdogConfig,
    mut rx_quote: Receiver<QuoteTick>,
    data_api: Arc<dyn PositionsClient>,
    data_api_user: String,
    fatal_tx: watch::Sender<Option<String>>,
    shutdown_trigger: ShutdownTrigger,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(cfg.interval_s.max(1)));
        let mut latest: HashMap<String, crate::state::market_state::MarketState> = HashMap::new();
        let mut watchdog = DesyncWatchdog::new(cfg);

        loop {
            tokio::select! {
                maybe_tick = rx_quote.recv() => {
                    match maybe_tick {
                        Some(QuoteTick { slug, state, .. }) => {
                            latest.insert(slug, state);
                        }
                        None => break,
                    }
                }
                _ = ticker.tick() => {
                    if latest.is_empty() {
                        continue;
                    }

                    let snapshots: Vec<MarketInventorySnapshot> = latest
                        .values()
                        .map(|state| MarketInventorySnapshot {
                            slug: state.identity.slug.clone(),
                            condition_id: state.identity.condition_id.clone(),
                            token_up: state.identity.token_up.clone(),
                            token_down: state.identity.token_down.clone(),
                            local_up: state.inventory.up.shares,
                            local_down: state.inventory.down.shares,
                        })
                        .collect();
                    let condition_ids: Vec<String> = snapshots
                        .iter()
                        .map(|snapshot| snapshot.condition_id.clone())
                        .collect();

                    let positions = match data_api
                        .fetch_positions(data_api_user.clone(), condition_ids)
                        .await
                    {
                        Ok(positions) => positions,
                        Err(err) => {
                            tracing::warn!(
                                target: "desync_watchdog",
                                error = %err,
                                "data api position fetch failed"
                            );
                            continue;
                        }
                    };

                    if let Some(reason) = watchdog.check(Instant::now(), &snapshots, &positions) {
                        tracing::error!(target: "desync_watchdog", reason = %reason, "inventory desync persisted; triggering shutdown");
                        let _ = fatal_tx.send(Some(reason));
                        shutdown_trigger.trigger();
                        break;
                    }
                }
            }
        }
    })
}

fn normalize_shares(value: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops;
    use crate::state::market_state::{MarketIdentity, MarketState};
    use tokio::sync::mpsc;

    fn make_snapshot(local_up: f64, local_down: f64) -> MarketInventorySnapshot {
        MarketInventorySnapshot {
            slug: "btc-updown-15m-0".to_string(),
            condition_id: "cond".to_string(),
            token_up: "up".to_string(),
            token_down: "down".to_string(),
            local_up,
            local_down,
        }
    }

    fn make_pos(asset: &str, size: f64) -> DataApiPosition {
        DataApiPosition {
            condition_id: "cond".to_string(),
            asset: asset.to_string(),
            outcome: String::new(),
            size,
            avg_price: 0.0,
            mergeable: false,
            redeemable: false,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn check_triggers_only_after_hold() {
        let cfg = DesyncWatchdogConfig {
            interval_s: 1,
            mismatch_hold_s: 5,
            max_abs_shares_diff: 0.05,
        };
        let mut watchdog = DesyncWatchdog::new(cfg);
        let snapshots = vec![make_snapshot(1.0, 1.0)];
        let positions = vec![make_pos("up", 2.0), make_pos("down", 1.0)];

        assert!(watchdog
            .check(Instant::now(), &snapshots, &positions)
            .is_none());

        tokio::time::advance(Duration::from_secs(6)).await;

        let reason = watchdog
            .check(Instant::now(), &snapshots, &positions)
            .expect("fatal");
        assert!(reason.contains("inventory_desync"));
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_triggers_shutdown_and_fatal() {
        #[derive(Clone)]
        struct StaticClient {
            positions: Vec<DataApiPosition>,
        }

        impl PositionsClient for StaticClient {
            fn fetch_positions(
                &self,
                _user: String,
                _condition_ids: Vec<String>,
            ) -> BoxFuture<'static, BotResult<Vec<DataApiPosition>>> {
                let positions = self.positions.clone();
                Box::pin(async move { Ok(positions) })
            }
        }

        let identity = MarketIdentity {
            slug: "btc-updown-15m-0".to_string(),
            interval_start_ts: 0,
            interval_end_ts: 900,
            condition_id: "cond".to_string(),
            token_up: "up".to_string(),
            token_down: "down".to_string(),
            active: true,
            closed: false,
            accepting_orders: true,
            restricted: false,
        };
        let mut state = MarketState::new(identity, 0);
        state.inventory.up.shares = 0.0;
        state.inventory.down.shares = 0.0;

        let (tx_quote, rx_quote) = mpsc::channel(4);
        let (fatal_tx, fatal_rx) = watch::channel::<Option<String>>(None);
        let (shutdown_trigger, shutdown) = ops::shutdown::channel();

        let client = Arc::new(StaticClient {
            positions: vec![make_pos("up", 1.0), make_pos("down", 0.0)],
        });

        spawn_desync_watchdog(
            DesyncWatchdogConfig {
                interval_s: 1,
                mismatch_hold_s: 0,
                max_abs_shares_diff: 0.05,
            },
            rx_quote,
            client,
            "user".to_string(),
            fatal_tx,
            shutdown_trigger,
        );

        tx_quote
            .send(QuoteTick {
                slug: "btc-updown-15m-0".to_string(),
                now_ms: 0,
                state,
            })
            .await
            .expect("send");

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        assert!(fatal_rx
            .borrow()
            .as_ref()
            .expect("reason")
            .contains("inventory_desync"));

        tokio::time::timeout(Duration::from_millis(50), shutdown.clone().wait())
            .await
            .expect("shutdown should be triggered");
    }
}
