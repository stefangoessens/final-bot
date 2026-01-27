#![allow(dead_code)]

use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;
use std::{collections::HashMap, future::Future};

use alloy_signer_local::PrivateKeySigner;
use futures_util::future::BoxFuture;
use polymarket_client_sdk::auth::{Credentials, Normal, Signer, Uuid};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::clob::{Client as SdkClient, Config as SdkConfig};
use polymarket_client_sdk::clob::types::{Amount, OrderType as SdkOrderType, Side, SignatureType, SignedOrder};
use polymarket_client_sdk::types::{Address, Decimal, U256};
use polymarket_client_sdk::POLYGON;

use crate::config::{AppConfig, CompletionOrderType, HeartbeatsConfig, WalletMode};
use crate::error::{BotError, BotResult};
use crate::strategy::DesiredOrder;

pub type OrderId = String;
pub type OrderRequest = SignedOrder;

const DEFAULT_FEE_RATE_TTL_MS: i64 = 60_000;

#[derive(Debug, Clone, Default)]
pub struct CancelResult {
    pub canceled: Vec<OrderId>,
    pub failed: Vec<OrderId>,
}

type SdkAuthedClient = SdkClient<Authenticated<Normal>>;

#[derive(Clone)]
pub struct ClobRestClient {
    inner: Option<Arc<SdkHandle>>,
}

#[derive(Clone)]
struct SdkHandle {
    client: SdkAuthedClient,
    signer: PrivateKeySigner,
    fee_rate_cache: FeeRateCache,
}

#[derive(Clone)]
struct FeeRateCache {
    ttl_ms: i64,
    inner: Arc<tokio::sync::Mutex<HashMap<String, FeeRateCacheEntry>>>,
}

#[derive(Clone, Copy)]
struct FeeRateCacheEntry {
    fee_rate_bps: u64,
    fetched_at_ms: i64,
}

impl FeeRateCache {
    fn new(ttl_ms: i64) -> Self {
        Self {
            ttl_ms,
            inner: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    async fn get_cached(&self, token_key: &str, now_ms: i64) -> Option<u64> {
        let mut guard = self.inner.lock().await;
        if let Some(entry) = guard.get(token_key) {
            if now_ms.saturating_sub(entry.fetched_at_ms) <= self.ttl_ms {
                return Some(entry.fee_rate_bps);
            }
        }
        guard.remove(token_key);
        None
    }

    async fn set(&self, token_key: &str, fee_rate_bps: u64, now_ms: i64) {
        let mut guard = self.inner.lock().await;
        guard.insert(
            token_key.to_string(),
            FeeRateCacheEntry {
                fee_rate_bps,
                fetched_at_ms: now_ms,
            },
        );
    }

    async fn get_or_fetch<F, Fut>(&self, token_key: &str, now_ms: i64, fetcher: F) -> BotResult<u64>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = BotResult<u64>>,
    {
        if let Some(cached) = self.get_cached(token_key, now_ms).await {
            return Ok(cached);
        }

        let fresh = fetcher().await?;
        self.set(token_key, fresh, now_ms).await;
        Ok(fresh)
    }
}

impl std::fmt::Debug for ClobRestClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClobRestClient")
            .field("authenticated", &self.inner.is_some())
            .finish()
    }
}

impl ClobRestClient {
    pub fn new(_base_url: String) -> Self {
        Self { inner: None }
    }

    pub async fn from_config(cfg: &AppConfig) -> BotResult<Self> {
        if cfg.trading.dry_run {
            return Ok(Self { inner: None });
        }

        let private_key = cfg
            .keys
            .private_key
            .as_deref()
            .unwrap_or_default()
            .trim();
        if private_key.is_empty() {
            return Err(BotError::Config(
                "missing required key: PMMB_KEYS__PRIVATE_KEY".to_string(),
            ));
        }

        let pk = private_key.strip_prefix("0x").unwrap_or(private_key);
        let signer: PrivateKeySigner = pk
            .parse::<PrivateKeySigner>()
            .map_err(|e| BotError::Other(format!("invalid private key: {e}")))?
            .with_chain_id(Some(POLYGON));

        let api_key = cfg
            .keys
            .clob_api_key
            .as_deref()
            .unwrap_or_default()
            .trim();
        let api_secret = cfg
            .keys
            .clob_api_secret
            .as_deref()
            .unwrap_or_default()
            .to_string();
        let api_passphrase = cfg
            .keys
            .clob_api_passphrase
            .as_deref()
            .unwrap_or_default()
            .to_string();
        if api_key.is_empty() {
            return Err(BotError::Config(
                "missing required key: PMMB_KEYS__CLOB_API_KEY".to_string(),
            ));
        }

        let api_key_uuid = Uuid::from_str(api_key)
            .map_err(|e| BotError::Config(format!("invalid CLOB API key uuid: {e}")))?;
        let creds = Credentials::new(api_key_uuid, api_secret, api_passphrase);

        let mut config = SdkConfig::default();
        if cfg.heartbeats.enabled {
            config = SdkConfig::builder()
                .heartbeat_interval(Duration::from_millis(cfg.heartbeats.interval_ms as u64))
                .build();
        }

        let client = SdkClient::new(&cfg.endpoints.clob_rest_base_url, config)
            .map_err(|e| BotError::Other(format!("sdk client init failed: {e}")))?;

        let signature_type = map_wallet_mode(cfg.keys.wallet_mode);
        let mut auth = client
            .authentication_builder(&signer)
            .credentials(creds)
            .signature_type(signature_type);

        if let Some(funder) = cfg.keys.funder_address.as_deref().map(str::trim) {
            if !funder.is_empty() {
                let addr = Address::from_str(funder)
                    .map_err(|e| BotError::Config(format!("invalid funder_address: {e}")))?;
                auth = auth.funder(addr);
            }
        }

        let client = auth
            .authenticate()
            .await
            .map_err(|e| BotError::Other(format!("sdk authenticate failed: {e}")))?;

        Ok(Self {
            inner: Some(Arc::new(SdkHandle {
                client,
                signer,
                fee_rate_cache: FeeRateCache::new(DEFAULT_FEE_RATE_TTL_MS),
            })),
        })
    }

    pub fn authenticated(&self) -> bool {
        self.inner.is_some()
    }

    pub async fn are_orders_scoring(
        &self,
        order_ids: &[String],
    ) -> BotResult<std::collections::HashMap<String, bool>> {
        if order_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let inner = self.inner.as_ref().ok_or_else(|| {
            BotError::Other("cannot check scoring: client not authenticated".to_string())
        })?;

        let ids: Vec<&str> = order_ids.iter().map(String::as_str).collect();
        let scoring = inner
            .client
            .are_orders_scoring(&ids)
            .await
            .map_err(|e| BotError::Other(format!("orders-scoring request failed: {e}")))?;
        Ok(scoring)
    }

    pub fn heartbeat_api(&self) -> BotResult<Arc<dyn HeartbeatApi>> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            BotError::Other("heartbeat api unavailable: client not authenticated".to_string())
        })?;
        Ok(Arc::new(SdkHeartbeatApi {
            client: inner.client.clone(),
        }))
    }

    pub async fn build_signed_orders(
        &self,
        desired: Vec<DesiredOrder>,
        _now_ms: i64,
    ) -> BotResult<Vec<OrderRequest>> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            BotError::Other("cannot sign orders: client not authenticated".to_string())
        })?;

        let mut out = Vec::with_capacity(desired.len());
        for order in desired {
            let token_id = parse_token_id(&order.token_id)?;
            let fee_rate_bps = self.get_fee_rate_bps_for_id(token_id).await?;
            inner
                .client
                .set_fee_rate_bps(token_id, fee_rate_bps as u32);
            let price = parse_decimal(order.price, 8)?;
            let size = parse_decimal(order.size, 2)?;

            let signable = inner
                .client
                .limit_order()
                .token_id(token_id)
                .price(price)
                .size(size)
                .side(Side::Buy)
                .order_type(SdkOrderType::GTC)
                .post_only(order.post_only)
                .build()
                .await
                .map_err(|e| BotError::Other(format!("build limit order failed: {e}")))?;

            let signed = inner
                .client
                .sign(&inner.signer, signable)
                .await
                .map_err(|e| BotError::Other(format!("sign order failed: {e}")))?;

            out.push(signed);
        }

        Ok(out)
    }

    pub async fn build_completion_order(
        &self,
        token_id: &str,
        shares: f64,
        p_max: f64,
        order_type: CompletionOrderType,
        _now_ms: i64,
    ) -> BotResult<OrderRequest> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            BotError::Other("cannot sign completion: client not authenticated".to_string())
        })?;

        let token_id = parse_token_id(token_id)?;
        let shares = parse_decimal(shares, 2)?;
        let p_max = parse_decimal(p_max, 8)?;

        let signable = inner
            .client
            .market_order()
            .token_id(token_id)
            .amount(Amount::shares(shares).map_err(|e| BotError::Other(format!("invalid shares: {e}")))?)
            .side(Side::Buy)
            .order_type(map_completion_order_type(order_type))
            .price(p_max)
            .build()
            .await
            .map_err(|e| BotError::Other(format!("build completion order failed: {e}")))?;

        inner
            .client
            .sign(&inner.signer, signable)
            .await
            .map_err(|e| BotError::Other(format!("sign completion failed: {e}")))
    }

    pub async fn get_fee_rate_bps(&self, token_id: &str) -> BotResult<u64> {
        let token_id = parse_token_id(token_id)?;
        self.get_fee_rate_bps_for_id(token_id).await
    }

    async fn get_fee_rate_bps_for_id(&self, token_id: U256) -> BotResult<u64> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            BotError::Other("fee_rate unavailable: client not authenticated".to_string())
        })?;
        let now = now_ms();
        let cache_key = token_id.to_string();
        inner
            .fee_rate_cache
            .get_or_fetch(&cache_key, now, || async {
                let resp = inner
                    .client
                    .fee_rate_bps(token_id)
                    .await
                    .map_err(|e| BotError::Other(format!("fee_rate fetch failed: {e}")))?;
                Ok(resp.base_fee as u64)
            })
            .await
    }

    pub async fn post_orders(&self, orders: Vec<OrderRequest>) -> BotResult<Vec<OrderId>> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            BotError::Other("post_orders unavailable: client not authenticated".to_string())
        })?;
        let responses = inner
            .client
            .post_orders(orders)
            .await
            .map_err(|e| BotError::Other(format!("post_orders failed: {e}")))?;

        let mut order_ids = Vec::with_capacity(responses.len());
        let mut failures = Vec::new();
        for resp in responses {
            if !resp.success {
                failures.push(resp.error_msg.unwrap_or_else(|| "unknown error".into()));
                continue;
            }
            if let Some(err) = resp.error_msg {
                failures.push(err);
                continue;
            }
            order_ids.push(resp.order_id);
        }

        if !failures.is_empty() {
            return Err(BotError::Other(format!(
                "clob post_orders failed: {}",
                failures.join("; ")
            )));
        }

        Ok(order_ids)
    }

    pub async fn cancel_orders(&self, order_ids: Vec<OrderId>) -> BotResult<CancelResult> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            BotError::Other("cancel_orders unavailable: client not authenticated".to_string())
        })?;
        if order_ids.is_empty() {
            return Ok(CancelResult::default());
        }
        let ids: Vec<&str> = order_ids.iter().map(|id| id.as_str()).collect();
        let resp = inner
            .client
            .cancel_orders(&ids)
            .await
            .map_err(|e| BotError::Other(format!("cancel_orders failed: {e}")))?;

        Ok(CancelResult {
            canceled: resp.canceled,
            failed: resp.not_canceled.into_keys().collect(),
        })
    }

    pub async fn cancel_all_orders(&self) -> BotResult<CancelResult> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            BotError::Other("cancel_all unavailable: client not authenticated".to_string())
        })?;
        let resp = inner
            .client
            .cancel_all_orders()
            .await
            .map_err(|e| BotError::Other(format!("cancel_all failed: {e}")))?;
        Ok(CancelResult {
            canceled: resp.canceled,
            failed: resp.not_canceled.into_keys().collect(),
        })
    }
}

pub trait HeartbeatApi: Send + Sync {
    fn post_heartbeat(
        &self,
        heartbeat_id: Option<Uuid>,
    ) -> BoxFuture<'static, BotResult<Option<Uuid>>>;
    fn cancel_all_orders(&self) -> BoxFuture<'static, BotResult<()>>;
}

#[derive(Clone)]
struct SdkHeartbeatApi {
    client: SdkAuthedClient,
}

impl HeartbeatApi for SdkHeartbeatApi {
    fn post_heartbeat(
        &self,
        heartbeat_id: Option<Uuid>,
    ) -> BoxFuture<'static, BotResult<Option<Uuid>>> {
        let client = self.client.clone();
        Box::pin(async move {
            let resp = client
                .post_heartbeat(heartbeat_id)
                .await
                .map_err(|e| BotError::Other(format!("post_heartbeat failed: {e}")))?;
            if let Some(err) = resp.error {
                return Err(BotError::Other(format!("heartbeat error: {err}")));
            }
            Ok(Some(resp.heartbeat_id))
        })
    }

    fn cancel_all_orders(&self) -> BoxFuture<'static, BotResult<()>> {
        let client = self.client.clone();
        Box::pin(async move {
            let _ = client
                .cancel_all_orders()
                .await
                .map_err(|e| BotError::Other(format!("cancel_all_orders failed: {e}")))?;
            Ok(())
        })
    }
}

pub async fn run_heartbeat_supervisor(
    api: Arc<dyn HeartbeatApi>,
    cfg: HeartbeatsConfig,
) -> BotResult<()> {
    if !cfg.enabled {
        return Ok(());
    }

    let mut heartbeat_id: Option<Uuid> = None;
    let mut last_ok_ms = now_ms();
    let mut ticker = tokio::time::interval(Duration::from_millis(cfg.interval_ms as u64));

    loop {
        ticker.tick().await;
        let now = now_ms();
        match api.post_heartbeat(heartbeat_id).await {
            Ok(next) => {
                heartbeat_id = next;
                last_ok_ms = now;
            }
            Err(err) => {
                tracing::warn!(target: "heartbeats", error = %err, "heartbeat failed");
            }
        }

        if now.saturating_sub(last_ok_ms) > cfg.grace_ms {
            tracing::error!(
                target: "heartbeats",
                grace_ms = cfg.grace_ms,
                last_ok_ms,
                now_ms = now,
                "heartbeat grace exceeded; canceling all orders"
            );
            let _ = api.cancel_all_orders().await;
            return Err(BotError::Other("heartbeat grace exceeded".to_string()));
        }
    }
}

fn map_wallet_mode(mode: WalletMode) -> SignatureType {
    match mode {
        WalletMode::Eoa => SignatureType::Eoa,
        WalletMode::ProxySafe => SignatureType::GnosisSafe,
    }
}

fn map_completion_order_type(order_type: CompletionOrderType) -> SdkOrderType {
    match order_type {
        CompletionOrderType::Fak => SdkOrderType::FAK,
        CompletionOrderType::Fok => SdkOrderType::FOK,
    }
}

fn parse_token_id(token_id: &str) -> BotResult<U256> {
    let token_id = token_id.trim();
    if token_id.is_empty() {
        return Err(BotError::Other("token_id empty".to_string()));
    }
    if let Some(hex) = token_id.strip_prefix("0x") {
        U256::from_str_radix(hex, 16)
            .map_err(|e| BotError::Other(format!("invalid token_id hex: {e}")))
    } else {
        token_id
            .parse::<U256>()
            .map_err(|e| BotError::Other(format!("invalid token_id: {e}")))
    }
}

fn parse_decimal(value: f64, max_dp: usize) -> BotResult<Decimal> {
    if !value.is_finite() || value < 0.0 {
        return Err(BotError::Other(format!("invalid decimal {value}")));
    }
    let mut s = format!("{value:.max_dp$}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    if s.is_empty() {
        s.push('0');
    }
    s.parse::<Decimal>()
        .map_err(|e| BotError::Other(format!("invalid decimal string {s}: {e}")))
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn completion_order_type_mapping() {
        assert_eq!(map_completion_order_type(CompletionOrderType::Fak), SdkOrderType::FAK);
        assert_eq!(map_completion_order_type(CompletionOrderType::Fok), SdkOrderType::FOK);
    }

    #[tokio::test]
    async fn heartbeat_grace_exceeded_triggers_cancel() {
        #[derive(Clone)]
        struct MockApi {
            cancels: Arc<AtomicUsize>,
        }

        impl HeartbeatApi for MockApi {
            fn post_heartbeat(
                &self,
                _heartbeat_id: Option<Uuid>,
            ) -> BoxFuture<'static, BotResult<Option<Uuid>>> {
                Box::pin(async { Err(BotError::Other("nope".to_string())) })
            }

            fn cancel_all_orders(&self) -> BoxFuture<'static, BotResult<()>> {
                let cancels = self.cancels.clone();
                Box::pin(async move {
                    cancels.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }
        }

        let api = Arc::new(MockApi {
            cancels: Arc::new(AtomicUsize::new(0)),
        });
        let cfg = HeartbeatsConfig {
            enabled: true,
            interval_ms: 10,
            grace_ms: 25,
        };

        let res = tokio::time::timeout(Duration::from_millis(500), run_heartbeat_supervisor(api.clone(), cfg))
            .await
            .expect("timeout");
        assert!(res.is_err(), "expected grace exceeded");
        assert_eq!(api.cancels.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fee_rate_cache_hits_within_ttl() {
        let cache = FeeRateCache::new(100);
        let calls = Arc::new(AtomicUsize::new(0));

        let fetch = |calls: Arc<AtomicUsize>| async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(42u64)
        };

        let first = cache
            .get_or_fetch("token", 0, || fetch(calls.clone()))
            .await
            .expect("fetch failed");
        let second = cache
            .get_or_fetch("token", 50, || fetch(calls.clone()))
            .await
            .expect("fetch failed");

        assert_eq!(first, 42);
        assert_eq!(second, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
