use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, StatusCode};
use serde::Serialize;

use crate::config::AppConfig;
use crate::error::{BotError, BotResult};
use crate::ops::shutdown::Shutdown;

const DEFAULT_MARKET_WS_STALE_MS: i64 = 5_000;
const DEFAULT_USER_WS_STALE_MS: i64 = 5_000;

#[derive(Clone)]
pub struct HealthState {
    inner: Arc<HealthInner>,
}

struct HealthInner {
    market_ws_stale_ms: i64,
    user_ws_stale_ms: i64,
    chainlink_stale_ms: i64,
    binance_stale_ms: i64,
    require_user_ws: bool,
    last_market_ws_ms: AtomicI64,
    last_user_ws_ms: AtomicI64,
    last_chainlink_ms: AtomicI64,
    last_binance_ms: AtomicI64,
    tracked_markets: AtomicUsize,
    started_ms: i64,
    halt_reason: RwLock<Option<String>>,
}

impl HealthState {
    pub fn new(cfg: &AppConfig) -> Self {
        let now_ms = now_ms();
        Self {
            inner: Arc::new(HealthInner {
                market_ws_stale_ms: DEFAULT_MARKET_WS_STALE_MS,
                user_ws_stale_ms: DEFAULT_USER_WS_STALE_MS,
                chainlink_stale_ms: cfg.oracle.chainlink_stale_ms,
                binance_stale_ms: cfg.oracle.binance_stale_ms,
                require_user_ws: !cfg.trading.dry_run,
                last_market_ws_ms: AtomicI64::new(now_ms),
                last_user_ws_ms: AtomicI64::new(now_ms),
                last_chainlink_ms: AtomicI64::new(now_ms),
                last_binance_ms: AtomicI64::new(now_ms),
                tracked_markets: AtomicUsize::new(0),
                started_ms: now_ms,
                halt_reason: RwLock::new(None),
            }),
        }
    }

    pub fn report(&self, now_ms: i64) -> HealthReport {
        let market_ws = feed_report(
            self.inner.last_market_ws_ms.load(Ordering::Relaxed),
            self.inner.market_ws_stale_ms,
            now_ms,
        );
        let user_ws = feed_report(
            self.inner.last_user_ws_ms.load(Ordering::Relaxed),
            self.inner.user_ws_stale_ms,
            now_ms,
        );
        let chainlink = feed_report(
            self.inner.last_chainlink_ms.load(Ordering::Relaxed),
            self.inner.chainlink_stale_ms,
            now_ms,
        );
        let binance = feed_report(
            self.inner.last_binance_ms.load(Ordering::Relaxed),
            self.inner.binance_stale_ms,
            now_ms,
        );

        let halted_reason = self.inner.halt_reason.read().ok().and_then(|r| r.clone());
        let required_healthy = market_ws.is_healthy()
            && chainlink.is_healthy()
            && (!self.inner.require_user_ws || user_ws.is_healthy());

        let status = if halted_reason.is_some() {
            "halted"
        } else if required_healthy && binance.is_healthy() {
            "ok"
        } else {
            "degraded"
        };

        let ok = halted_reason.is_none() && required_healthy;

        HealthReport {
            ok,
            status,
            now_ms,
            uptime_ms: now_ms.saturating_sub(self.inner.started_ms),
            tracked_markets: self.inner.tracked_markets.load(Ordering::Relaxed),
            halted_reason,
            feeds: FeedReportSet {
                market_ws,
                user_ws,
                rtds_chainlink: chainlink,
                rtds_binance: binance,
            },
        }
    }

    #[allow(dead_code)]
    pub fn mark_market_ws(&self, ts_ms: i64) {
        self.inner.last_market_ws_ms.store(ts_ms, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn mark_user_ws(&self, ts_ms: i64) {
        self.inner.last_user_ws_ms.store(ts_ms, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn mark_chainlink(&self, ts_ms: i64) {
        self.inner.last_chainlink_ms.store(ts_ms, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn mark_binance(&self, ts_ms: i64) {
        self.inner.last_binance_ms.store(ts_ms, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn set_tracked_markets(&self, count: usize) {
        self.inner.tracked_markets.store(count, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn set_halt_reason(&self, reason: Option<String>) {
        if let Ok(mut guard) = self.inner.halt_reason.write() {
            *guard = reason;
        }
    }
}

#[derive(Debug, Serialize)]
pub struct HealthReport {
    pub ok: bool,
    pub status: &'static str,
    pub now_ms: i64,
    pub uptime_ms: i64,
    pub tracked_markets: usize,
    pub halted_reason: Option<String>,
    pub feeds: FeedReportSet,
}

#[derive(Debug, Serialize)]
pub struct FeedReportSet {
    pub market_ws: FeedReport,
    pub user_ws: FeedReport,
    pub rtds_chainlink: FeedReport,
    pub rtds_binance: FeedReport,
}

#[derive(Debug, Serialize)]
pub struct FeedReport {
    pub status: &'static str,
    pub last_update_ms: i64,
    pub stale_after_ms: i64,
    pub age_ms: Option<i64>,
}

impl FeedReport {
    fn is_healthy(&self) -> bool {
        self.status == "healthy"
    }
}

pub async fn serve(addr: SocketAddr, health: HealthState, shutdown: Shutdown) -> BotResult<()> {
    tracing::info!(target: "health", bind = %addr, "health server starting");

    let make_svc = make_service_fn(move |_| {
        let health = health.clone();
        async move { Ok::<_, Infallible>(service_fn(move |req| handle(req, health.clone()))) }
    });

    let server = hyper::Server::try_bind(&addr)
        .map_err(|e| BotError::Other(format!("health bind failed: {e}")))?;
    let server = server.serve(make_svc);
    let server = server.with_graceful_shutdown(shutdown.wait());

    server
        .await
        .map_err(|e| BotError::Other(format!("health server error: {e}")))
}

async fn handle(req: Request<Body>, health: HealthState) -> Result<Response<Body>, Infallible> {
    if req.method() != Method::GET {
        return Ok(response_with_status(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed",
            None,
        ));
    }

    match req.uri().path() {
        "/healthz" => {
            let report = health.report(now_ms());
            let body = serde_json::to_vec(&report).unwrap_or_else(|_| b"{}".to_vec());
            let mut resp = Response::new(Body::from(body));
            resp.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_static("application/json"),
            );
            Ok(resp)
        }
        _ => Ok(response_with_status(StatusCode::NOT_FOUND, "not found", None)),
    }
}

fn feed_report(last_update_ms: i64, stale_after_ms: i64, now_ms: i64) -> FeedReport {
    let age_ms = if last_update_ms > 0 {
        Some(now_ms.saturating_sub(last_update_ms))
    } else {
        None
    };

    let status = match age_ms {
        None => "unknown",
        Some(age) if age > stale_after_ms => "stale",
        Some(_) => "healthy",
    };

    FeedReport {
        status,
        last_update_ms,
        stale_after_ms,
        age_ms,
    }
}

fn response_with_status(status: StatusCode, body: &str, content_type: Option<&str>) -> Response<Body> {
    let mut resp = Response::builder()
        .status(status)
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from(body.to_string())));

    if let Some(ct) = content_type {
        if let Ok(value) = hyper::header::HeaderValue::from_str(ct) {
            resp.headers_mut()
                .insert(hyper::header::CONTENT_TYPE, value);
        }
    }
    resp
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
