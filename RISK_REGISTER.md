# RISK_REGISTER.md
# Polymarket BTC 15m Market-Maker Bot - Risk Register v1.1 (Alpha-first)

Last updated: 2026-01-26

1) Leg risk / unpaired exposure
- Risk: Getting filled heavily on one side -> directional exposure and loss.
- Mitigation:
  - Inventory skew ladder (mild/moderate/severe) with missing-side bias and excess-side suppression.
  - Emergency completion near cutoff (aggressive maker then taker only if positive EV).
  - Hard caps: max_unpaired_shares_per_market and global max.
  - Track cost basis and model q_up to avoid completing at negative EV by default.

2) Adverse selection / being picked off in fast moves
- Risk: Toxic flow when BTC moves quickly; passive quotes get hit before update.
- Mitigation (core moat):
  - RTDS BTC feed for low-latency detection of fast moves.
  - Toxicity regimes: FAST_MOVE reduces size and reduces target_total; STALE_ORACLE (Chainlink stale) pauses quoting and cancels orders.
  - Churn minimization to keep queue position but not at expense of quoting stale.
  - Fair-value caps: do not quote above model cap in NORMAL mode.

3) Feed staleness / data gaps
- Risk: WS disconnect or missed messages -> stale book -> bad quotes.
- Mitigation:
  - Staleness timers for market ws, user ws, RTDS.
  - Auto reconnect with exponential backoff.
  - If stale: cancel orders + pause quoting.
  - If L2 deltas used: sequence tracking and periodic resync.

4) Fee curve changes / feeRateBps signing requirement changes
- Risk: Order rejections or incorrect EV calculations if fees change.
- Mitigation:
  - Always fetch feeRateBps dynamically via /fee-rate for signing (cache with TTL).
  - Fee curve formula implemented from official maker rebates docs; unit tests match table points.
  - Monitor Polymarket changelog daily.

5) Maker rebates / liquidity rewards program parameter drift
- Risk: Program rules change; reward chasing could become negative EV.
- Mitigation:
  - Treat incentives as optional upside; do not violate risk constraints to chase them.
  - Scoring checks are gated behind enable_liquidity_rewards_chasing.
  - Metrics to separate pnl from incentives.

6) Rate limits / throttling
- Risk: API calls throttled -> slow order updates -> worse fills.
- Mitigation:
  - WS-first approach for data; REST only for execution.
  - Batch post/cancel.
  - Debounce and min_update_interval.
  - Token-bucket rate limiter per endpoint category.

7) Order churn causing loss of queue position
- Risk: Excess cancel/repost reduces fills and increases costs.
- Mitigation:
  - Reprice_min_ticks, resize_min_pct, min_update_interval.
  - Only update when necessary (crossing risk, cap violation, scoring requirements).

8) Credential leakage / key compromise
- Risk: Loss of funds.
- Mitigation:
  - Secrets Manager or env-only with locked-down host; never log secrets.
  - Strict IAM policies.
  - Optional KMS signing (trade latency).
  - Rotate API keys if suspected leak.

9) Geoblock / compliance restriction
- Risk: account restriction or banned.
- Mitigation:
  - Detect restricted responses and halt immediately.
  - Do not bypass geoblocks.
  - Alerts on any restricted/forbidden response.

10) Resolution delays / redemption failures
- Risk: Capital locked longer; redeem errors.
- Mitigation:
  - Redeemer with retries + alerting if unresolved beyond timeout.
  - Post-resolution playbook: cancel orders -> wait -> redeem -> record pnl.

11) Competition: sophisticated bots
- Risk: Our fill quality and edge erode.
- Mitigation (explicit moat):
  - RTDS gating + fair value caps (avoid toxic fills).
  - Incentive-aware quoting (scoring checks).
  - Data capture + replay to iterate quickly.
  - Multi-level ladder to capture depth + improve fill rate.

12) Infra / deployment outages
- Risk: bot stops, stale orders remain (if no heartbeat).
- Mitigation:
  - Heartbeats are mandatory (cancel-on-disconnect) and supervised; cancel all if heartbeats fail beyond grace.
  - Health checks + restart policy.
  - Graceful shutdown cancels orders.

13) Capital velocity / capital lock (merge missing)
- Risk: Without MERGE, capital is locked until resolution and ROI collapses.
- Mitigation:
  - Implement merge loop for full sets (min_sets + batching + rate limits).
  - Wallet-mode-aware policy: Relayer (gasless) vs EOA (thresholded).

14) Oracle alignment / systematic mispricing
- Risk: Using Binance as settlement truth causes systematic mispricing vs Chainlink-settled outcome.
- Mitigation:
  - Chainlink BTC/USD is the oracle truth for S0/St and probability model.
  - Binance is leading indicator only (fast-move detection), not a halt trigger by itself.

15) Gas risk (EOA mode merge/redeem)
- Risk: Over-merging in EOA mode bleeds gas and destroys edge.
- Mitigation:
  - Merge thresholds + max_ops_per_minute, batch_sets, and pause_during_fast_move.
  - Prefer RELAYER/PROXY mode when possible.

16) Completion slippage / thin book tail-loss
- Risk: “marketable limit” completion can over-cross thin books and create tail losses.
- Mitigation:
  - Completion orders must use FOK/FAK with explicit worst-price cap (P_max).
  - Enforce max_loss_usdc per completion burst and block if ask > P_max.
