use std::collections::{BTreeSet, HashMap};

use tokio::sync::mpsc::Sender;

use crate::clients::clob_public::ClobPublicClient;
use crate::clients::clob_ws_market::MarketWsCommand;
use crate::clients::gamma::GammaClient;
use crate::error::BotResult;
use crate::state::market_state::MarketIdentity;
use crate::state::state_manager::{AppEvent, TickSizeSeed};
use crate::time::{interval_start_s, now_s, BTC_15M_INTERVAL_S};

#[derive(Debug, Clone)]
struct DiscoverySlugs {
    cur_start_s: i64,
    next_start_s: i64,
    prev_start_s: Option<i64>,
    cur_slug: String,
    next_slug: String,
    prev_slug: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IdentityFingerprint {
    interval_start_ts: i64,
    interval_end_ts: i64,
    condition_id: String,
    token_up: String,
    token_down: String,
    active: bool,
    closed: bool,
    accepting_orders: bool,
    restricted: bool,
}

impl IdentityFingerprint {
    fn from_identity(identity: &MarketIdentity) -> Self {
        Self {
            interval_start_ts: identity.interval_start_ts,
            interval_end_ts: identity.interval_end_ts,
            condition_id: identity.condition_id.clone(),
            token_up: identity.token_up.clone(),
            token_down: identity.token_down.clone(),
            active: identity.active,
            closed: identity.closed,
            accepting_orders: identity.accepting_orders,
            restricted: identity.restricted,
        }
    }
}

fn discovery_slugs(now_s: i64, grace_s: i64) -> DiscoverySlugs {
    let cur_start_s = interval_start_s(now_s);
    let next_start_s = cur_start_s + BTC_15M_INTERVAL_S;

    let mut prev_start_s = None;
    let mut prev_slug = None;
    if grace_s > 0 && now_s.saturating_sub(cur_start_s) <= grace_s {
        let start = cur_start_s - BTC_15M_INTERVAL_S;
        prev_start_s = Some(start);
        prev_slug = Some(format!("btc-updown-15m-{start}"));
    }

    DiscoverySlugs {
        cur_start_s,
        next_start_s,
        prev_start_s,
        cur_slug: format!("btc-updown-15m-{cur_start_s}"),
        next_slug: format!("btc-updown-15m-{next_start_s}"),
        prev_slug,
    }
}

fn tracked_slugs(slugs: &DiscoverySlugs) -> BTreeSet<String> {
    let mut tracked = BTreeSet::new();
    tracked.insert(slugs.cur_slug.clone());
    tracked.insert(slugs.next_slug.clone());
    if let Some(prev) = &slugs.prev_slug {
        tracked.insert(prev.clone());
    }
    tracked
}

async fn fetch_or_cached_identity(
    gamma: &GammaClient,
    slug: &str,
    interval_start: i64,
    cache: &mut HashMap<String, MarketIdentity>,
    fetch: bool,
) -> BotResult<Option<MarketIdentity>> {
    if fetch {
        match gamma.fetch_market_by_slug(slug).await? {
            Some(market) => {
                let identity = market.to_market_identity(interval_start)?;
                cache.insert(slug.to_string(), identity.clone());
                Ok(Some(identity))
            }
            None => Ok(cache.get(slug).cloned()),
        }
    } else {
        Ok(cache.get(slug).cloned())
    }
}

#[derive(Debug, Clone)]
pub struct MarketDiscoveryLoop {
    gamma: GammaClient,
    clob_public: ClobPublicClient,
    tx_events: Sender<AppEvent>,
    tx_market_ws: Option<Sender<MarketWsCommand>>,
    poll_interval_ms: u64,
    grace_s: i64,
}

impl MarketDiscoveryLoop {
    pub fn new(
        gamma: GammaClient,
        clob_public: ClobPublicClient,
        tx_events: Sender<AppEvent>,
        tx_market_ws: Option<Sender<MarketWsCommand>>,
        poll_interval_ms: u64,
        grace_s: i64,
    ) -> Self {
        Self {
            gamma,
            clob_public,
            tx_events,
            tx_market_ws,
            poll_interval_ms,
            grace_s: grace_s.max(0),
        }
    }

    pub async fn run(self) -> BotResult<()> {
        let mut tick =
            tokio::time::interval(tokio::time::Duration::from_millis(self.poll_interval_ms));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut last_tracked: BTreeSet<String> = BTreeSet::new();
        let mut subscribed_tokens: BTreeSet<String> = BTreeSet::new();
        let mut tick_size_seeded: BTreeSet<String> = BTreeSet::new();
        let mut last_sent: HashMap<String, IdentityFingerprint> = HashMap::new();
        let mut identity_cache: HashMap<String, MarketIdentity> = HashMap::new();

        loop {
            tick.tick().await;

            let now = now_s();
            let slugs = discovery_slugs(now, self.grace_s);
            let tracked = tracked_slugs(&slugs);

            if tracked != last_tracked {
                last_tracked = tracked.clone();
                let tracked_slugs_vec: Vec<String> = tracked.iter().cloned().collect();
                let _ = self
                    .tx_events
                    .send(AppEvent::SetTrackedMarkets {
                        slugs: tracked_slugs_vec,
                    })
                    .await;
                tracing::info!(
                    target: "market_discovery",
                    current_slug = %slugs.cur_slug,
                    next_slug = %slugs.next_slug,
                    prev_slug = slugs.prev_slug.as_deref().unwrap_or(""),
                    "tracking current+next BTC 15m markets"
                );
            }

            last_sent.retain(|slug, _| tracked.contains(slug));
            identity_cache.retain(|slug, _| tracked.contains(slug));

            let mut desired_tokens: BTreeSet<String> = BTreeSet::new();

            for (slug, interval_start, subscribe, fetch) in [
                (slugs.cur_slug.clone(), slugs.cur_start_s, true, true),
                (slugs.next_slug.clone(), slugs.next_start_s, true, true),
            ] {
                let identity = fetch_or_cached_identity(
                    &self.gamma,
                    &slug,
                    interval_start,
                    &mut identity_cache,
                    fetch,
                )
                .await?;
                if let Some(identity) = identity {
                    let fingerprint = IdentityFingerprint::from_identity(&identity);
                    let changed = last_sent
                        .get(&identity.slug)
                        .map(|prev| prev != &fingerprint)
                        .unwrap_or(true);

                    let token_up = identity.token_up.clone();
                    let token_down = identity.token_down.clone();

                    if changed {
                        last_sent.insert(identity.slug.clone(), fingerprint);
                        tracing::info!(
                            target: "market_discovery",
                            slug = %identity.slug,
                            token_up = %identity.token_up,
                            token_down = %identity.token_down,
                            accepting_orders = identity.accepting_orders,
                            restricted = identity.restricted,
                            active = identity.active,
                            closed = identity.closed,
                            end_ts = identity.interval_end_ts,
                            "market discovered"
                        );
                        let _ = self
                            .tx_events
                            .send(AppEvent::MarketDiscovered(identity))
                            .await;
                    } else {
                        tracing::debug!(
                            target: "market_discovery",
                            slug = %identity.slug,
                            "market identity unchanged; skip emit"
                        );
                    }

                    if subscribe {
                        desired_tokens.insert(token_up);
                        desired_tokens.insert(token_down);
                    }
                } else {
                    tracing::debug!(target: "market_discovery", %slug, "market not found yet");
                }
            }

            if let (Some(prev_slug), Some(prev_start)) =
                (slugs.prev_slug.clone(), slugs.prev_start_s)
            {
                let fetch_prev = match identity_cache.get(&prev_slug) {
                    Some(identity) => !identity.closed,
                    None => true,
                };
                let identity = fetch_or_cached_identity(
                    &self.gamma,
                    &prev_slug,
                    prev_start,
                    &mut identity_cache,
                    fetch_prev,
                )
                .await?;

                if let Some(identity) = identity {
                    let fingerprint = IdentityFingerprint::from_identity(&identity);
                    let changed = last_sent
                        .get(&identity.slug)
                        .map(|prev| prev != &fingerprint)
                        .unwrap_or(true);

                    if changed {
                        last_sent.insert(identity.slug.clone(), fingerprint);
                        tracing::info!(
                            target: "market_discovery",
                            slug = %identity.slug,
                            token_up = %identity.token_up,
                            token_down = %identity.token_down,
                            accepting_orders = identity.accepting_orders,
                            restricted = identity.restricted,
                            active = identity.active,
                            closed = identity.closed,
                            end_ts = identity.interval_end_ts,
                            "market discovered"
                        );
                        let _ = self
                            .tx_events
                            .send(AppEvent::MarketDiscovered(identity))
                            .await;
                    } else {
                        tracing::debug!(
                            target: "market_discovery",
                            slug = %identity.slug,
                            "market identity unchanged; skip emit"
                        );
                    }
                }
            }

            if !desired_tokens.is_empty() {
                let to_seed: Vec<String> = desired_tokens
                    .iter()
                    .filter(|token| !tick_size_seeded.contains(*token))
                    .cloned()
                    .collect();
                if !to_seed.is_empty() {
                    match self.clob_public.fetch_tick_sizes(&to_seed).await {
                        Ok(ticks) => {
                            let ts_ms = now_ms();
                            for (token_id, tick_size) in ticks {
                                tick_size_seeded.insert(token_id.clone());
                                let _ = self
                                    .tx_events
                                    .send(AppEvent::TickSizeSeed(TickSizeSeed {
                                        token_id,
                                        tick_size,
                                        ts_ms,
                                    }))
                                    .await;
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "market_discovery",
                                error = %err,
                                "tick size seed failed"
                            );
                        }
                    }
                }
            }

            if let Some(tx_market_ws) = &self.tx_market_ws {
                let to_sub: Vec<String> = desired_tokens
                    .difference(&subscribed_tokens)
                    .cloned()
                    .collect();
                let to_unsub: Vec<String> = subscribed_tokens
                    .difference(&desired_tokens)
                    .cloned()
                    .collect();

                if !to_unsub.is_empty() {
                    tracing::info!(
                        target: "market_discovery",
                        count = to_unsub.len(),
                        "market ws unsubscribe tokens"
                    );
                    let _ = tx_market_ws
                        .send(MarketWsCommand::Unsubscribe(to_unsub))
                        .await;
                }
                if !to_sub.is_empty() {
                    tracing::info!(
                        target: "market_discovery",
                        count = to_sub.len(),
                        "market ws subscribe tokens"
                    );
                    let _ = tx_market_ws.send(MarketWsCommand::Subscribe(to_sub)).await;
                }

                subscribed_tokens = desired_tokens;
            }
        }
    }
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
    fn tracked_slugs_include_current_next_and_grace_previous() {
        let now = 1_769_390_101;
        let grace_s = 20 * 60;
        let slugs = discovery_slugs(now, grace_s);
        let tracked = tracked_slugs(&slugs);

        assert!(tracked.contains("btc-updown-15m-1769390100"));
        assert!(tracked.contains("btc-updown-15m-1769391000"));
        assert!(tracked.contains("btc-updown-15m-1769389200"));
    }
}
