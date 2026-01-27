use std::time::Duration;

use serde::Deserialize;
use tokio::sync::mpsc::Sender;

use crate::state::state_manager::{AppEvent, GeoblockStatus};

#[derive(Debug, Clone)]
pub struct GeoblockLoop {
    url: String,
    poll_interval_ms: u64,
    http: reqwest::Client,
}

impl GeoblockLoop {
    pub fn new(url: String, poll_interval_ms: u64) -> Self {
        Self {
            url,
            poll_interval_ms: poll_interval_ms.max(1),
            http: reqwest::Client::new(),
        }
    }

    pub async fn run(self, tx_events: Sender<AppEvent>) {
        let mut tick = tokio::time::interval(Duration::from_millis(self.poll_interval_ms));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut last_blocked: Option<bool> = None;
        loop {
            tick.tick().await;
            if tx_events.is_closed() {
                return;
            }

            let ts_ms = now_ms();
            let status = match self.fetch_once().await {
                Ok(resp) => GeoblockStatus {
                    blocked: Some(resp.blocked),
                    ip: resp.ip,
                    country: resp.country,
                    region: resp.region,
                    ts_ms,
                    error: None,
                },
                Err(err) => {
                    tracing::warn!(
                        target: "geoblock",
                        error = %err,
                        "geoblock check failed"
                    );
                    GeoblockStatus {
                        blocked: None,
                        ip: None,
                        country: None,
                        region: None,
                        ts_ms,
                        error: Some(err.to_string()),
                    }
                }
            };

            if let Some(blocked) = status.blocked {
                if last_blocked != Some(blocked) {
                    tracing::info!(
                        target: "geoblock",
                        blocked,
                        country = status.country.as_deref().unwrap_or(""),
                        region = status.region.as_deref().unwrap_or(""),
                        ip = status.ip.as_deref().unwrap_or(""),
                        "geoblock status update"
                    );
                }
                last_blocked = Some(blocked);
            }

            if tx_events
                .send(AppEvent::GeoblockStatus(status))
                .await
                .is_err()
            {
                return;
            }
        }
    }

    async fn fetch_once(&self) -> Result<GeoblockResponse, reqwest::Error> {
        self.http
            .get(&self.url)
            .send()
            .await?
            .error_for_status()?
            .json::<GeoblockResponse>()
            .await
    }
}

#[derive(Debug, Deserialize)]
struct GeoblockResponse {
    blocked: bool,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    region: Option<String>,
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
