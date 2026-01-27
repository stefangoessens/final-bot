use reqwest::StatusCode;
use serde::Deserialize;
use time::format_description::well_known::Rfc3339;

use crate::error::{BotError, BotResult};
use crate::state::market_state::MarketIdentity;

#[derive(Debug, Clone)]
pub struct GammaClient {
    base_url: String,
    http: reqwest::Client,
}

impl GammaClient {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            http: reqwest::Client::new(),
        }
    }

    pub async fn fetch_market_by_slug(&self, slug: &str) -> BotResult<Option<GammaMarket>> {
        let url = format!(
            "{}/markets/slug/{}",
            self.base_url.trim_end_matches('/'),
            slug
        );

        let resp = self.http.get(url).send().await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(BotError::Other(format!(
                "gamma returned status {}",
                resp.status()
            )));
        }

        let market = resp.json::<GammaMarket>().await?;
        Ok(Some(market))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GammaMarket {
    pub slug: String,
    pub condition_id: String,
    pub end_date: String,
    pub clob_token_ids: String,
    pub outcomes: String,

    pub active: bool,
    pub closed: bool,
    pub accepting_orders: bool,
    pub restricted: bool,
}

impl GammaMarket {
    pub fn to_market_identity(&self, interval_start_ts: i64) -> BotResult<MarketIdentity> {
        let end_dt = time::OffsetDateTime::parse(&self.end_date, &Rfc3339).map_err(|e| {
            BotError::Other(format!(
                "failed to parse gamma endDate '{}': {e}",
                self.end_date
            ))
        })?;
        let interval_end_ts = end_dt.unix_timestamp();

        let token_ids: Vec<String> = serde_json::from_str(&self.clob_token_ids).map_err(|e| {
            BotError::Other(format!(
                "failed to parse gamma clobTokenIds '{}': {e}",
                self.clob_token_ids
            ))
        })?;

        let outcomes: Vec<String> = serde_json::from_str(&self.outcomes).map_err(|e| {
            BotError::Other(format!(
                "failed to parse gamma outcomes '{}': {e}",
                self.outcomes
            ))
        })?;

        if token_ids.len() < 2 {
            return Err(BotError::Other(format!(
                "gamma clobTokenIds expected len>=2, got {}",
                token_ids.len()
            )));
        }

        let mut token_up = token_ids[0].clone();
        let mut token_down = token_ids[1].clone();

        if outcomes.len() == token_ids.len() {
            if let Some(idx) = outcomes.iter().position(|o| o.eq_ignore_ascii_case("Up")) {
                token_up = token_ids[idx].clone();
            }
            if let Some(idx) = outcomes.iter().position(|o| o.eq_ignore_ascii_case("Down")) {
                token_down = token_ids[idx].clone();
            }
        }

        Ok(MarketIdentity {
            slug: self.slug.clone(),
            interval_start_ts,
            interval_end_ts,
            condition_id: self.condition_id.clone(),
            token_up,
            token_down,
            active: self.active,
            closed: self.closed,
            accepting_orders: self.accepting_orders,
            restricted: self.restricted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamma_to_identity_parses_token_ids_and_outcomes() {
        let m = GammaMarket {
            slug: "btc-updown-15m-1000".to_string(),
            condition_id: "cond".to_string(),
            end_date: "2026-01-26T01:30:00Z".to_string(),
            clob_token_ids: "[\"t_up\",\"t_down\"]".to_string(),
            outcomes: "[\"Up\",\"Down\"]".to_string(),
            active: true,
            closed: false,
            accepting_orders: true,
            restricted: false,
        };

        let id = m.to_market_identity(1_000).unwrap();
        assert_eq!(id.token_up, "t_up");
        assert_eq!(id.token_down, "t_down");
    }

    #[test]
    fn gamma_to_identity_maps_up_down_by_outcomes() {
        let m = GammaMarket {
            slug: "btc-updown-15m-1000".to_string(),
            condition_id: "cond".to_string(),
            end_date: "2026-01-26T01:30:00Z".to_string(),
            clob_token_ids: "[\"t_down\",\"t_up\"]".to_string(),
            outcomes: "[\"Down\",\"Up\"]".to_string(),
            active: true,
            closed: false,
            accepting_orders: true,
            restricted: false,
        };

        let id = m.to_market_identity(1_000).unwrap();
        assert_eq!(id.token_up, "t_up");
        assert_eq!(id.token_down, "t_down");
    }
}
