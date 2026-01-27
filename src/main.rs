mod alpha;
mod clients;
mod config;
mod error;
mod execution;
// T15: inventory + resolution modules (scaffolding)
mod inventory;
mod market_discovery;
// T17: persistence modules (event log + replay scaffold)
#[allow(dead_code)]
mod persistence;
// T15: inventory + resolution modules (scaffolding)
mod resolution;
mod state;
mod strategy;
mod time;

mod ops;
mod reconciliation;

use crate::error::BotResult;
use crate::persistence::{spawn_event_logger, EventLogConfig};
use std::sync::Arc;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> BotResult<()> {
    let cfg = config::load_config()?;
    ops::logging::init_with_default(&cfg.infra.log_level);
    // Install rustls crypto provider before any TLS clients are constructed.
    ops::tls::install_rustls_provider();
    let ops_state = ops::OpsState::new(&cfg);
    let (shutdown_trigger, shutdown) = ops::start_http_servers(&cfg, ops_state.clone());

    let event_log_cfg = EventLogConfig::default();
    let tx_log = spawn_event_logger(event_log_cfg);

    tracing::info!(
        target: "boot",
        aws_region = %cfg.infra.aws_region,
        dry_run = cfg.trading.dry_run,
        "polymarket-mm-bot starting"
    );

    let (tx_events, rx_events) = mpsc::channel(4096);
    let (tx_quote_raw, rx_quote_raw) = mpsc::channel(64);
    let (tx_quote_strategy, rx_quote_strategy) = mpsc::channel(64);
    let (tx_quote_inventory, rx_quote_inventory) = mpsc::channel(64);
    let (tx_quote_rewards, rx_quote_rewards) = mpsc::channel(64);
    let (tx_exec, rx_exec) = mpsc::channel(256);
    let (tx_completion, rx_completion) = mpsc::channel(256);
    let (tx_market_ws_cmd, rx_market_ws_cmd) = mpsc::channel(256);
    let (tx_user_orders, rx_user_orders) = mpsc::channel(256);
    let rewards_enabled = cfg.rewards.enable_liquidity_rewards_chasing;
    let (fatal_tx, fatal_rx) = tokio::sync::watch::channel::<Option<String>>(None);
    let data_api_user = reconciliation::resolve_data_api_user(&cfg);
    let enable_desync_watchdog = !cfg.trading.dry_run
        && cfg.inventory.desync_watchdog_enabled
        && data_api_user.is_some()
        && !cfg.endpoints.data_api_base_url.trim().is_empty();

    let (tx_quote_desync, rx_quote_desync) = if enable_desync_watchdog {
        let (tx, rx) = mpsc::channel(64);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    tokio::spawn(
        state::state_manager::StateManager::new(
            cfg.alpha.clone(),
            cfg.oracle.clone(),
            cfg.trading.clone(),
            ops_state.health.clone(),
            ops_state.metrics.clone(),
        )
        .run(rx_events, tx_quote_raw),
    );

    tokio::spawn(async move {
        let mut rx_quote_raw = rx_quote_raw;
        let tx_quote_strategy = tx_quote_strategy;
        let tx_quote_inventory = tx_quote_inventory;
        let tx_quote_rewards = tx_quote_rewards;
        let tx_quote_desync = tx_quote_desync;
        let rewards_enabled = rewards_enabled;

        while let Some(tick) = rx_quote_raw.recv().await {
            let tick_for_strategy = tick.clone();
            let tick_for_desync = tx_quote_desync.as_ref().map(|_| tick.clone());
            let tick_for_rewards = if rewards_enabled {
                Some(tick.clone())
            } else {
                None
            };

            if let Err(err) = tx_quote_strategy.try_send(tick_for_strategy) {
                match err {
                    tokio::sync::mpsc::error::TrySendError::Full(_) => {
                        tracing::debug!(
                            target: "quote_fanout",
                            "strategy loop lagging; dropping quote tick"
                        );
                    }
                    tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                        tracing::warn!(
                            target: "quote_fanout",
                            "strategy quote channel closed; stopping fanout"
                        );
                        break;
                    }
                }
            }

            if tx_quote_inventory.try_send(tick).is_err() {
                tracing::debug!(
                    target: "quote_fanout",
                    "inventory loop lagging; dropping quote tick"
                );
            }

            if let (Some(tx_quote_desync), Some(tick)) = (&tx_quote_desync, tick_for_desync) {
                if tx_quote_desync.try_send(tick).is_err() {
                    tracing::debug!(
                        target: "quote_fanout",
                        "desync watchdog lagging; dropping quote tick"
                    );
                }
            }

            if rewards_enabled {
                if let Some(tick) = tick_for_rewards {
                    if tx_quote_rewards.try_send(tick).is_err() {
                        tracing::debug!(
                            target: "quote_fanout",
                            "reward engine lagging; dropping quote tick"
                        );
                    }
                } else {
                    tracing::debug!(
                        target: "quote_fanout",
                        "reward engine lagging; dropping quote tick"
                    );
                }
            }
        }
    });

    let rest = clients::clob_rest::ClobRestClient::from_config(&cfg).await?;

    let data_api = if cfg.endpoints.data_api_base_url.trim().is_empty() {
        None
    } else {
        Some(clients::data_api::DataApiClient::new(
            cfg.endpoints.data_api_base_url.clone(),
        ))
    };

    let reconciler = reconciliation::StartupReconciler::new(
        clients::gamma::GammaClient::new(cfg.endpoints.gamma_base_url.clone()),
        rest.clone(),
        data_api.clone(),
    );
    let seed_orders = !cfg.trading.startup_cancel_all;
    let reconciliation = reconciler
        .run(&cfg, &tx_events, seed_orders)
        .await
        .map_err(|e| {
            tracing::error!(
                target: "startup",
                error = %e,
                "startup reconciliation failed"
            );
            e
        })?;

    if !cfg.trading.dry_run {
        if rest.authenticated() {
            if cfg.trading.startup_cancel_all {
                if let Err(err) = rest.cancel_all_orders().await {
                    tracing::error!(
                        target: "startup",
                        error = %err,
                        "startup cancel_all_orders failed; refusing to trade"
                    );
                    return Err(err);
                }
            } else if !reconciliation.cancel_order_ids.is_empty() {
                if let Err(err) = rest
                    .cancel_orders(reconciliation.cancel_order_ids.clone())
                    .await
                {
                    tracing::warn!(
                        target: "startup",
                        error = %err,
                        "startup cancel_orders failed"
                    );
                }
            }
        } else {
            tracing::error!(
                target: "startup",
                "rest client not authenticated; refusing to trade"
            );
            return Err(error::BotError::Other(
                "rest client not authenticated; refusing to trade".to_string(),
            ));
        }
    }

    let rewards_rx = if cfg.rewards.enable_liquidity_rewards_chasing {
        let (reward_engine, rewards_rx) =
            strategy::reward_engine::RewardEngine::new(cfg.rewards.clone(), rest.clone());
        tokio::spawn(reward_engine.run(rx_quote_rewards, Some(tx_log.clone())));
        Some(rewards_rx)
    } else {
        None
    };

    let strategy_engine = strategy::engine::StrategyEngine::new(
        cfg.trading.clone(),
        cfg.inventory.clone(),
        cfg.completion.clone(),
        cfg.rewards.clone(),
        rewards_rx,
    );
    tokio::spawn(strategy_engine.run_with_logger(
        rx_quote_strategy,
        tx_exec,
        tx_completion,
        Some(tx_log.clone()),
    ));

    let mut inventory_engine =
        inventory::InventoryEngine::new(cfg.merge.clone(), cfg.alpha.clone(), cfg.oracle.clone());
    for (condition_id, flags) in &reconciliation.readiness_by_condition {
        inventory_engine.set_mergeable_readiness(condition_id, flags.mergeable);
        inventory_engine.set_redeemable_readiness(condition_id, flags.redeemable);
    }
    let tx_onchain = inventory::spawn_onchain_worker(
        inventory::OnchainWorkerConfig {
            enabled: cfg.merge.enabled && !cfg.trading.dry_run,
            wallet_mode: cfg.merge.wallet_mode,
            rpc_url: cfg.endpoints.polygon_rpc_url.clone(),
            private_key: cfg.keys.private_key.clone(),
        },
        // P5: send onchain merge/redeem updates to StateManager.
        Some(tx_events.clone()),
        Some(tx_log.clone()),
    );
    let mut inventory_loop = inventory::InventoryLoop::new(
        inventory_engine,
        inventory::InventoryExecutor::new(tx_onchain),
        cfg.merge.interval_s,
    );
    if let (Some(data_api), Some(user)) = (data_api.clone(), data_api_user.clone()) {
        inventory_loop =
            inventory_loop.with_readiness(data_api, user, cfg.merge.readiness_poll_interval_s);
    }
    tokio::spawn(inventory_loop.run(rx_quote_inventory));

    if let (Some(rx_quote_desync), Some(data_api), Some(user)) =
        (rx_quote_desync, data_api.clone(), data_api_user.clone())
    {
        tracing::info!(
            target: "desync_watchdog",
            interval_s = cfg.inventory.desync_watchdog_interval_s,
            mismatch_hold_s = cfg.inventory.desync_watchdog_mismatch_hold_s,
            max_abs_shares_diff = cfg.inventory.desync_watchdog_max_abs_shares_diff,
            "starting inventory desync watchdog"
        );
        inventory::spawn_desync_watchdog(
            inventory::DesyncWatchdogConfig {
                interval_s: cfg.inventory.desync_watchdog_interval_s,
                mismatch_hold_s: cfg.inventory.desync_watchdog_mismatch_hold_s,
                max_abs_shares_diff: cfg.inventory.desync_watchdog_max_abs_shares_diff,
            },
            rx_quote_desync,
            Arc::new(data_api),
            user,
            fatal_tx.clone(),
            shutdown_trigger.clone(),
        );
    }

    let mut order_manager = execution::order_manager::OrderManager::new(
        cfg.trading.clone(),
        rest.clone(),
        rx_user_orders,
    );
    if !cfg.trading.startup_cancel_all {
        for (slug, orders) in &reconciliation.orders_by_slug {
            if !orders.is_empty() {
                order_manager.seed_live_orders(slug, orders.clone());
            }
        }
    }
    // W7.15: forward live order updates to StateManager for rewards scoring.
    tokio::spawn(order_manager.run(rx_exec, tx_events.clone(), Some(tx_log.clone())));

    let completion_executor =
        execution::completion::CompletionExecutor::new(cfg.completion.clone(), rest.clone());
    tokio::spawn(completion_executor.run(rx_completion, Some(tx_log.clone()), cfg.trading.dry_run));

    let market_ws =
        clients::clob_ws_market::MarketWsLoop::new(cfg.endpoints.clob_ws_market_url.clone());
    tokio::spawn(market_ws.run(rx_market_ws_cmd, tx_events.clone(), Some(tx_log.clone())));

    // P0.1: pass chainlink_stale_ms explicitly to avoid config reload inside RTDS loop.
    let rtds_ws = clients::rtds_ws::RTDSLoop::new(
        cfg.endpoints.rtds_ws_url.clone(),
        cfg.oracle.chainlink_stale_ms,
    );
    let tx_log_rtds = tx_log.clone();
    tokio::spawn({
        let tx_events = tx_events.clone();
        async move {
            if let Err(e) = rtds_ws.run(tx_events, Some(tx_log_rtds)).await {
                tracing::error!(target: "rtds_ws", error = %e, "rtds loop exited");
            }
        }
    });

    if !cfg.trading.dry_run && cfg.heartbeats.enabled {
        let api = rest.heartbeat_api()?;
        let hb_cfg = cfg.heartbeats.clone();
        let shutdown_trigger = shutdown_trigger.clone();
        let fatal_tx = fatal_tx.clone();
        tokio::spawn(async move {
            if let Err(err) = clients::clob_rest::run_heartbeat_supervisor(api, hb_cfg).await {
                tracing::error!(
                    target: "heartbeats",
                    error = %err,
                    "heartbeat supervisor exited; shutting down"
                );
                let _ = fatal_tx.send(Some(format!("heartbeat supervisor exited: {err}")));
                shutdown_trigger.trigger();
            }
        });
    }

    if !cfg.trading.dry_run {
        let user_ws =
            clients::clob_ws_user::UserWsLoop::from_config(&cfg, Vec::new(), Some(tx_user_orders))?;
        let tx_log_user = tx_log.clone();
        tokio::spawn({
            let tx_events = tx_events.clone();
            async move {
                if let Err(e) = user_ws.run(tx_events, Some(tx_log_user)).await {
                    tracing::error!(target: "clob_ws_user", error = %e, "user ws loop exited");
                }
            }
        });
    }

    let gamma = clients::gamma::GammaClient::new(cfg.endpoints.gamma_base_url.clone());
    let clob_public =
        clients::clob_public::ClobPublicClient::new(cfg.endpoints.clob_rest_base_url.clone());
    let discovery = market_discovery::MarketDiscoveryLoop::new(
        gamma,
        clob_public,
        tx_events.clone(),
        Some(tx_market_ws_cmd),
        cfg.infra.market_discovery_interval_ms,
        cfg.infra.market_discovery_grace_s, // grace window for previous market tracking
    );
    tokio::spawn(async move {
        if let Err(e) = discovery.run().await {
            tracing::error!(target: "market_discovery", error = %e, "market discovery loop exited");
        }
    });

    let mut tick = tokio::time::interval(tokio::time::Duration::from_millis(
        cfg.infra.quote_tick_interval_ms,
    ));
    loop {
        tokio::select! {
            _ = shutdown.clone().wait() => {
                tracing::info!(target: "shutdown", "main loop received shutdown");
                break;
            }
            _ = tick.tick() => {
                let now_ms = now_ms();
                if tx_events
                    .send(state::state_manager::AppEvent::TimerTick { now_ms })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }

    if rest.authenticated() {
        if let Err(err) = rest.cancel_all_orders().await {
            tracing::warn!(target: "shutdown", error = %err, "best-effort cancel_all_orders failed");
        }
    }

    if let Some(reason) = fatal_rx.borrow().clone() {
        return Err(crate::error::BotError::Other(reason));
    }

    Ok(())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
