use serde::Deserialize;

use crate::error::{BotError, BotResult};

#[derive(Debug, Clone)]
pub struct ClobPublicClient {
    base_url: String,
    http: reqwest::Client,
}

impl ClobPublicClient {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            http: reqwest::Client::new(),
        }
    }

    pub async fn fetch_tick_sizes(&self, token_ids: &[String]) -> BotResult<Vec<(String, f64)>> {
        if token_ids.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{}/books", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(url)
            .json(&serde_json::json!({ "token_ids": token_ids }))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(BotError::Other(format!(
                "clob /books returned status {}",
                resp.status()
            )));
        }

        let books = resp.json::<Vec<BookResponse>>().await?;
        let mut out = Vec::with_capacity(books.len());
        for book in books {
            if !book.tick_size.is_finite() || book.tick_size <= 0.0 {
                return Err(BotError::Other(format!(
                    "invalid tick_size {} for token {}",
                    book.tick_size, book.asset_id
                )));
            }
            out.push((book.asset_id, book.tick_size));
        }
        Ok(out)
    }
}

#[derive(Debug, Deserialize)]
struct BookResponse {
    #[serde(rename = "asset_id")]
    asset_id: String,
    #[serde(deserialize_with = "de_f64")]
    tick_size: f64,
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
    fn parses_tick_size_from_books_response() {
        let raw = r#"[{"asset_id":"1","tick_size":"0.01","bids":[],"asks":[]}]"#;
        let parsed: Vec<BookResponse> = serde_json::from_str(raw).expect("parse books");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].asset_id, "1");
        assert!((parsed[0].tick_size - 0.01).abs() < 1e-12);
    }
}

