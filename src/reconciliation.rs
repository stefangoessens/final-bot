use std::collections::{BTreeSet, HashMap};

use tokio::sync::mpsc::Sender;

use polymarket_client_sdk::clob::types::Side as SdkSide;

use crate::clients::clob_rest::ClobRestClient;
use crate::clients::data_api::{DataApiClient, DataApiPosition};
use crate::clients::gamma::GammaClient;
use crate::config::AppConfig;
use crate::error::{BotError, BotResult};
use crate::state::inventory::{InventorySide, TokenSide};
use crate::state::market_state::MarketIdentity;
use crate::state::order_state::{LiveOrder, OrderStatus};
use crate::state::state_manager::{AppEvent, InventorySeed, OrderSeed};
use crate::time::{interval_start_s, now_s, BTC_15M_INTERVAL_S};

#[derive(Debug, Clone, Default)]
pub struct ReconciliationResult {
    pub orders_by_slug: HashMap<String, Vec<LiveOrder>>,
    pub inventory_by_slug: HashMap<String, InventorySeed>,
    pub readiness_by_condition: HashMap<String, ReadinessFlags>,
    pub cancel_order_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReadinessFlags {
    pub mergeable: bool,
    pub redeemable: bool,
}

pub struct StartupReconciler {
    gamma: GammaClient,
    rest: ClobRestClient,
    data_api: Option<DataApiClient>,
}

#[derive(Debug, Clone)]
struct OpenOrderLite {
    id: String,
    side: SdkSide,
    asset_id: String,
    price: f64,
    original_size: f64,
    size_matched: f64,
    created_at_ms: i64,
}

impl OpenOrderLite {
    fn try_from_sdk(
        order: polymarket_client_sdk::clob::types::response::OpenOrderResponse,
    ) -> BotResult<Self> {
        Ok(Self {
            id: order.id,
            side: order.side,
            asset_id: order.asset_id.to_string(),
            price: decimal_to_f64(&order.price)?,
            original_size: decimal_to_f64(&order.original_size)?,
            size_matched: decimal_to_f64(&order.size_matched)?,
            created_at_ms: order.created_at.timestamp_millis(),
        })
    }
}

impl StartupReconciler {
    pub fn new(gamma: GammaClient, rest: ClobRestClient, data_api: Option<DataApiClient>) -> Self {
        Self {
            gamma,
            rest,
            data_api,
        }
    }

    pub async fn run(
        &self,
        cfg: &AppConfig,
        tx_events: &Sender<AppEvent>,
        seed_orders: bool,
    ) -> BotResult<ReconciliationResult> {
        let now = now_s();
        let tracked = tracked_slugs(now, cfg.infra.market_discovery_grace_s);
        let mut identities = Vec::new();

        for (slug, interval_start) in &tracked {
            if let Some(market) = self.gamma.fetch_market_by_slug(slug).await? {
                let identity = market.to_market_identity(*interval_start)?;
                tx_events
                    .send(AppEvent::MarketDiscovered(identity.clone()))
                    .await
                    .map_err(|_| {
                        BotError::Other(
                            "startup reconciliation: state manager channel closed".to_string(),
                        )
                    })?;
                identities.push(identity);
            }
        }

        let tracked_slugs_vec: Vec<String> = tracked.iter().map(|(slug, _)| slug.clone()).collect();
        tx_events
            .send(AppEvent::SetTrackedMarkets {
                slugs: tracked_slugs_vec,
            })
            .await
            .map_err(|_| {
                BotError::Other("startup reconciliation: state manager channel closed".to_string())
            })?;

        let mut result = ReconciliationResult::default();

        if seed_orders && self.rest.authenticated() {
            for identity in &identities {
                let open_orders = self
                    .rest
                    .get_active_orders_for_market(&identity.condition_id)
                    .await?;
                let open_orders: Vec<OpenOrderLite> = open_orders
                    .into_iter()
                    .map(OpenOrderLite::try_from_sdk)
                    .collect::<BotResult<Vec<_>>>()?;
                let (seed, cancels) = map_open_orders(identity, open_orders)?;
                if !seed.orders.is_empty() {
                    if seed_orders {
                        tx_events
                            .send(AppEvent::SeedOrders(seed.clone()))
                            .await
                            .map_err(|_| {
                                BotError::Other(
                                    "startup reconciliation: state manager channel closed"
                                        .to_string(),
                                )
                            })?;
                    }
                    result
                        .orders_by_slug
                        .insert(seed.slug.clone(), seed.orders.clone());
                }
                result.cancel_order_ids.extend(cancels);
            }
        }

        if let Some(data_api) = self.data_api.as_ref() {
            if let Some(user) = resolve_data_api_user(cfg) {
                let condition_ids: Vec<String> =
                    identities.iter().map(|m| m.condition_id.clone()).collect();
                let positions = data_api
                    .fetch_positions(&user, Some(&condition_ids))
                    .await?;
                let (inventory_seeds, readiness) = map_positions(&identities, &positions);
                for seed in inventory_seeds.values() {
                    tx_events
                        .send(AppEvent::SeedInventory(seed.clone()))
                        .await
                        .map_err(|_| {
                            BotError::Other(
                                "startup reconciliation: state manager channel closed".to_string(),
                            )
                        })?;
                }
                result.inventory_by_slug = inventory_seeds;
                result.readiness_by_condition = readiness;
            } else {
                tracing::warn!(
                    target: "startup",
                    "data api user address unavailable; skipping position seed"
                );
            }
        }

        Ok(result)
    }
}

fn tracked_slugs(now_s: i64, grace_s: i64) -> Vec<(String, i64)> {
    let cur_start_s = interval_start_s(now_s);
    let next_start_s = cur_start_s + BTC_15M_INTERVAL_S;

    let mut tracked: BTreeSet<(String, i64)> = BTreeSet::new();
    tracked.insert((format!("btc-updown-15m-{cur_start_s}"), cur_start_s));
    tracked.insert((format!("btc-updown-15m-{next_start_s}"), next_start_s));

    if grace_s > 0 && now_s.saturating_sub(cur_start_s) <= grace_s {
        let prev_start_s = cur_start_s - BTC_15M_INTERVAL_S;
        tracked.insert((format!("btc-updown-15m-{prev_start_s}"), prev_start_s));
    }

    tracked.into_iter().collect()
}

fn map_open_orders(
    identity: &MarketIdentity,
    orders: Vec<OpenOrderLite>,
) -> BotResult<(OrderSeed, Vec<String>)> {
    let mut by_token: HashMap<String, Vec<_>> = HashMap::new();
    let mut cancels = Vec::new();

    for order in orders {
        if order.side != SdkSide::Buy {
            cancels.push(order.id);
            continue;
        }
        let token_id = order.asset_id.clone();
        if token_id != identity.token_up && token_id != identity.token_down {
            cancels.push(order.id);
            continue;
        }
        by_token.entry(token_id).or_default().push(order);
    }

    let mut live_orders = Vec::new();
    for (token_id, mut token_orders) in by_token {
        token_orders.sort_by(|a, b| {
            b.price
                .partial_cmp(&a.price)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.created_at_ms.cmp(&b.created_at_ms))
        });

        for (level, order) in token_orders.into_iter().enumerate() {
            let price = order.price;
            let size = order.original_size;
            let matched = order.size_matched;
            let remaining = (size - matched).max(0.0);
            let status = if remaining <= 0.0 {
                OrderStatus::Filled
            } else if matched > 0.0 {
                OrderStatus::PartiallyFilled
            } else {
                OrderStatus::Open
            };
            live_orders.push(LiveOrder {
                order_id: order.id,
                token_id: token_id.clone(),
                level,
                price,
                size,
                remaining,
                status,
                last_update_ms: order.created_at_ms,
            });
        }
    }

    Ok((
        OrderSeed {
            slug: identity.slug.clone(),
            orders: live_orders,
            ts_ms: now_ms(),
        },
        cancels,
    ))
}

fn map_positions(
    identities: &[MarketIdentity],
    positions: &[DataApiPosition],
) -> (
    HashMap<String, InventorySeed>,
    HashMap<String, ReadinessFlags>,
) {
    let mut by_condition: HashMap<String, &MarketIdentity> = HashMap::new();
    for identity in identities {
        by_condition.insert(identity.condition_id.clone(), identity);
    }

    let mut inventory_by_slug: HashMap<String, InventorySeed> = HashMap::new();
    let mut readiness: HashMap<String, ReadinessFlags> = HashMap::new();

    for pos in positions {
        let Some(identity) = by_condition.get(&pos.condition_id) else {
            continue;
        };
        let side = map_position_side(identity, pos);
        let Some(side) = side else {
            continue;
        };

        let entry = inventory_by_slug
            .entry(identity.slug.clone())
            .or_insert_with(|| InventorySeed {
                slug: identity.slug.clone(),
                up: InventorySide::default(),
                down: InventorySide::default(),
                ts_ms: now_ms(),
            });

        let notional = if pos.size.is_finite() && pos.avg_price.is_finite() {
            pos.size * pos.avg_price
        } else {
            0.0
        };

        match side {
            TokenSide::Up => {
                entry.up.shares += pos.size;
                entry.up.notional_usdc += notional;
            }
            TokenSide::Down => {
                entry.down.shares += pos.size;
                entry.down.notional_usdc += notional;
            }
        }

        let flags = readiness.entry(identity.condition_id.clone()).or_default();
        flags.mergeable |= pos.mergeable;
        flags.redeemable |= pos.redeemable;
    }

    (inventory_by_slug, readiness)
}

fn map_position_side(identity: &MarketIdentity, pos: &DataApiPosition) -> Option<TokenSide> {
    if pos.asset == identity.token_up {
        return Some(TokenSide::Up);
    }
    if pos.asset == identity.token_down {
        return Some(TokenSide::Down);
    }
    if pos.outcome.eq_ignore_ascii_case("Up") {
        return Some(TokenSide::Up);
    }
    if pos.outcome.eq_ignore_ascii_case("Down") {
        return Some(TokenSide::Down);
    }
    None
}

fn decimal_to_f64(value: &polymarket_client_sdk::types::Decimal) -> BotResult<f64> {
    value
        .to_string()
        .parse::<f64>()
        .map_err(|e| BotError::Other(format!("invalid decimal: {e}")))
}

pub fn resolve_data_api_user(cfg: &AppConfig) -> Option<String> {
    if let Some(user) = cfg.keys.data_api_user.as_deref() {
        if !user.trim().is_empty() {
            return Some(user.trim().to_string());
        }
    }
    if let Some(funder) = cfg.keys.funder_address.as_deref() {
        if !funder.trim().is_empty() {
            return Some(funder.trim().to_string());
        }
    }
    if cfg.keys.wallet_mode == crate::config::WalletMode::ProxySafe {
        if let Some(private_key) = cfg.keys.private_key.as_deref() {
            if !private_key.trim().is_empty() {
                let pk = private_key
                    .trim()
                    .strip_prefix("0x")
                    .unwrap_or(private_key.trim());
                if let Ok(signer) = pk.parse::<alloy_signer_local::PrivateKeySigner>() {
                    if let Some(proxy) = polymarket_client_sdk::derive_proxy_wallet(
                        signer.address(),
                        polymarket_client_sdk::POLYGON,
                    ) {
                        return Some(proxy.to_string());
                    }
                    return Some(signer.address().to_string());
                }
            }
        }
    }
    if let Some(private_key) = cfg.keys.private_key.as_deref() {
        if !private_key.trim().is_empty() {
            let pk = private_key
                .trim()
                .strip_prefix("0x")
                .unwrap_or(private_key.trim());
            if let Ok(signer) = pk.parse::<alloy_signer_local::PrivateKeySigner>() {
                return Some(signer.address().to_string());
            }
        }
    }
    None
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

    fn make_identity() -> MarketIdentity {
        MarketIdentity {
            slug: "btc-updown-15m-0".to_string(),
            interval_start_ts: 0,
            interval_end_ts: 900,
            condition_id: "0x01".to_string(),
            token_up: "1".to_string(),
            token_down: "2".to_string(),
            active: true,
            closed: false,
            accepting_orders: true,
            restricted: false,
        }
    }

    fn make_order(id: &str, asset_id: &str, price: f64, created_at_ms: i64) -> OpenOrderLite {
        OpenOrderLite {
            id: id.to_string(),
            side: SdkSide::Buy,
            asset_id: asset_id.to_string(),
            price,
            original_size: 1.0,
            size_matched: 0.0,
            created_at_ms,
        }
    }

    #[test]
    fn map_positions_sets_inventory() {
        let identity = make_identity();
        let positions = vec![DataApiPosition {
            condition_id: "0x01".to_string(),
            asset: "1".to_string(),
            outcome: "Up".to_string(),
            size: 2.0,
            avg_price: 0.4,
            mergeable: true,
            redeemable: false,
        }];

        let (inventory, readiness) = map_positions(&[identity.clone()], &positions);
        let seed = inventory.get(&identity.slug).expect("seed");
        assert_eq!(seed.up.shares, 2.0);
        assert_eq!(seed.up.notional_usdc, 0.8);
        assert!(readiness.get(&identity.condition_id).unwrap().mergeable);
    }

    #[test]
    fn map_open_orders_assigns_levels() {
        let identity = make_identity();
        let order1 = make_order("o1", "1", 0.6, 100);
        let order2 = make_order("o2", "1", 0.5, 200);
        let (seed, _) = map_open_orders(&identity, vec![order1, order2]).expect("seed");
        assert_eq!(seed.orders.len(), 2);
        assert_eq!(seed.orders[0].level, 0);
        assert!(seed.orders[0].price >= seed.orders[1].price);
    }
}
