#![allow(dead_code)]

use std::collections::HashMap;
use std::str::FromStr as _;
use std::time::Duration;

use alloy::providers::{DynProvider, Provider as _, ProviderBuilder};
use alloy::sol;
use alloy_signer_local::PrivateKeySigner;
use serde_json::json;
use tokio::sync::mpsc;

use polymarket_client_sdk::auth::Signer as _;
use polymarket_client_sdk::ctf::types::{MergePositionsRequest, RedeemPositionsRequest};
use polymarket_client_sdk::ctf::Client as CtfClient;
use polymarket_client_sdk::types::{B256, ChainId, U256};
use polymarket_client_sdk::{contract_config, POLYGON};

use crate::config::MergeWalletMode;
use crate::error::{BotError, BotResult};
use crate::persistence::LogEvent;
use crate::state::state_manager::AppEvent;

const READINESS_TTL_MS: i64 = 30_000;

sol! {
    #[sol(rpc)]
    interface IConditionalTokensRead {
        function getOutcomeSlotCount(bytes32 conditionId) external view returns (uint256);
        function payoutDenominator(bytes32 conditionId) external view returns (uint256);
    }
}

type ConditionalTokensRead = IConditionalTokensRead::IConditionalTokensReadInstance<DynProvider>;

#[derive(Debug, Clone)]
pub struct OnchainWorkerConfig {
    pub enabled: bool,
    pub wallet_mode: MergeWalletMode,
    pub rpc_url: String,
    pub private_key: Option<String>,
}

#[derive(Debug, Clone)]
pub enum OnchainRequest {
    Merge {
        condition_id: String,
        qty_base_units: u64,
    },
    Redeem {
        condition_id: String,
    },
}

pub fn spawn_onchain_worker(
    cfg: OnchainWorkerConfig,
    tx_events: Option<mpsc::Sender<AppEvent>>,
    log_tx: Option<mpsc::Sender<LogEvent>>,
) -> mpsc::Sender<OnchainRequest> {
    let (tx, mut rx) = mpsc::channel::<OnchainRequest>(256);

    tokio::spawn(async move {
        if !cfg.enabled {
            tracing::info!(target: "onchain_worker", "onchain worker disabled");
        }

        let chain_id: ChainId = POLYGON;
        let contract_cfg = contract_config(chain_id, false)
            .ok_or_else(|| BotError::Other(format!("missing contract config for chain {chain_id}")));
        let (collateral, conditional_tokens) = match contract_cfg {
            Ok(c) => (c.collateral, c.conditional_tokens),
            Err(err) => {
                tracing::error!(target: "onchain_worker", error = %err, "cannot initialize chain config");
                // Drain requests to avoid blocking senders.
                while rx.recv().await.is_some() {}
                return;
            }
        };

        let ctf = match cfg.wallet_mode {
            MergeWalletMode::Eoa => match build_ctf_client(&cfg, chain_id).await {
                Ok(client) => Some(client),
                Err(err) => {
                    tracing::error!(target: "onchain_worker", error = %err, "failed to initialize EOA CTF client; merges/redeems disabled");
                    None
                }
            },
            MergeWalletMode::Relayer => {
                tracing::warn!(
                    target: "onchain_worker",
                    "merge wallet_mode=RELAYER requested but relayer integration is not implemented; merges/redeems disabled"
                );
                None
            }
        };
        let ctf_read = ctf
            .as_ref()
            .map(|client| IConditionalTokensRead::new(conditional_tokens, client.provider().clone()));
        let mut readiness_cache = ReadinessCache::default();

        while let Some(req) = rx.recv().await {
            if !cfg.enabled {
                continue;
            }
            let Some(ctf) = ctf.as_ref() else {
                continue;
            };
            let Some(ctf_read) = ctf_read.as_ref() else {
                continue;
            };
            let ts_ms = now_ms();

            match req {
                OnchainRequest::Merge {
                    condition_id,
                    qty_base_units,
                } => {
                    if qty_base_units == 0 {
                        continue;
                    }
                    match condition_prepared(
                        ctf_read,
                        &mut readiness_cache,
                        &condition_id,
                        ts_ms,
                    )
                    .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            tracing::info!(
                                target: "onchain_worker",
                                condition_id = %condition_id,
                                "merge blocked; condition not prepared"
                            );
                            continue;
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "onchain_worker",
                                condition_id = %condition_id,
                                error = %err,
                                "merge readiness check failed"
                            );
                            continue;
                        }
                    }
                    match merge_once(ctf, collateral, &condition_id, qty_base_units).await {
                        Ok(resp) => {
                            tracing::info!(
                                target: "onchain_worker",
                                condition_id = %condition_id,
                                qty_base_units,
                                tx_hash = %resp.transaction_hash,
                                block_number = %resp.block_number,
                                "merge_positions submitted"
                            );
                            log_onchain_event(
                                &log_tx,
                                ts_ms,
                                "onchain.merge",
                                json!({
                                    "condition_id": condition_id,
                                    "qty_base_units": qty_base_units,
                                    "tx_hash": format!("{}", resp.transaction_hash),
                                    "block_number": resp.block_number.to_string(),
                                }),
                            );
                            emit_state_event(
                                &tx_events,
                                AppEvent::OnchainMerge {
                                    condition_id: condition_id.clone(),
                                    qty_base_units,
                                    ts_ms,
                                },
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "onchain_worker",
                                condition_id = %condition_id,
                                qty_base_units,
                                error = %err,
                                "merge_positions failed"
                            );
                            log_onchain_error(
                                &log_tx,
                                ts_ms,
                                "onchain.merge_error",
                                &condition_id,
                                &err.to_string(),
                            );
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                    }
                }
                OnchainRequest::Redeem { condition_id } => {
                    match condition_resolved(
                        ctf_read,
                        &mut readiness_cache,
                        &condition_id,
                        ts_ms,
                    )
                    .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            tracing::info!(
                                target: "onchain_worker",
                                condition_id = %condition_id,
                                "redeem blocked; condition not resolved"
                            );
                            continue;
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "onchain_worker",
                                condition_id = %condition_id,
                                error = %err,
                                "redeem readiness check failed"
                            );
                            continue;
                        }
                    }
                    match redeem_once(ctf, collateral, &condition_id).await {
                        Ok(resp) => {
                            tracing::info!(
                                target: "onchain_worker",
                                condition_id = %condition_id,
                                tx_hash = %resp.transaction_hash,
                                block_number = %resp.block_number,
                                "redeem_positions submitted"
                            );
                            log_onchain_event(
                                &log_tx,
                                ts_ms,
                                "onchain.redeem",
                                json!({
                                    "condition_id": condition_id,
                                    "tx_hash": format!("{}", resp.transaction_hash),
                                    "block_number": resp.block_number.to_string(),
                                }),
                            );
                            emit_state_event(
                                &tx_events,
                                AppEvent::OnchainRedeem {
                                    condition_id: condition_id.clone(),
                                    ts_ms,
                                },
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "onchain_worker",
                                condition_id = %condition_id,
                                error = %err,
                                "redeem_positions failed"
                            );
                            log_onchain_error(
                                &log_tx,
                                ts_ms,
                                "onchain.redeem_error",
                                &condition_id,
                                &err.to_string(),
                            );
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                    }
                }
            }
        }
    });

    tx
}

async fn build_ctf_client(
    cfg: &OnchainWorkerConfig,
    chain_id: ChainId,
) -> BotResult<CtfClient<alloy::providers::DynProvider>> {
    let private_key = cfg
        .private_key
        .as_deref()
        .unwrap_or_default()
        .trim();
    if private_key.is_empty() {
        return Err(BotError::Config(
            "missing required key for onchain: PMMB_KEYS__PRIVATE_KEY".to_string(),
        ));
    }
    let pk = private_key.strip_prefix("0x").unwrap_or(private_key);
    let signer: PrivateKeySigner = pk
        .parse::<PrivateKeySigner>()
        .map_err(|e| BotError::Other(format!("invalid private key: {e}")))?
        .with_chain_id(Some(chain_id));

    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect(cfg.rpc_url.as_str())
        .await
        .map_err(|e| BotError::Other(format!("polygon rpc connect failed: {e}")))?;
    let provider = provider.erased();

    CtfClient::new(provider, chain_id).map_err(|e| BotError::Other(format!("ctf init failed: {e}")))
}

async fn merge_once(
    ctf: &CtfClient<alloy::providers::DynProvider>,
    collateral: polymarket_client_sdk::types::Address,
    condition_id: &str,
    qty_base_units: u64,
) -> BotResult<polymarket_client_sdk::ctf::types::MergePositionsResponse> {
    let condition = parse_condition_id(condition_id)?;
    let amount = U256::from(qty_base_units);
    let req = MergePositionsRequest::for_binary_market(collateral, condition, amount);
    ctf.merge_positions(&req)
        .await
        .map_err(|e| BotError::Other(format!("ctf merge_positions failed: {e}")))
}

async fn redeem_once(
    ctf: &CtfClient<alloy::providers::DynProvider>,
    collateral: polymarket_client_sdk::types::Address,
    condition_id: &str,
) -> BotResult<polymarket_client_sdk::ctf::types::RedeemPositionsResponse> {
    let condition = parse_condition_id(condition_id)?;
    let req = RedeemPositionsRequest::for_binary_market(collateral, condition);
    ctf.redeem_positions(&req)
        .await
        .map_err(|e| BotError::Other(format!("ctf redeem_positions failed: {e}")))
}

fn parse_condition_id(condition_id: &str) -> BotResult<B256> {
    let s = condition_id.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    B256::from_str(&format!("0x{s}"))
        .map_err(|e| BotError::Other(format!("invalid condition_id {condition_id}: {e}")))
}

fn log_onchain_event(
    log_tx: &Option<mpsc::Sender<LogEvent>>,
    ts_ms: i64,
    event: &str,
    payload: serde_json::Value,
) {
    let Some(tx) = log_tx else {
        return;
    };
    let _ = tx.try_send(LogEvent {
        ts_ms,
        event: event.to_string(),
        payload,
    });
}

fn log_onchain_error(
    log_tx: &Option<mpsc::Sender<LogEvent>>,
    ts_ms: i64,
    event: &str,
    condition_id: &str,
    message: &str,
) {
    log_onchain_event(
        log_tx,
        ts_ms,
        event,
        json!({
            "condition_id": condition_id,
            "message": message,
        }),
    );
}

#[derive(Debug, Default)]
struct ReadinessCache {
    prepared: HashMap<String, CacheEntry>,
    resolved: HashMap<String, CacheEntry>,
}

#[derive(Debug, Clone, Copy)]
struct CacheEntry {
    ready: bool,
    last_check_ms: i64,
}

async fn condition_prepared(
    ctf_read: &ConditionalTokensRead,
    cache: &mut ReadinessCache,
    condition_id: &str,
    now_ms: i64,
) -> BotResult<bool> {
    if let Some(cached) = cached_readiness(&cache.prepared, condition_id, now_ms) {
        return Ok(cached);
    }
    let condition = parse_condition_id(condition_id)?;
    let count = match ctf_read.getOutcomeSlotCount(condition).call().await {
        Ok(value) => value,
        Err(err) => {
            cache.prepared.insert(
                condition_id.to_string(),
                CacheEntry {
                    ready: false,
                    last_check_ms: now_ms,
                },
            );
            return Err(BotError::Other(format!(
                "ctf getOutcomeSlotCount failed: {err}"
            )));
        }
    };
    let ready = !count.is_zero();
    cache.prepared.insert(
        condition_id.to_string(),
        CacheEntry {
            ready,
            last_check_ms: now_ms,
        },
    );
    Ok(ready)
}

async fn condition_resolved(
    ctf_read: &ConditionalTokensRead,
    cache: &mut ReadinessCache,
    condition_id: &str,
    now_ms: i64,
) -> BotResult<bool> {
    if let Some(cached) = cached_readiness(&cache.resolved, condition_id, now_ms) {
        return Ok(cached);
    }
    let condition = parse_condition_id(condition_id)?;
    let denom = match ctf_read.payoutDenominator(condition).call().await {
        Ok(value) => value,
        Err(err) => {
            cache.resolved.insert(
                condition_id.to_string(),
                CacheEntry {
                    ready: false,
                    last_check_ms: now_ms,
                },
            );
            return Err(BotError::Other(format!(
                "ctf payoutDenominator failed: {err}"
            )));
        }
    };
    let ready = !denom.is_zero();
    cache.resolved.insert(
        condition_id.to_string(),
        CacheEntry {
            ready,
            last_check_ms: now_ms,
        },
    );
    Ok(ready)
}

fn cached_readiness(
    map: &HashMap<String, CacheEntry>,
    condition_id: &str,
    now_ms: i64,
) -> Option<bool> {
    let entry = map.get(condition_id)?;
    if entry.ready {
        return Some(true);
    }
    if now_ms.saturating_sub(entry.last_check_ms) < READINESS_TTL_MS {
        return Some(false);
    }
    None
}

fn emit_state_event(tx: &Option<mpsc::Sender<AppEvent>>, event: AppEvent) {
    let Some(tx) = tx else {
        return;
    };
    if tx.try_send(event).is_err() {
        tracing::warn!(target: "onchain_worker", "state event queue full; dropping onchain update");
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
