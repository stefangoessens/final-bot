use std::collections::BTreeSet;

use tokio::sync::mpsc::Sender;

use crate::clients::clob_ws_market::MarketWsCommand;
use crate::clients::gamma::GammaClient;
use crate::error::BotResult;
use crate::state::state_manager::AppEvent;
use crate::time::{interval_start_s, now_s, BTC_15M_INTERVAL_S};

#[derive(Debug, Clone)]
pub struct MarketDiscoveryLoop {
    gamma: GammaClient,
    tx_events: Sender<AppEvent>,
    tx_market_ws: Option<Sender<MarketWsCommand>>,
    poll_interval_ms: u64,
}

impl MarketDiscoveryLoop {
    pub fn new(
        gamma: GammaClient,
        tx_events: Sender<AppEvent>,
        tx_market_ws: Option<Sender<MarketWsCommand>>,
        poll_interval_ms: u64,
    ) -> Self {
        Self {
            gamma,
            tx_events,
            tx_market_ws,
            poll_interval_ms,
        }
    }

    pub async fn run(self) -> BotResult<()> {
        let mut tick =
            tokio::time::interval(tokio::time::Duration::from_millis(self.poll_interval_ms));

        let mut last_tracked: BTreeSet<String> = BTreeSet::new();
        let mut subscribed_tokens: BTreeSet<String> = BTreeSet::new();

        loop {
            tick.tick().await;

            let now = now_s();
            let cur_start_s = interval_start_s(now);
            let next_start_s = cur_start_s + BTC_15M_INTERVAL_S;
            let cur_slug = format!("btc-updown-15m-{cur_start_s}");
            let next_slug = format!("btc-updown-15m-{next_start_s}");

            let tracked: BTreeSet<String> =
                [cur_slug.clone(), next_slug.clone()].into_iter().collect();
            if tracked != last_tracked {
                last_tracked = tracked.clone();
                let slugs: Vec<String> = tracked.iter().cloned().collect();
                let _ = self
                    .tx_events
                    .send(AppEvent::SetTrackedMarkets { slugs })
                    .await;
                tracing::info!(
                    target: "market_discovery",
                    current_slug = %format!("btc-updown-15m-{cur_start_s}"),
                    next_slug = %format!("btc-updown-15m-{next_start_s}"),
                    "tracking current+next BTC 15m markets"
                );
            }

            let mut desired_tokens: BTreeSet<String> = BTreeSet::new();

            for (slug, interval_start) in [(cur_slug, cur_start_s), (next_slug, next_start_s)] {
                match self.gamma.fetch_market_by_slug(&slug).await? {
                    Some(market) => {
                        let identity = market.to_market_identity(interval_start)?;
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
                        desired_tokens.insert(identity.token_up.clone());
                        desired_tokens.insert(identity.token_down.clone());
                        let _ = self
                            .tx_events
                            .send(AppEvent::MarketDiscovered(identity))
                            .await;
                    }
                    None => {
                        tracing::debug!(target: "market_discovery", %slug, "market not found yet");
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
