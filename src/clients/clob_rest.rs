#![allow(dead_code)]

use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;
use std::{collections::HashMap, future::Future};

use alloy_signer_local::PrivateKeySigner;
use futures_util::future::BoxFuture;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Credentials, ExposeSecret as _, Normal, Signer, Uuid};
use polymarket_client_sdk::clob::types::request::OrdersRequest;
use polymarket_client_sdk::clob::types::response::{
    FeeRateResponse, OpenOrderResponse, Page, PostOrderResponse,
};
use polymarket_client_sdk::clob::types::{
    OrderStatusType, OrderType as SdkOrderType, Side, SignatureType, SignedOrder,
};
use polymarket_client_sdk::clob::{Client as SdkClient, Config as SdkConfig};
use polymarket_client_sdk::types::{Address, Decimal, B256, U256};
use polymarket_client_sdk::{derive_proxy_wallet, POLYGON};

use crate::config::{ApiCredsSource, AppConfig, CompletionOrderType, HeartbeatsConfig, WalletMode};
use crate::error::{BotError, BotResult};
use crate::strategy::DesiredOrder;

pub type OrderId = String;
pub type OrderRequest = SignedOrder;

const DEFAULT_FEE_RATE_TTL_MS: i64 = 60_000;

#[derive(Debug, Clone, Default)]
pub struct CancelResult {
    pub canceled: Vec<OrderId>,
    pub failed: HashMap<OrderId, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostOrdersResult {
    pub accepted: Vec<PostOrderAccepted>,
    pub rejected: Vec<PostOrderRejected>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostOrderAccepted {
    pub idx: usize,
    pub order_id: OrderId,
    pub token_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostOrderRejected {
    pub idx: usize,
    pub error: String,
    pub order_id: Option<OrderId>,
    pub token_id: Option<String>,
}

type SdkAuthedClient = SdkClient<Authenticated<Normal>>;

#[derive(Clone)]
pub struct ClobRestClient {
    inner: Option<Arc<SdkHandle>>,
}

#[derive(Clone)]
pub struct ClobApiCreds {
    pub api_key: String,
    pub api_secret: String,
    pub api_passphrase: String,
}

struct SdkHandle {
    client: SdkAuthedClient,
    signer: PrivateKeySigner,
    api_creds: ClobApiCreds,
    fee_rate_cache: FeeRateCache,
    fee_rate_http: reqwest::Client,
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

        let private_key = cfg.keys.private_key.as_deref().unwrap_or_default().trim();
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

        let mut config = SdkConfig::default();
        if cfg.heartbeats.enabled {
            config = SdkConfig::builder()
                .heartbeat_interval(Duration::from_millis(cfg.heartbeats.interval_ms as u64))
                .build();
        }

        let client = SdkClient::new(&cfg.endpoints.clob_rest_base_url, config)
            .map_err(|e| BotError::Other(format!("sdk client init failed: {e}")))?;

        let signature_type = map_wallet_mode(cfg.keys.wallet_mode);
        let creds = match cfg.keys.api_creds_source {
            ApiCredsSource::Explicit => {
                let api_key = cfg.keys.clob_api_key.as_deref().unwrap_or_default().trim();
                let api_secret = cfg
                    .keys
                    .clob_api_secret
                    .as_deref()
                    .unwrap_or_default()
                    .trim();
                let api_passphrase = cfg
                    .keys
                    .clob_api_passphrase
                    .as_deref()
                    .unwrap_or_default()
                    .trim();

                if api_key.is_empty() {
                    return Err(BotError::Config(
                        "missing required key: PMMB_KEYS__CLOB_API_KEY".to_string(),
                    ));
                }
                if api_secret.is_empty() {
                    return Err(BotError::Config(
                        "missing required key: PMMB_KEYS__CLOB_API_SECRET".to_string(),
                    ));
                }
                if api_passphrase.is_empty() {
                    return Err(BotError::Config(
                        "missing required key: PMMB_KEYS__CLOB_API_PASSPHRASE".to_string(),
                    ));
                }

                let api_key_uuid = Uuid::from_str(api_key)
                    .map_err(|e| BotError::Config(format!("invalid CLOB API key uuid: {e}")))?;
                Credentials::new(
                    api_key_uuid,
                    api_secret.to_string(),
                    api_passphrase.to_string(),
                )
            }
            ApiCredsSource::Derive => {
                tracing::info!(
                    target: "clob_rest",
                    wallet_mode = ?cfg.keys.wallet_mode,
                    "deriving CLOB API credentials via /auth/*"
                );
                client
                    .create_or_derive_api_key(&signer, None)
                    .await
                    .map_err(|e| BotError::Other(format!("sdk derive api key failed: {e}")))?
            }
        };

        let api_creds = ClobApiCreds {
            api_key: creds.key().to_string(),
            api_secret: creds.secret().expose_secret().to_string(),
            api_passphrase: creds.passphrase().expose_secret().to_string(),
        };

        let mut auth = client
            .authentication_builder(&signer)
            .credentials(creds)
            .signature_type(signature_type);

        let mut funder = cfg
            .keys
            .funder_address
            .as_deref()
            .map(str::trim)
            .unwrap_or("");
        let mut derived = None;
        if cfg.keys.wallet_mode == WalletMode::ProxySafe && funder.is_empty() {
            derived = derive_proxy_wallet(signer.address(), POLYGON);
            if let Some(addr) = derived {
                funder = ""; // keep empty; set via auth.funder below
                tracing::info!(
                    target: "clob_rest",
                    funder_address = %addr,
                    "derived proxy wallet address via CREATE2"
                );
                auth = auth.funder(addr);
            } else {
                return Err(BotError::Config(
                    "wallet_mode=PROXY_SAFE but proxy wallet derivation failed; set keys.funder_address explicitly"
                        .to_string(),
                ));
            }
        }

        if !funder.is_empty() {
            let addr = Address::from_str(funder)
                .map_err(|e| BotError::Config(format!("invalid funder_address: {e}")))?;
            if cfg.keys.wallet_mode == WalletMode::ProxySafe {
                if let Some(derived_addr) = derived {
                    if derived_addr != addr {
                        tracing::warn!(
                            target: "clob_rest",
                            configured = %addr,
                            derived = %derived_addr,
                            "funder_address does not match derived proxy wallet"
                        );
                    }
                } else if let Some(derived_addr) = derive_proxy_wallet(signer.address(), POLYGON) {
                    if derived_addr != addr {
                        tracing::warn!(
                            target: "clob_rest",
                            configured = %addr,
                            derived = %derived_addr,
                            "funder_address does not match derived proxy wallet"
                        );
                    }
                }
            }
            auth = auth.funder(addr);
        }

        let client = auth
            .authenticate()
            .await
            .map_err(|e| BotError::Other(format!("sdk authenticate failed: {e}")))?;

        Ok(Self {
            inner: Some(Arc::new(SdkHandle {
                client,
                signer,
                api_creds,
                fee_rate_cache: FeeRateCache::new(DEFAULT_FEE_RATE_TTL_MS),
                fee_rate_http: reqwest::Client::new(),
            })),
        })
    }

    pub fn authenticated(&self) -> bool {
        self.inner.is_some()
    }

    pub fn api_creds(&self) -> Option<ClobApiCreds> {
        self.inner.as_ref().map(|inner| inner.api_creds.clone())
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
            let price = parse_decimal(order.price, 8)?;
            let size = parse_decimal(order.size, 2)?;

            inner.client.set_fee_rate_bps(token_id, fee_rate_bps as u32);

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
        side: Side,
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
        let p_max = parse_decimal_trunc(p_max, 8)?;
        let fee_rate_bps = self.get_fee_rate_bps_for_id(token_id).await?;

        inner.client.set_fee_rate_bps(token_id, fee_rate_bps as u32);

        // NOTE: Completion is modeled as a marketable limit order (FAK/FOK) with an explicit
        // limit price `p_max` (cap for buys / floor for sells). This avoids any ambiguity around
        // "market order" amount units and targets an exact share delta.
        let signable = inner
            .client
            .limit_order()
            .token_id(token_id)
            .price(p_max)
            .size(shares)
            .side(side)
            .order_type(map_completion_order_type(order_type))
            .post_only(false)
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

    pub async fn get_active_orders_for_market(
        &self,
        condition_id: &str,
    ) -> BotResult<Vec<OpenOrderResponse>> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            BotError::Other("active_orders unavailable: client not authenticated".to_string())
        })?;
        let market = parse_condition_id(condition_id)?;
        let request = OrdersRequest::builder().market(market).build();

        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page: Page<OpenOrderResponse> = inner
                .client
                .orders(&request, cursor.clone())
                .await
                .map_err(|e| BotError::Other(format!("active_orders fetch failed: {e}")))?;
            out.extend(page.data);
            if page.next_cursor.trim().is_empty() {
                break;
            }
            cursor = Some(page.next_cursor);
        }

        Ok(out)
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
                // The SDK internally caches `fee_rate_bps` with no TTL. We bypass that cache so
                // our TTL is honored, and then we explicitly seed the SDK cache right before
                // signing via `set_fee_rate_bps(...)`.
                fetch_fee_rate_bps_uncached(inner, token_id).await
            })
            .await
    }

    pub async fn post_orders(&self, orders: Vec<OrderRequest>) -> BotResult<Vec<OrderId>> {
        let result = self.post_orders_result(orders).await?;
        let mut failures = Vec::new();
        for rejected in &result.rejected {
            failures.push(rejected.error.clone());
        }

        if !failures.is_empty() {
            return Err(BotError::Other(format!(
                "clob post_orders failed: {}",
                failures.join("; ")
            )));
        }

        Ok(result
            .accepted
            .into_iter()
            .map(|item| item.order_id)
            .collect())
    }

    pub async fn post_orders_result(
        &self,
        orders: Vec<OrderRequest>,
    ) -> BotResult<PostOrdersResult> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            BotError::Other("post_orders unavailable: client not authenticated".to_string())
        })?;
        if orders.is_empty() {
            return Ok(PostOrdersResult::default());
        }

        let token_ids: Vec<Option<String>> =
            orders.iter().map(extract_signed_order_token_id).collect();

        let responses = inner
            .client
            .post_orders(orders)
            .await
            .map_err(|e| BotError::Other(format!("post_orders failed: {e}")))?;

        Ok(post_orders_result_from_responses(responses, &token_ids))
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
            failed: resp.not_canceled,
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
            failed: resp.not_canceled,
        })
    }
}

fn extract_signed_order_token_id(order: &OrderRequest) -> Option<String> {
    Some(order.order.tokenId.to_string())
}

fn post_orders_result_from_responses(
    responses: Vec<PostOrderResponse>,
    token_ids: &[Option<String>],
) -> PostOrdersResult {
    let mut result = PostOrdersResult::default();
    let response_len = responses.len();
    for (idx, resp) in responses.into_iter().enumerate() {
        let token_id = token_ids.get(idx).cloned().unwrap_or(None);
        let order_id_trimmed = resp.order_id.trim();
        let has_order_id = !order_id_trimmed.is_empty();
        let normalized_error = resp
            .error_msg
            .as_deref()
            .map(str::trim)
            .filter(|msg| !msg.is_empty())
            .map(|msg| msg.to_string());

        // Some API responses include `success=false` with an empty error message but still return an
        // `order_id` and `status=LIVE`/`UNMATCHED`. Treat these as accepted to avoid hidden live
        // orders causing exposure/cap drift. This matches observed live behavior on AWS.
        let inconsistent_but_live = !resp.success
            && normalized_error.is_none()
            && has_order_id
            && matches!(
                resp.status,
                OrderStatusType::Live | OrderStatusType::Unmatched | OrderStatusType::Matched
            );

        if inconsistent_but_live {
            tracing::warn!(
                target: "clob_rest",
                idx,
                token_id = ?token_id,
                order_id = %resp.order_id,
                status = ?resp.status,
                success = resp.success,
                action = "post_order_accept_inconsistent",
                "post_orders returned success=false but no error; treating as accepted"
            );
            result.accepted.push(PostOrderAccepted {
                idx,
                order_id: resp.order_id,
                token_id,
            });
            continue;
        }

        let error = if !resp.success {
            Some(normalized_error.unwrap_or_else(|| {
                format!("unknown error (success=false status={:?})", resp.status)
            }))
        } else if let Some(err) = normalized_error {
            Some(err)
        } else if !has_order_id {
            Some("missing order_id".to_string())
        } else {
            None
        };

        if let Some(err) = error {
            let order_id = if has_order_id {
                Some(resp.order_id)
            } else {
                None
            };
            tracing::warn!(
                target: "clob_rest",
                idx,
                token_id = ?token_id,
                order_id = order_id.as_deref().unwrap_or(""),
                status = ?resp.status,
                success = resp.success,
                error = %err,
                action = "post_order_reject",
                "order rejected"
            );
            result.rejected.push(PostOrderRejected {
                idx,
                error: err,
                order_id,
                token_id,
            });
        } else {
            tracing::info!(
                target: "clob_rest",
                idx,
                token_id = ?token_id,
                order_id = %resp.order_id,
                status = ?resp.status,
                success = resp.success,
                action = "post_order_accept",
                "order accepted"
            );
            result.accepted.push(PostOrderAccepted {
                idx,
                order_id: resp.order_id,
                token_id,
            });
        }
    }

    if response_len < token_ids.len() {
        for idx in response_len..token_ids.len() {
            let token_id = token_ids.get(idx).cloned().unwrap_or(None);
            let err = "missing response".to_string();
            tracing::warn!(
                target: "clob_rest",
                idx,
                token_id = ?token_id,
                error = %err,
                action = "post_order_reject",
                "order rejected"
            );
            result.rejected.push(PostOrderRejected {
                idx,
                error: err,
                order_id: None,
                token_id,
            });
        }
    } else if response_len > token_ids.len() {
        tracing::warn!(
            target: "clob_rest",
            response_len,
            request_len = token_ids.len(),
            "post_orders returned more responses than requests"
        );
    }

    result
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
        WalletMode::ProxySafe => SignatureType::Proxy,
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

fn parse_condition_id(condition_id: &str) -> BotResult<B256> {
    let condition_id = condition_id.trim();
    if condition_id.is_empty() {
        return Err(BotError::Other("condition_id empty".to_string()));
    }
    B256::from_str(condition_id).map_err(|e| BotError::Other(format!("invalid condition_id: {e}")))
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

fn parse_decimal_trunc(value: f64, max_dp: usize) -> BotResult<Decimal> {
    if !value.is_finite() || value < 0.0 {
        return Err(BotError::Other(format!("invalid decimal {value}")));
    }
    if max_dp == 0 {
        let int_part = value.floor();
        let s = format!("{int_part:.0}");
        return s
            .parse::<Decimal>()
            .map_err(|e| BotError::Other(format!("invalid decimal string {s}: {e}")));
    }

    let scale = 10u128
        .checked_pow(max_dp as u32)
        .ok_or_else(|| BotError::Other("decimal scale overflow".to_string()))?;
    let scaled = (value * scale as f64).floor();
    if !scaled.is_finite() {
        return Err(BotError::Other(format!("invalid decimal {value}")));
    }
    let scaled = scaled as u128;
    let int_part = scaled / scale;
    let frac_part = scaled % scale;

    let mut s = format!("{int_part}.{:0width$}", frac_part, width = max_dp);
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

async fn fetch_fee_rate_bps_uncached(inner: &SdkHandle, token_id: U256) -> BotResult<u64> {
    let base = inner.client.host().as_str().trim_end_matches('/');
    let url = format!("{base}/fee-rate");

    let resp = inner
        .fee_rate_http
        .get(url)
        .query(&[("token_id", token_id.to_string())])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read response body>".to_string());
        let body = body.chars().take(300).collect::<String>();
        return Err(BotError::Other(format!(
            "fee_rate fetch failed: status={status} token_id={token_id} body={body}"
        )));
    }

    let parsed = resp.json::<FeeRateResponse>().await?;
    Ok(parsed.base_fee as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn completion_order_type_mapping() {
        assert_eq!(
            map_completion_order_type(CompletionOrderType::Fak),
            SdkOrderType::FAK
        );
        assert_eq!(
            map_completion_order_type(CompletionOrderType::Fok),
            SdkOrderType::FOK
        );
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

        let res = tokio::time::timeout(
            Duration::from_millis(500),
            run_heartbeat_supervisor(api.clone(), cfg),
        )
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

    #[tokio::test]
    async fn fee_rate_cache_refetches_after_ttl() {
        let cache = FeeRateCache::new(100);
        let calls = Arc::new(AtomicUsize::new(0));

        let fetch = |calls: Arc<AtomicUsize>| async move {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            Ok(40u64 + n as u64)
        };

        let first = cache
            .get_or_fetch("token", 0, || fetch(calls.clone()))
            .await
            .expect("fetch failed");
        let second = cache
            .get_or_fetch("token", 101, || fetch(calls.clone()))
            .await
            .expect("fetch failed");

        assert_eq!(first, 40);
        assert_eq!(second, 41);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn parses_fee_rate_response_base_fee() {
        let parsed: FeeRateResponse =
            serde_json::from_str(r#"{"base_fee":12}"#).expect("parse FeeRateResponse");
        assert_eq!(parsed.base_fee, 12);
    }

    #[test]
    fn post_orders_result_keeps_accepted_on_partial_failure() {
        fn resp(success: bool, order_id: &str, error: Option<&str>) -> PostOrderResponse {
            serde_json::from_value(serde_json::json!({
                "errorMsg": error,
                "makingAmount": "0",
                "takingAmount": "0",
                "orderID": order_id,
                "status": "LIVE",
                "success": success,
                "transactionHashes": [],
                "tradeIds": [],
            }))
            .expect("deserialize PostOrderResponse")
        }

        let responses = vec![
            resp(true, "order-1", None),
            resp(false, "", Some("insufficient balance")),
            resp(true, "order-3", None),
        ];
        let token_ids = vec![
            Some("100".to_string()),
            Some("200".to_string()),
            Some("300".to_string()),
        ];

        let result = post_orders_result_from_responses(responses, &token_ids);

        assert_eq!(result.accepted.len(), 2);
        assert_eq!(result.rejected.len(), 1);

        assert_eq!(result.accepted[0].idx, 0);
        assert_eq!(result.accepted[0].order_id, "order-1");
        assert_eq!(result.accepted[1].idx, 2);
        assert_eq!(result.accepted[1].order_id, "order-3");

        assert_eq!(result.rejected[0].idx, 1);
        assert_eq!(result.rejected[0].error, "insufficient balance");
    }

    #[test]
    fn post_orders_result_accepts_inconsistent_success_false_with_order_id() {
        let responses = vec![serde_json::from_value(serde_json::json!({
            "errorMsg": "",
            "makingAmount": "0",
            "takingAmount": "0",
            "orderID": "order-1",
            "status": "LIVE",
            "success": false,
            "transactionHashes": [],
            "tradeIds": [],
        }))
        .expect("deserialize PostOrderResponse")];

        let token_ids = vec![Some("100".to_string())];
        let result = post_orders_result_from_responses(responses, &token_ids);

        assert_eq!(result.accepted.len(), 1);
        assert_eq!(result.rejected.len(), 0);
        assert_eq!(result.accepted[0].order_id, "order-1");
    }

    #[tokio::test]
    async fn completion_order_is_marketable_limit_order_in_sdk() {
        use polymarket_client_sdk::clob::types::TickSize;

        let token_id = U256::from(123u64);

        let signer: PrivateKeySigner =
            "0000000000000000000000000000000000000000000000000000000000000001"
                .parse::<PrivateKeySigner>()
                .expect("parse signer")
                .with_chain_id(Some(POLYGON));

        let creds = Credentials::new(Uuid::nil(), "secret".to_string(), "pass".to_string());
        let sdk = SdkClient::new("http://localhost", SdkConfig::default()).expect("sdk client");

        let mut client = sdk
            .authentication_builder(&signer)
            .credentials(creds)
            .signature_type(SignatureType::Eoa)
            .authenticate()
            .await
            .expect("authenticate");
        client.stop_heartbeats().await.expect("stop heartbeats");

        client.set_tick_size(token_id, TickSize::Hundredth);
        client.set_neg_risk(token_id, false);

        let fee_rate_cache = FeeRateCache::new(DEFAULT_FEE_RATE_TTL_MS);
        fee_rate_cache
            .set(&token_id.to_string(), 12, now_ms())
            .await;

        let rest = ClobRestClient {
            inner: Some(Arc::new(SdkHandle {
                client,
                signer,
                api_creds: ClobApiCreds {
                    api_key: "key".to_string(),
                    api_secret: "secret".to_string(),
                    api_passphrase: "pass".to_string(),
                },
                fee_rate_cache,
                fee_rate_http: reqwest::Client::new(),
            })),
        };

        let signed = rest
            .build_completion_order(
                "123",
                Side::Buy,
                10.0,
                0.35,
                CompletionOrderType::Fak,
                now_ms(),
            )
            .await
            .expect("build completion order");

        assert_eq!(signed.order.tokenId, token_id);
        assert_eq!(signed.order.side, Side::Buy as u8);
        assert_eq!(signed.order_type, SdkOrderType::FAK);
        assert_eq!(signed.post_only, Some(false));
        assert_eq!(signed.order.feeRateBps, U256::from(12u64));
        assert_eq!(signed.order.takerAmount, U256::from(10_000_000u64));
        assert_eq!(signed.order.makerAmount, U256::from(3_500_000u64));

        let signed_sell = rest
            .build_completion_order(
                "123",
                Side::Sell,
                10.0,
                0.35,
                CompletionOrderType::Fak,
                now_ms(),
            )
            .await
            .expect("build completion sell order");

        assert_eq!(signed_sell.order.tokenId, token_id);
        assert_eq!(signed_sell.order.side, Side::Sell as u8);
        assert_eq!(signed_sell.order_type, SdkOrderType::FAK);
        assert_eq!(signed_sell.post_only, Some(false));
        assert_eq!(signed_sell.order.feeRateBps, U256::from(12u64));
        assert_eq!(signed_sell.order.makerAmount, U256::from(10_000_000u64));
        assert_eq!(signed_sell.order.takerAmount, U256::from(3_500_000u64));
    }
}
