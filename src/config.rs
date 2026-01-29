use std::path::Path;

use figment::providers::{Env, Format, Json, Serialized, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};

use crate::error::{BotError, BotResult};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AppConfig {
    pub trading: TradingConfig,
    pub inventory: InventoryConfig,
    pub alpha: AlphaConfig,
    pub oracle: OracleConfig,
    pub merge: MergeConfig,
    pub completion: CompletionConfig,
    pub heartbeats: HeartbeatsConfig,
    pub rewards: RewardsConfig,
    pub infra: InfraConfig,
    pub keys: KeysConfig,
    pub endpoints: EndpointsConfig,
}

impl AppConfig {
    fn apply_deprecated_overrides(&mut self) {
        let default_alpha = AlphaConfig::default();
        let default_oracle = OracleConfig::default();
        let default_inventory = InventoryConfig::default();
        let default_completion = CompletionConfig::default();

        if self.alpha.market_ws_stale_ms == default_alpha.market_ws_stale_ms
            && self.alpha.rtds_stale_ms != default_alpha.rtds_stale_ms
        {
            self.alpha.market_ws_stale_ms = self.alpha.rtds_stale_ms;
        }

        if self.oracle.chainlink_stale_ms == default_oracle.chainlink_stale_ms
            && self.oracle.binance_stale_ms == default_oracle.binance_stale_ms
            && self.alpha.rtds_stale_ms != default_alpha.rtds_stale_ms
        {
            self.oracle.chainlink_stale_ms = self.alpha.rtds_stale_ms;
            self.oracle.binance_stale_ms = self.alpha.rtds_stale_ms;
        }

        if self.oracle.fast_move_threshold_bps == default_oracle.fast_move_threshold_bps
            && self.alpha.fast_move_threshold_bps != default_alpha.fast_move_threshold_bps
        {
            self.oracle.fast_move_threshold_bps = self.alpha.fast_move_threshold_bps;
        }

        if self.oracle.oracle_disagree_threshold_bps == default_oracle.oracle_disagree_threshold_bps
            && self.alpha.rtds_divergence_threshold != default_alpha.rtds_divergence_threshold
        {
            self.oracle.oracle_disagree_threshold_bps =
                self.alpha.rtds_divergence_threshold * 10_000.0;
        }

        if self.completion.min_profit_per_share == default_completion.min_profit_per_share
            && self.inventory.completion_min_profit_per_share
                != default_inventory.completion_min_profit_per_share
        {
            self.completion.min_profit_per_share = self.inventory.completion_min_profit_per_share;
        }

        if self.completion.enabled == default_completion.enabled
            && self.inventory.allow_taker_completion != default_inventory.allow_taker_completion
        {
            self.completion.enabled = self.inventory.allow_taker_completion;
        }
    }

    /// Backwards-compatible env var aliases for migrating from older bots.
    ///
    /// These are only used to fill missing values; explicit `PMMB_...` config always wins.
    fn apply_legacy_env_overrides(&mut self) {
        fn env_trimmed(key: &str) -> Option<String> {
            let value = std::env::var(key).ok()?;
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }

        fn env_bool(key: &str) -> Option<bool> {
            let value = env_trimmed(key)?;
            match value.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "y" | "on" => Some(true),
                "0" | "false" | "no" | "n" | "off" => Some(false),
                _ => None,
            }
        }

        let legacy_private_key = env_trimmed("BOT_PRIVATE_KEY");
        if self
            .keys
            .private_key
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            if let Some(pk) = legacy_private_key {
                self.keys.private_key = Some(pk);
            }
        }

        let legacy_funder = env_trimmed("CLOB_ADDRESS");
        if self
            .keys
            .funder_address
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            if let Some(addr) = legacy_funder.clone() {
                self.keys.funder_address = Some(addr);
            }
        }

        if std::env::var("PMMB_KEYS__WALLET_MODE").is_err()
            && self.keys.wallet_mode == WalletMode::Eoa
            && legacy_funder.is_some()
        {
            self.keys.wallet_mode = WalletMode::ProxySafe;
        }

        if self
            .keys
            .clob_api_key
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            self.keys.clob_api_key = env_trimmed("CLOB_API_KEY");
        }
        if self
            .keys
            .clob_api_secret
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            self.keys.clob_api_secret = env_trimmed("CLOB_API_SECRET");
        }
        if self
            .keys
            .clob_api_passphrase
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            self.keys.clob_api_passphrase = env_trimmed("CLOB_API_PASSPHRASE");
        }

        if std::env::var("PMMB_KEYS__API_CREDS_SOURCE").is_err()
            && self.keys.api_creds_source == ApiCredsSource::Explicit
            && env_bool("CLOB_AUTO_DERIVE_ENABLED") == Some(true)
        {
            self.keys.api_creds_source = ApiCredsSource::Derive;
        }
    }

    pub fn validate(&self) -> BotResult<()> {
        self.trading.validate()?;
        self.inventory.validate()?;
        self.alpha.validate()?;
        self.oracle.validate()?;
        self.merge.validate()?;
        self.completion.validate()?;
        self.heartbeats.validate()?;
        if !self.trading.dry_run && !self.heartbeats.enabled {
            return Err(BotError::Config(
                "heartbeats.enabled must be true when trading.dry_run=false".to_string(),
            ));
        }
        self.rewards.validate()?;
        self.infra.validate()?;
        self.keys.validate(self.trading.dry_run)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EndpointsConfig {
    pub clob_rest_base_url: String,
    pub clob_ws_market_url: String,
    pub clob_ws_user_url: String,
    pub gamma_base_url: String,
    pub data_api_base_url: String,
    pub geoblock_url: String,
    pub rtds_ws_url: String,
    pub polygon_rpc_url: String,
}

impl Default for EndpointsConfig {
    fn default() -> Self {
        Self {
            clob_rest_base_url: "https://clob.polymarket.com".to_string(),
            clob_ws_market_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string(),
            clob_ws_user_url: "wss://ws-subscriptions-clob.polymarket.com/ws/user".to_string(),
            gamma_base_url: "https://gamma-api.polymarket.com".to_string(),
            data_api_base_url: "https://data-api.polymarket.com".to_string(),
            geoblock_url: "https://polymarket.com/api/geoblock".to_string(),
            rtds_ws_url: "wss://ws-live-data.polymarket.com".to_string(),
            polygon_rpc_url: "https://polygon-rpc.com".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TradingConfig {
    /// When true, never posts/cancels orders; used for connectivity/integration tests.
    pub dry_run: bool,
    /// When true, cancels all open orders on startup before trading.
    pub startup_cancel_all: bool,

    pub target_total_base: f64,
    pub target_total_min: f64,
    pub target_total_max: f64,

    /// When enabled, allow `target_total` to widen (go lower) under toxic conditions
    /// (high vol, wide spreads, close to cutoff, fast move).
    pub adaptive_target_total_enabled: bool,
    pub target_total_min_toxic: f64,
    pub target_total_max_toxic: f64,

    pub cutoff_seconds: i64,
    /// Hard per-market USDC exposure cap (inventory cost + open order notional).
    pub max_usdc_exposure_per_market: f64,

    /// Do not quote below this price. If either outcome is trading below this floor, quoting pauses.
    pub min_quote_price: f64,

    pub ladder_levels: usize,
    pub ladder_step_ticks: u64,

    /// When enabled, reduce ladder depth / widen spacing under higher volatility.
    pub adaptive_ladder_enabled: bool,
    pub adaptive_ladder_vol_ratio_threshold: f64,
    pub ladder_levels_toxic: usize,
    pub ladder_step_ticks_toxic: u64,
    pub size_decay: f64,

    pub base_size_shares_current: f64,
    pub base_size_shares_next: f64,
    pub min_order_size_shares: f64,

    pub base_improve_ticks: u64,
    pub max_improve_ticks: u64,
    pub min_ticks_from_ask: u64,

    pub reprice_min_ticks: u64,
    pub resize_min_pct: f64,
    pub min_update_interval_ms: u64,

    /// Hard cap on the number of orders posted per minute (across markets).
    /// 0 disables the limiter.
    pub max_orders_per_min: u32,

    // --- Adverse-selection / pairing controls ---
    /// Pause maker quoting for a short period after detecting negative fill markout.
    pub markout_cooldown_enabled: bool,
    pub markout_horizon_short_ms: i64,
    pub markout_horizon_long_ms: i64,
    pub markout_bad_threshold_bps: f64,
    pub markout_cooldown_ms: i64,
    pub markout_ewma_alpha: f64,
    pub markout_min_fills_before_activation: u64,

    /// Scale down maker aggressiveness when inventory pairing is deteriorating.
    pub pair_protection_enabled: bool,
    pub pair_protection_ratio_threshold: f64,
    pub pair_protection_min_total_shares: f64,
    pub pair_protection_unpaired_duration_s: i64,
    pub pair_protection_max_ladder_levels: usize,
    pub pair_protection_size_scale_min: f64,
    pub pair_protection_repair_extra_improve_ticks: u64,

    /// Limit how fast level-0 price can move upward (anti-pickoff).
    pub chase_limiter_enabled: bool,
    pub chase_level0_max_up_ticks_per_s: f64,
    pub chase_level0_max_up_ticks_per_s_repair: f64,

    /// Avoid canceling all depth on a repair-only token after a good repair fill.
    pub selective_cancel_on_fill_enabled: bool,
    pub selective_cancel_on_fill_near_ticks: u64,
    pub selective_cancel_on_fill_repair_max_level: usize,
}

impl Default for TradingConfig {
    fn default() -> Self {
        Self {
            dry_run: true,
            startup_cancel_all: true,
            // Paired MM edge target: keep p_up + p_down in [0.98, 0.995]
            // (i.e., ~0.5–2.0¢ theoretical edge), per user requirements.
            target_total_base: 0.992,
            target_total_min: 0.98,
            target_total_max: 0.995,
            adaptive_target_total_enabled: true,
            // In toxic regimes we only widen modestly (still within 0.98–0.995).
            target_total_min_toxic: 0.98,
            target_total_max_toxic: 0.99,
            cutoff_seconds: 60,
            max_usdc_exposure_per_market: 10.0,
            min_quote_price: 0.20,
            ladder_levels: 2,
            ladder_step_ticks: 1,
            adaptive_ladder_enabled: true,
            adaptive_ladder_vol_ratio_threshold: 2.0,
            ladder_levels_toxic: 1,
            ladder_step_ticks_toxic: 1,
            size_decay: 0.6,
            base_size_shares_current: 5.0,
            base_size_shares_next: 3.0,
            min_order_size_shares: 1.0,
            base_improve_ticks: 0,
            max_improve_ticks: 2,
            min_ticks_from_ask: 1,
            reprice_min_ticks: 1,
            resize_min_pct: 0.10,
            min_update_interval_ms: 250,
            max_orders_per_min: 300,

            markout_cooldown_enabled: true,
            markout_horizon_short_ms: 1_000,
            markout_horizon_long_ms: 5_000,
            markout_bad_threshold_bps: 10.0,
            markout_cooldown_ms: 3_000,
            markout_ewma_alpha: 0.2,
            markout_min_fills_before_activation: 5,

            pair_protection_enabled: true,
            pair_protection_ratio_threshold: 0.85,
            pair_protection_min_total_shares: 1.0,
            pair_protection_unpaired_duration_s: 20,
            pair_protection_max_ladder_levels: 1,
            pair_protection_size_scale_min: 0.5,
            pair_protection_repair_extra_improve_ticks: 1,

            chase_limiter_enabled: true,
            chase_level0_max_up_ticks_per_s: 5.0,
            chase_level0_max_up_ticks_per_s_repair: 5.0,

            selective_cancel_on_fill_enabled: true,
            selective_cancel_on_fill_near_ticks: 2,
            selective_cancel_on_fill_repair_max_level: 0,
        }
    }
}

impl TradingConfig {
    pub fn validate(&self) -> BotResult<()> {
        if !(0.20..=1.0).contains(&self.min_quote_price) {
            return Err(BotError::Config(format!(
                "trading.min_quote_price must be in [0.20,1], got {}",
                self.min_quote_price
            )));
        }
        if !(0.0..=1.0).contains(&self.target_total_base) {
            return Err(BotError::Config(format!(
                "trading.target_total_base must be in [0,1], got {}",
                self.target_total_base
            )));
        }
        if !(0.0..=1.0).contains(&self.target_total_min) {
            return Err(BotError::Config(format!(
                "trading.target_total_min must be in [0,1], got {}",
                self.target_total_min
            )));
        }
        if !(0.0..=1.0).contains(&self.target_total_max) {
            return Err(BotError::Config(format!(
                "trading.target_total_max must be in [0,1], got {}",
                self.target_total_max
            )));
        }
        if self.target_total_min > self.target_total_max {
            return Err(BotError::Config(format!(
                "trading.target_total_min ({}) > trading.target_total_max ({})",
                self.target_total_min, self.target_total_max
            )));
        }

        if self.target_total_base + 1e-12 < self.target_total_min
            || self.target_total_base > self.target_total_max + 1e-12
        {
            return Err(BotError::Config(format!(
                "trading.target_total_base ({}) must be within [trading.target_total_min ({}), trading.target_total_max ({})]",
                self.target_total_base, self.target_total_min, self.target_total_max
            )));
        }

        if !(0.0..=1.0).contains(&self.target_total_min_toxic) {
            return Err(BotError::Config(format!(
                "trading.target_total_min_toxic must be in [0,1], got {}",
                self.target_total_min_toxic
            )));
        }
        if !(0.0..=1.0).contains(&self.target_total_max_toxic) {
            return Err(BotError::Config(format!(
                "trading.target_total_max_toxic must be in [0,1], got {}",
                self.target_total_max_toxic
            )));
        }
        if self.target_total_min_toxic > self.target_total_max_toxic {
            return Err(BotError::Config(format!(
                "trading.target_total_min_toxic ({}) > trading.target_total_max_toxic ({})",
                self.target_total_min_toxic, self.target_total_max_toxic
            )));
        }

        // Adaptive target_total is intended to widen (go lower) in toxicity.
        if self.target_total_min_toxic > self.target_total_min + 1e-12 {
            return Err(BotError::Config(format!(
                "trading.target_total_min_toxic ({}) must be <= trading.target_total_min ({})",
                self.target_total_min_toxic, self.target_total_min
            )));
        }
        if self.target_total_max_toxic > self.target_total_max + 1e-12 {
            return Err(BotError::Config(format!(
                "trading.target_total_max_toxic ({}) must be <= trading.target_total_max ({})",
                self.target_total_max_toxic, self.target_total_max
            )));
        }

        if self.adaptive_ladder_vol_ratio_threshold < 0.0
            || !self.adaptive_ladder_vol_ratio_threshold.is_finite()
        {
            return Err(BotError::Config(format!(
                "trading.adaptive_ladder_vol_ratio_threshold must be finite and >=0, got {}",
                self.adaptive_ladder_vol_ratio_threshold
            )));
        }
        if self.ladder_levels < 2 {
            return Err(BotError::Config(format!(
                "trading.ladder_levels must be >=2, got {}",
                self.ladder_levels
            )));
        }
        if self.ladder_levels_toxic == 0 {
            return Err(BotError::Config(format!(
                "trading.ladder_levels_toxic must be >=1, got {}",
                self.ladder_levels_toxic
            )));
        }
        if self.ladder_step_ticks_toxic == 0 {
            return Err(BotError::Config(
                "trading.ladder_step_ticks_toxic must be >=1".to_string(),
            ));
        }
        if !self.max_usdc_exposure_per_market.is_finite()
            || self.max_usdc_exposure_per_market <= 0.0
        {
            return Err(BotError::Config(format!(
                "trading.max_usdc_exposure_per_market must be >0, got {}",
                self.max_usdc_exposure_per_market
            )));
        }
        if self.min_order_size_shares <= 0.0 {
            return Err(BotError::Config(format!(
                "trading.min_order_size_shares must be >0, got {}",
                self.min_order_size_shares
            )));
        }

        if self.markout_horizon_short_ms <= 0 {
            return Err(BotError::Config(format!(
                "trading.markout_horizon_short_ms must be >0, got {}",
                self.markout_horizon_short_ms
            )));
        }
        if self.markout_horizon_long_ms <= self.markout_horizon_short_ms {
            return Err(BotError::Config(format!(
                "trading.markout_horizon_long_ms ({}) must be > trading.markout_horizon_short_ms ({})",
                self.markout_horizon_long_ms, self.markout_horizon_short_ms
            )));
        }
        if !self.markout_bad_threshold_bps.is_finite() || self.markout_bad_threshold_bps <= 0.0 {
            return Err(BotError::Config(format!(
                "trading.markout_bad_threshold_bps must be finite and >0, got {}",
                self.markout_bad_threshold_bps
            )));
        }
        if self.markout_cooldown_ms <= 0 {
            return Err(BotError::Config(format!(
                "trading.markout_cooldown_ms must be >0, got {}",
                self.markout_cooldown_ms
            )));
        }
        if !(0.0..=1.0).contains(&self.markout_ewma_alpha) {
            return Err(BotError::Config(format!(
                "trading.markout_ewma_alpha must be in [0,1], got {}",
                self.markout_ewma_alpha
            )));
        }

        if !self.pair_protection_ratio_threshold.is_finite()
            || self.pair_protection_ratio_threshold <= 0.0
            || self.pair_protection_ratio_threshold > 1.0
        {
            return Err(BotError::Config(format!(
                "trading.pair_protection_ratio_threshold must be in (0,1], got {}",
                self.pair_protection_ratio_threshold
            )));
        }
        if !self.pair_protection_min_total_shares.is_finite()
            || self.pair_protection_min_total_shares < 0.0
        {
            return Err(BotError::Config(format!(
                "trading.pair_protection_min_total_shares must be finite and >=0, got {}",
                self.pair_protection_min_total_shares
            )));
        }
        if self.pair_protection_unpaired_duration_s < 0 {
            return Err(BotError::Config(format!(
                "trading.pair_protection_unpaired_duration_s must be >=0, got {}",
                self.pair_protection_unpaired_duration_s
            )));
        }
        if self.pair_protection_max_ladder_levels == 0 {
            return Err(BotError::Config(
                "trading.pair_protection_max_ladder_levels must be >=1".to_string(),
            ));
        }
        if !self.pair_protection_size_scale_min.is_finite()
            || self.pair_protection_size_scale_min <= 0.0
            || self.pair_protection_size_scale_min > 1.0
        {
            return Err(BotError::Config(format!(
                "trading.pair_protection_size_scale_min must be in (0,1], got {}",
                self.pair_protection_size_scale_min
            )));
        }

        if !self.chase_level0_max_up_ticks_per_s.is_finite()
            || self.chase_level0_max_up_ticks_per_s < 0.0
        {
            return Err(BotError::Config(format!(
                "trading.chase_level0_max_up_ticks_per_s must be finite and >=0, got {}",
                self.chase_level0_max_up_ticks_per_s
            )));
        }
        if !self.chase_level0_max_up_ticks_per_s_repair.is_finite()
            || self.chase_level0_max_up_ticks_per_s_repair < 0.0
        {
            return Err(BotError::Config(format!(
                "trading.chase_level0_max_up_ticks_per_s_repair must be finite and >=0, got {}",
                self.chase_level0_max_up_ticks_per_s_repair
            )));
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InventoryConfig {
    pub skew_mild: f64,
    pub skew_moderate: f64,
    pub skew_severe: f64,

    pub max_unpaired_shares_per_market: f64,
    pub max_unpaired_shares_global: f64,

    pub emergency_window_s: i64,
    pub taker_window_s: i64,

    /// Deprecated: use completion.min_profit_per_share.
    pub completion_min_profit_per_share: f64,

    /// Deprecated: use completion.enabled.
    pub allow_taker_completion: bool,
    pub allow_negative_completion: bool,

    pub desync_watchdog_enabled: bool,
    pub desync_watchdog_interval_s: u64,
    pub desync_watchdog_mismatch_hold_s: u64,
    pub desync_watchdog_max_abs_shares_diff: f64,
}

impl Default for InventoryConfig {
    fn default() -> Self {
        Self {
            skew_mild: 0.10,
            skew_moderate: 0.25,
            skew_severe: 0.50,
            max_unpaired_shares_per_market: 250.0,
            max_unpaired_shares_global: 400.0,
            emergency_window_s: 90,
            taker_window_s: 30,
            completion_min_profit_per_share: 0.001,
            allow_taker_completion: true,
            allow_negative_completion: false,
            desync_watchdog_enabled: true,
            desync_watchdog_interval_s: 30,
            desync_watchdog_mismatch_hold_s: 60,
            desync_watchdog_max_abs_shares_diff: 0.05,
        }
    }
}

impl InventoryConfig {
    pub fn validate(&self) -> BotResult<()> {
        if self.max_unpaired_shares_per_market <= 0.0 {
            return Err(BotError::Config(format!(
                "inventory.max_unpaired_shares_per_market must be >0, got {}",
                self.max_unpaired_shares_per_market
            )));
        }
        if self.max_unpaired_shares_global <= 0.0 {
            return Err(BotError::Config(format!(
                "inventory.max_unpaired_shares_global must be >0, got {}",
                self.max_unpaired_shares_global
            )));
        }
        if self.desync_watchdog_interval_s == 0 {
            return Err(BotError::Config(
                "inventory.desync_watchdog_interval_s must be >0".to_string(),
            ));
        }
        if self.desync_watchdog_max_abs_shares_diff < 0.0
            || !self.desync_watchdog_max_abs_shares_diff.is_finite()
        {
            return Err(BotError::Config(format!(
                "inventory.desync_watchdog_max_abs_shares_diff must be finite and >=0, got {}",
                self.desync_watchdog_max_abs_shares_diff
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AlphaConfig {
    pub vol_halflife_s: f64,
    pub drift_halflife_s: f64,
    pub drift_clamp_per_s: f64,

    pub var_ref: f64,
    pub spread_ref: f64,
    pub k_vol: f64,
    pub k_spread: f64,
    pub k_time: f64,
    pub k_fast: f64,

    /// Deprecated: use oracle.fast_move_threshold_bps.
    pub fast_move_threshold_bps: f64,

    /// Deprecated: use oracle.chainlink_stale_ms / oracle.binance_stale_ms.
    pub rtds_stale_ms: i64,

    /// Market WS token staleness threshold.
    pub market_ws_stale_ms: i64,

    /// Deprecated: use oracle.oracle_disagree_threshold_bps (converted to bps).
    pub rtds_divergence_threshold: f64,

    /// Deprecated: use oracle.fast_move_window_ms for timing and oracle_disagree for warnings.
    pub divergence_hold_ms: i64,
}

impl Default for AlphaConfig {
    fn default() -> Self {
        Self {
            vol_halflife_s: 30.0,
            drift_halflife_s: 120.0,
            drift_clamp_per_s: 0.0,
            var_ref: 1e-8,
            spread_ref: 0.01,
            k_vol: 0.15,
            k_spread: 0.10,
            k_time: 0.05,
            k_fast: 0.15,
            fast_move_threshold_bps: 10.0,
            rtds_stale_ms: 750,
            market_ws_stale_ms: 750,
            rtds_divergence_threshold: 0.0015,
            divergence_hold_ms: 500,
        }
    }
}

impl AlphaConfig {
    pub fn validate(&self) -> BotResult<()> {
        if self.rtds_stale_ms <= 0 {
            return Err(BotError::Config(format!(
                "alpha.rtds_stale_ms must be >0, got {}",
                self.rtds_stale_ms
            )));
        }
        if self.market_ws_stale_ms <= 0 {
            return Err(BotError::Config(format!(
                "alpha.market_ws_stale_ms must be >0, got {}",
                self.market_ws_stale_ms
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OracleConfig {
    pub chainlink_stale_ms: i64,
    pub binance_stale_ms: i64,
    pub fast_move_window_ms: i64,
    pub fast_move_threshold_bps: f64,
    pub oracle_disagree_threshold_bps: f64,
}

impl Default for OracleConfig {
    fn default() -> Self {
        Self {
            // RTDS Chainlink updates can be bursty; keep this comfortably above the typical cadence
            // to avoid flapping pause/resume on healthy feeds.
            chainlink_stale_ms: 5_000,
            binance_stale_ms: 1_500,
            fast_move_window_ms: 1_000,
            fast_move_threshold_bps: 10.0,
            oracle_disagree_threshold_bps: 75.0,
        }
    }
}

impl OracleConfig {
    pub fn validate(&self) -> BotResult<()> {
        if self.chainlink_stale_ms <= 0 {
            return Err(BotError::Config(format!(
                "oracle.chainlink_stale_ms must be >0, got {}",
                self.chainlink_stale_ms
            )));
        }
        if self.binance_stale_ms <= 0 {
            return Err(BotError::Config(format!(
                "oracle.binance_stale_ms must be >0, got {}",
                self.binance_stale_ms
            )));
        }
        if self.fast_move_window_ms <= 0 {
            return Err(BotError::Config(format!(
                "oracle.fast_move_window_ms must be >0, got {}",
                self.fast_move_window_ms
            )));
        }
        if self.fast_move_threshold_bps < 0.0 {
            return Err(BotError::Config(format!(
                "oracle.fast_move_threshold_bps must be >=0, got {}",
                self.fast_move_threshold_bps
            )));
        }
        if self.oracle_disagree_threshold_bps < 0.0 {
            return Err(BotError::Config(format!(
                "oracle.oracle_disagree_threshold_bps must be >=0, got {}",
                self.oracle_disagree_threshold_bps
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeWalletMode {
    #[default]
    Relayer,
    Eoa,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MergeConfig {
    pub enabled: bool,
    pub min_sets: u64,
    pub batch_sets: u64,
    pub interval_s: u64,
    pub max_ops_per_minute: u64,
    pub pause_during_fast_move: bool,
    pub wallet_mode: MergeWalletMode,
    pub readiness_poll_interval_s: u64,
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_sets: 25,
            batch_sets: 25,
            interval_s: 10,
            max_ops_per_minute: 6,
            pause_during_fast_move: true,
            wallet_mode: MergeWalletMode::Eoa,
            readiness_poll_interval_s: 30,
        }
    }
}

impl MergeConfig {
    pub fn validate(&self) -> BotResult<()> {
        if self.min_sets == 0 {
            return Err(BotError::Config("merge.min_sets must be >0".to_string()));
        }
        if self.batch_sets == 0 {
            return Err(BotError::Config("merge.batch_sets must be >0".to_string()));
        }
        if self.batch_sets > self.min_sets {
            return Err(BotError::Config(format!(
                "merge.batch_sets ({}) must be <= merge.min_sets ({})",
                self.batch_sets, self.min_sets
            )));
        }
        if self.interval_s == 0 {
            return Err(BotError::Config("merge.interval_s must be >0".to_string()));
        }
        if self.max_ops_per_minute == 0 {
            return Err(BotError::Config(
                "merge.max_ops_per_minute must be >0".to_string(),
            ));
        }
        if self.readiness_poll_interval_s == 0 {
            return Err(BotError::Config(
                "merge.readiness_poll_interval_s must be >0".to_string(),
            ));
        }
        if self.enabled && self.wallet_mode == MergeWalletMode::Relayer {
            return Err(BotError::Config(
                "merge.wallet_mode=RELAYER is not implemented; use merge.wallet_mode=EOA or disable merges"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CompletionOrderType {
    #[default]
    Fak,
    Fok,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompletionConfig {
    pub enabled: bool,
    pub order_type: CompletionOrderType,
    pub max_loss_usdc: f64,
    pub min_profit_per_share: f64,
    pub use_explicit_price_cap: bool,
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            order_type: CompletionOrderType::Fak,
            max_loss_usdc: 10.0,
            min_profit_per_share: 0.0005,
            use_explicit_price_cap: true,
        }
    }
}

impl CompletionConfig {
    pub fn validate(&self) -> BotResult<()> {
        if self.max_loss_usdc < 0.0 {
            return Err(BotError::Config(format!(
                "completion.max_loss_usdc must be >=0, got {}",
                self.max_loss_usdc
            )));
        }
        if self.min_profit_per_share < 0.0 {
            return Err(BotError::Config(format!(
                "completion.min_profit_per_share must be >=0, got {}",
                self.min_profit_per_share
            )));
        }
        if self.enabled && !self.use_explicit_price_cap {
            return Err(BotError::Config(
                "completion.use_explicit_price_cap must be true when completion.enabled"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HeartbeatsConfig {
    pub enabled: bool,
    pub interval_ms: i64,
    pub grace_ms: i64,
}

impl Default for HeartbeatsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 1_000,
            grace_ms: 2_500,
        }
    }
}

impl HeartbeatsConfig {
    pub fn validate(&self) -> BotResult<()> {
        if self.interval_ms <= 0 {
            return Err(BotError::Config(format!(
                "heartbeats.interval_ms must be >0, got {}",
                self.interval_ms
            )));
        }
        if self.grace_ms <= 0 {
            return Err(BotError::Config(format!(
                "heartbeats.grace_ms must be >0, got {}",
                self.grace_ms
            )));
        }
        if self.grace_ms < self.interval_ms {
            return Err(BotError::Config(format!(
                "heartbeats.grace_ms ({}) must be >= heartbeats.interval_ms ({})",
                self.grace_ms, self.interval_ms
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RewardsConfig {
    pub enable_liquidity_rewards_chasing: bool,
    pub require_scoring_orders: bool,
    pub scoring_check_interval_s: i64,
}

impl Default for RewardsConfig {
    fn default() -> Self {
        Self {
            enable_liquidity_rewards_chasing: false,
            require_scoring_orders: false,
            scoring_check_interval_s: 30,
        }
    }
}

impl RewardsConfig {
    pub fn validate(&self) -> BotResult<()> {
        if self.scoring_check_interval_s <= 0 {
            return Err(BotError::Config(format!(
                "rewards.scoring_check_interval_s must be >0, got {}",
                self.scoring_check_interval_s
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InfraConfig {
    pub aws_region: String,
    pub log_level: String,
    pub metrics_port: u16,
    pub health_port: u16,
    pub quote_tick_interval_ms: u64,
    pub market_discovery_interval_ms: u64,
    pub market_discovery_grace_s: i64,
    pub geoblock_poll_interval_ms: u64,
}

impl Default for InfraConfig {
    fn default() -> Self {
        Self {
            aws_region: "eu-west-1".to_string(),
            log_level: "info".to_string(),
            metrics_port: 9090,
            health_port: 8080,
            quote_tick_interval_ms: 200,
            market_discovery_interval_ms: 1_000,
            market_discovery_grace_s: 20 * 60,
            geoblock_poll_interval_ms: 30_000,
        }
    }
}

impl InfraConfig {
    pub fn validate(&self) -> BotResult<()> {
        if self.quote_tick_interval_ms == 0 {
            return Err(BotError::Config(
                "infra.quote_tick_interval_ms must be >0".to_string(),
            ));
        }
        if self.market_discovery_interval_ms == 0 {
            return Err(BotError::Config(
                "infra.market_discovery_interval_ms must be >0".to_string(),
            ));
        }
        if self.market_discovery_grace_s < 0 {
            return Err(BotError::Config(format!(
                "infra.market_discovery_grace_s must be >=0, got {}",
                self.market_discovery_grace_s
            )));
        }
        if self.geoblock_poll_interval_ms == 0 {
            return Err(BotError::Config(
                "infra.geoblock_poll_interval_ms must be >0".to_string(),
            ));
        }
        if self.geoblock_poll_interval_ms < 250 {
            return Err(BotError::Config(format!(
                "infra.geoblock_poll_interval_ms must be >=250, got {}",
                self.geoblock_poll_interval_ms
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WalletMode {
    #[default]
    #[serde(alias = "Eoa", alias = "eoa")]
    Eoa,
    #[serde(alias = "ProxySafe", alias = "proxy_safe")]
    ProxySafe,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PrivateKeySource {
    #[default]
    #[serde(alias = "Env", alias = "env")]
    Env,
    #[serde(alias = "SecretsManager", alias = "secrets_manager")]
    SecretsManager,
    #[serde(alias = "Kms", alias = "kms")]
    Kms,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApiCredsSource {
    #[serde(alias = "derive")]
    Derive,
    #[default]
    #[serde(alias = "explicit")]
    Explicit,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct KeysConfig {
    pub wallet_mode: WalletMode,
    pub private_key_source: PrivateKeySource,
    pub api_creds_source: ApiCredsSource,

    pub funder_address: Option<String>,
    pub data_api_user: Option<String>,

    /// EOA private key (if wallet_mode=Eoa and private_key_source=Env)
    pub private_key: Option<String>,

    /// CLOB API credentials (if api_creds_source=Explicit)
    pub clob_api_key: Option<String>,
    pub clob_api_secret: Option<String>,
    pub clob_api_passphrase: Option<String>,
}

impl KeysConfig {
    pub fn validate(&self, dry_run: bool) -> BotResult<()> {
        if dry_run {
            return Ok(());
        }

        match self.private_key_source {
            PrivateKeySource::Env => {
                if self.private_key.as_deref().unwrap_or_default().is_empty() {
                    return Err(BotError::Config(
                        "missing required key: PMMB_KEYS__PRIVATE_KEY".to_string(),
                    ));
                }
            }
            PrivateKeySource::SecretsManager | PrivateKeySource::Kms => {
                return Err(BotError::Config(format!(
                    "keys.private_key_source={:?} is not implemented",
                    self.private_key_source
                )));
            }
        }

        match self.api_creds_source {
            ApiCredsSource::Derive => {}
            ApiCredsSource::Explicit => {
                if self.clob_api_key.as_deref().unwrap_or_default().is_empty() {
                    return Err(BotError::Config(
                        "missing required key: PMMB_KEYS__CLOB_API_KEY".to_string(),
                    ));
                }
                if self
                    .clob_api_secret
                    .as_deref()
                    .unwrap_or_default()
                    .is_empty()
                {
                    return Err(BotError::Config(
                        "missing required key: PMMB_KEYS__CLOB_API_SECRET".to_string(),
                    ));
                }
                if self
                    .clob_api_passphrase
                    .as_deref()
                    .unwrap_or_default()
                    .is_empty()
                {
                    return Err(BotError::Config(
                        "missing required key: PMMB_KEYS__CLOB_API_PASSPHRASE".to_string(),
                    ));
                }
            }
        }

        let funder = self.funder_address.as_deref().unwrap_or_default().trim();
        if self.wallet_mode == WalletMode::Eoa && !funder.is_empty() {
            return Err(BotError::Config(
                "keys.funder_address must be empty when keys.wallet_mode=EOA; use PMMB_KEYS__WALLET_MODE=PROXY_SAFE"
                    .to_string(),
            ));
        }

        Ok(())
    }
}

pub fn load_config() -> BotResult<AppConfig> {
    let figment = build_figment_from_env()?;
    load_config_from(figment)
}

fn build_figment_from_env() -> BotResult<Figment> {
    let mut figment = Figment::from(Serialized::defaults(AppConfig::default()));

    if let Ok(path) = std::env::var("PMMB_CONFIG_PATH") {
        figment = merge_config_file(figment, &path)?;
    }

    figment = figment.merge(Env::prefixed("PMMB_").split("__"));
    Ok(figment)
}

fn merge_config_file(figment: Figment, path: &str) -> BotResult<Figment> {
    let p = Path::new(path);
    match p.extension().and_then(|s| s.to_str()) {
        Some("toml") => Ok(figment.merge(Toml::file(path))),
        Some("json") => Ok(figment.merge(Json::file(path))),
        _ => Err(BotError::Config(format!(
            "unsupported config file extension for PMMB_CONFIG_PATH: {path} (expected .toml or .json)"
        ))),
    }
}

fn load_config_from(figment: Figment) -> BotResult<AppConfig> {
    let mut cfg: AppConfig = figment
        .extract()
        .map_err(|e| BotError::Config(e.to_string()))?;
    cfg.apply_deprecated_overrides();
    cfg.apply_legacy_env_overrides();
    cfg.validate()?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::Serialized;

    #[test]
    fn defaults_load() {
        let cfg =
            load_config_from(Figment::from(Serialized::defaults(AppConfig::default()))).unwrap();
        assert!(cfg.trading.ladder_levels >= 2);
        assert!(cfg.trading.cutoff_seconds > 0);
    }

    #[test]
    fn min_quote_price_floor_is_enforced() {
        let cfg = TradingConfig {
            min_quote_price: 0.19,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("trading.min_quote_price"), "{msg}");
        assert!(msg.contains("[0.20,1]"), "{msg}");
    }

    #[test]
    fn min_quote_price_floor_is_allowed() {
        let cfg = TradingConfig {
            min_quote_price: 0.20,
            ..Default::default()
        };
        cfg.validate().unwrap();
    }

    #[test]
    fn missing_required_keys_fails_with_clear_message() {
        let mut cfg = AppConfig::default();
        cfg.trading.dry_run = false;
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("PMMB_KEYS__PRIVATE_KEY"), "{msg}");
    }

    #[test]
    fn non_dry_run_requires_heartbeats() {
        let mut cfg = AppConfig::default();
        cfg.trading.dry_run = false;
        cfg.heartbeats.enabled = false;
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("heartbeats.enabled"), "{msg}");
    }

    #[test]
    fn relayer_merge_mode_is_rejected_when_enabled() {
        let mut cfg = AppConfig::default();
        cfg.merge.enabled = true;
        cfg.merge.wallet_mode = MergeWalletMode::Relayer;
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("merge.wallet_mode"), "{msg}");
    }

    #[test]
    fn unimplemented_private_key_sources_are_rejected_when_not_dry_run() {
        for source in [PrivateKeySource::SecretsManager, PrivateKeySource::Kms] {
            let mut cfg = AppConfig::default();
            cfg.trading.dry_run = false;
            cfg.keys.private_key_source = source;

            let err = cfg.validate().unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("keys.private_key_source"), "{msg}");
            assert!(msg.contains("not implemented"), "{msg}");
        }
    }

    #[test]
    fn derive_api_creds_source_is_allowed_when_not_dry_run() {
        let mut cfg = AppConfig::default();
        cfg.trading.dry_run = false;
        cfg.keys.private_key = Some("not-empty".to_string());
        cfg.keys.api_creds_source = ApiCredsSource::Derive;

        cfg.validate().unwrap();
    }
}
