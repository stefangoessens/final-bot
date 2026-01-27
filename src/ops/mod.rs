pub mod health;
pub mod logging;
pub mod metrics;
pub mod shutdown;

use std::net::SocketAddr;

use crate::config::AppConfig;

#[derive(Clone)]
pub struct OpsState {
    pub metrics: metrics::Metrics,
    pub health: health::HealthState,
}

impl OpsState {
    pub fn new(cfg: &AppConfig) -> Self {
        Self {
            metrics: metrics::Metrics::new(),
            health: health::HealthState::new(cfg),
        }
    }
}

pub fn start_http_servers(
    cfg: &AppConfig,
    ops: OpsState,
) -> (shutdown::ShutdownTrigger, shutdown::Shutdown) {
    let (trigger, shutdown) = shutdown::channel();
    let signal_trigger = trigger.clone();
    tokio::spawn(shutdown::listen_for_shutdown(signal_trigger));

    let metrics_addr = SocketAddr::from(([0, 0, 0, 0], cfg.infra.metrics_port));
    let health_addr = SocketAddr::from(([0, 0, 0, 0], cfg.infra.health_port));

    let metrics_state = ops.metrics.clone();
    let metrics_shutdown = shutdown.clone();
    tokio::spawn(async move {
        if let Err(err) = metrics::serve(metrics_addr, metrics_state, metrics_shutdown).await {
            tracing::error!(
                target: "metrics",
                error = %err,
                "metrics server stopped"
            );
        }
    });

    let health_state = ops.health.clone();
    let health_shutdown = shutdown.clone();
    tokio::spawn(async move {
        if let Err(err) = health::serve(health_addr, health_state, health_shutdown).await {
            tracing::error!(
                target: "health",
                error = %err,
                "health server stopped"
            );
        }
    });

    (trigger, shutdown)
}
