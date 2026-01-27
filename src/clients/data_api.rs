use serde::Deserialize;

use crate::error::{BotError, BotResult};

#[derive(Debug, Clone)]
pub struct DataApiClient {
    base_url: String,
    http: reqwest::Client,
}

impl DataApiClient {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            http: reqwest::Client::new(),
        }
    }

    pub async fn fetch_positions(
        &self,
        user: &str,
        markets: Option<&[String]>,
    ) -> BotResult<Vec<DataApiPosition>> {
        let url = format!("{}/positions", self.base_url.trim_end_matches('/'));
        let mut params: Vec<(String, String)> = vec![
            ("user".to_string(), user.to_string()),
            ("sizeThreshold".to_string(), "0".to_string()),
            ("limit".to_string(), "500".to_string()),
        ];

        if let Some(markets) = markets {
            for market in markets {
                params.push(("market".to_string(), market.clone()));
            }
        }

        let resp = self.http.get(url).query(&params).send().await?;
        if !resp.status().is_success() {
            return Err(BotError::Other(format!(
                "data api positions returned status {}",
                resp.status()
            )));
        }

        let payload = resp.json::<PositionsResponse>().await?;
        Ok(match payload {
            PositionsResponse::List(data) => data,
            PositionsResponse::Wrapped { data, .. } => data,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataApiPosition {
    pub condition_id: String,
    pub asset: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(deserialize_with = "de_f64")]
    pub size: f64,
    #[serde(default, deserialize_with = "de_f64")]
    pub avg_price: f64,
    #[serde(default)]
    pub mergeable: bool,
    #[serde(default)]
    pub redeemable: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PositionsResponse {
    List(Vec<DataApiPosition>),
    Wrapped { data: Vec<DataApiPosition> },
}

fn de_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(num) => num
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("invalid f64")),
        serde_json::Value::String(s) => s
            .parse::<f64>()
            .map_err(|e| serde::de::Error::custom(format!("invalid f64: {e}"))),
        serde_json::Value::Null => Ok(0.0),
        other => Err(serde::de::Error::custom(format!(
            "unexpected value for f64: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_response_accepts_list() {
        let raw = r#"[{"conditionId":"cond","asset":"1","outcome":"Up","size":1,"avgPrice":0.4,"mergeable":true,"redeemable":false,"slug":"m"}]"#;
        let parsed: PositionsResponse = serde_json::from_str(raw).expect("parse");
        match parsed {
            PositionsResponse::List(data) => {
                assert_eq!(data.len(), 1);
                assert_eq!(data[0].condition_id, "cond");
                assert_eq!(data[0].avg_price, 0.4);
            }
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn positions_response_accepts_wrapped() {
        let raw = r#"{"data":[{"conditionId":"cond","asset":"1","outcome":"Up","size":"2.5","avgPrice":"0.45","mergeable":false,"redeemable":true,"slug":"m"}],"count":1}"#;
        let parsed: PositionsResponse = serde_json::from_str(raw).expect("parse");
        match parsed {
            PositionsResponse::Wrapped { data, .. } => {
                assert_eq!(data.len(), 1);
                assert_eq!(data[0].size, 2.5);
                assert_eq!(data[0].avg_price, 0.45);
            }
            _ => panic!("expected wrapped"),
        }
    }
}
