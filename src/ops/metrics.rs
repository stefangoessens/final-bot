use std::convert::Infallible;
use std::net::SocketAddr;

use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, StatusCode};
use prometheus::core::Collector;
use prometheus::{
    Counter, Encoder, Gauge, GaugeVec, Histogram, HistogramOpts, IntCounter, IntGauge, Registry,
    TextEncoder,
};

use crate::error::{BotError, BotResult};
use crate::ops::shutdown::Shutdown;

#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    pub ws_market_connected: IntGauge,
    pub ws_user_connected: IntGauge,
    pub rtds_connected: IntGauge,
    pub ws_message_lag_ms: Histogram,
    pub order_submit_latency_ms: Histogram,
    pub order_rejects_total: IntCounter,
    pub fills_total: IntCounter,
    pub unpaired_shares: GaugeVec,
    pub market_exposure_usdc: GaugeVec,
    pub market_exposure_cap_usdc: GaugeVec,
    pub market_exposure_over_cap: GaugeVec,
    pub pnl_realized_usdc: Gauge,
    pub fee_paid_usdc: Counter,
    pub rebate_estimated_usdc: Counter,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let ws_market_connected =
            register(&registry, IntGauge::new("ws_market_connected", "CLOB market ws up").unwrap());
        let ws_user_connected =
            register(&registry, IntGauge::new("ws_user_connected", "CLOB user ws up").unwrap());
        let rtds_connected =
            register(&registry, IntGauge::new("rtds_connected", "RTDS ws up").unwrap());

        let ws_message_lag_ms = register(
            &registry,
            Histogram::with_opts(
                HistogramOpts::new("ws_message_lag_ms", "WS message lag in ms")
                    .buckets(prometheus::exponential_buckets(1.0, 2.0, 12).unwrap()),
            )
            .unwrap(),
        );

        let order_submit_latency_ms = register(
            &registry,
            Histogram::with_opts(
                HistogramOpts::new("order_submit_latency_ms", "Order submit latency in ms")
                    .buckets(prometheus::exponential_buckets(2.0, 2.0, 12).unwrap()),
            )
            .unwrap(),
        );

        let order_rejects_total =
            register(&registry, IntCounter::new("order_rejects_total", "Order rejects").unwrap());
        let fills_total =
            register(&registry, IntCounter::new("fills_total", "Fills total").unwrap());

        let unpaired_shares = register(
            &registry,
            GaugeVec::new(
                prometheus::Opts::new("unpaired_shares", "Unpaired shares per market"),
                &["market_slug"],
            )
            .unwrap(),
        );
        let market_exposure_usdc = register(
            &registry,
            GaugeVec::new(
                prometheus::Opts::new(
                    "market_exposure_usdc",
                    "USDC exposure per market (inventory + open orders)",
                ),
                &["market_slug"],
            )
            .unwrap(),
        );
        let market_exposure_cap_usdc = register(
            &registry,
            GaugeVec::new(
                prometheus::Opts::new("market_exposure_cap_usdc", "USDC exposure cap per market"),
                &["market_slug"],
            )
            .unwrap(),
        );
        let market_exposure_over_cap = register(
            &registry,
            GaugeVec::new(
                prometheus::Opts::new(
                    "market_exposure_over_cap",
                    "1 if market exposure exceeds cap else 0",
                ),
                &["market_slug"],
            )
            .unwrap(),
        );

        let pnl_realized_usdc = register(
            &registry,
            Gauge::new("pnl_realized_usdc", "Realized pnl in USDC").unwrap(),
        );
        let fee_paid_usdc =
            register(&registry, Counter::new("fee_paid_usdc", "Fees paid in USDC").unwrap());
        let rebate_estimated_usdc = register(
            &registry,
            Counter::new("rebate_estimated_usdc", "Estimated rebates in USDC").unwrap(),
        );

        Self {
            registry,
            ws_market_connected,
            ws_user_connected,
            rtds_connected,
            ws_message_lag_ms,
            order_submit_latency_ms,
            order_rejects_total,
            fills_total,
            unpaired_shares,
            market_exposure_usdc,
            market_exposure_cap_usdc,
            market_exposure_over_cap,
            pnl_realized_usdc,
            fee_paid_usdc,
            rebate_estimated_usdc,
        }
    }

    pub fn render(&self) -> Vec<u8> {
        let metric_families = self.registry.gather();
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        if let Err(err) = encoder.encode(&metric_families, &mut buffer) {
            tracing::warn!(
                target: "metrics",
                error = %err,
                "failed to encode prometheus metrics"
            );
        }
        buffer
    }

    #[allow(dead_code)]
    pub fn set_ws_market_connected(&self, connected: bool) {
        self.ws_market_connected.set(i64::from(connected));
    }

    #[allow(dead_code)]
    pub fn set_ws_user_connected(&self, connected: bool) {
        self.ws_user_connected.set(i64::from(connected));
    }

    #[allow(dead_code)]
    pub fn set_rtds_connected(&self, connected: bool) {
        self.rtds_connected.set(i64::from(connected));
    }

    #[allow(dead_code)]
    pub fn observe_ws_message_lag_ms(&self, lag_ms: f64) {
        self.ws_message_lag_ms.observe(lag_ms);
    }

    #[allow(dead_code)]
    pub fn observe_order_submit_latency_ms(&self, latency_ms: f64) {
        self.order_submit_latency_ms.observe(latency_ms);
    }

    #[allow(dead_code)]
    pub fn inc_order_rejects(&self) {
        self.order_rejects_total.inc();
    }

    #[allow(dead_code)]
    pub fn inc_fills(&self) {
        self.fills_total.inc();
    }

    #[allow(dead_code)]
    pub fn set_unpaired_shares(&self, market_slug: &str, shares: f64) {
        self.unpaired_shares
            .with_label_values(&[market_slug])
            .set(shares);
    }

    #[allow(dead_code)]
    pub fn set_market_exposure_usdc(&self, market_slug: &str, exposure: f64) {
        self.market_exposure_usdc
            .with_label_values(&[market_slug])
            .set(exposure);
    }

    #[allow(dead_code)]
    pub fn set_market_exposure_cap_usdc(&self, market_slug: &str, cap: f64) {
        self.market_exposure_cap_usdc
            .with_label_values(&[market_slug])
            .set(cap);
    }

    #[allow(dead_code)]
    pub fn set_market_exposure_over_cap(&self, market_slug: &str, over_cap: bool) {
        self.market_exposure_over_cap
            .with_label_values(&[market_slug])
            .set(if over_cap { 1.0 } else { 0.0 });
    }

    #[allow(dead_code)]
    pub fn set_pnl_realized_usdc(&self, pnl: f64) {
        self.pnl_realized_usdc.set(pnl);
    }

    #[allow(dead_code)]
    pub fn add_fee_paid_usdc(&self, fee: f64) {
        self.fee_paid_usdc.inc_by(fee);
    }

    #[allow(dead_code)]
    pub fn add_rebate_estimated_usdc(&self, rebate: f64) {
        self.rebate_estimated_usdc.inc_by(rebate);
    }
}

pub async fn serve(addr: SocketAddr, metrics: Metrics, shutdown: Shutdown) -> BotResult<()> {
    tracing::info!(target: "metrics", bind = %addr, "metrics server starting");

    let make_svc = make_service_fn(move |_| {
        let metrics = metrics.clone();
        async move { Ok::<_, Infallible>(service_fn(move |req| handle(req, metrics.clone()))) }
    });

    let server = hyper::Server::try_bind(&addr)
        .map_err(|e| BotError::Other(format!("metrics bind failed: {e}")))?;
    let server = server.serve(make_svc);
    let server = server.with_graceful_shutdown(shutdown.wait());

    server
        .await
        .map_err(|e| BotError::Other(format!("metrics server error: {e}")))
}

async fn handle(req: Request<Body>, metrics: Metrics) -> Result<Response<Body>, Infallible> {
    if req.method() != Method::GET {
        return Ok(response_with_status(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed",
        ));
    }

    match req.uri().path() {
        "/metrics" => {
            let body = metrics.render();
            let mut resp = Response::new(Body::from(body));
            resp.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_static("text/plain; version=0.0.4"),
            );
            Ok(resp)
        }
        _ => Ok(response_with_status(StatusCode::NOT_FOUND, "not found")),
    }
}

fn response_with_status(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from(body.to_string())))
}

fn register<M>(registry: &Registry, metric: M) -> M
where
    M: Collector + Clone + 'static,
{
    registry
        .register(Box::new(metric.clone()))
        .expect("metric registration");
    metric
}
