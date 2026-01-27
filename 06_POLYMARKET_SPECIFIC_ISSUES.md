# Polymarket-Specific Issues — CLOB/WS/RTDS/CTF Pitfalls

**Severity: CRITICAL**

This section flags Polymarket integration issues that are likely to cause hard failures (no data, rejected orders) or subtle PnL leaks (fee/signing mismatches).

## CRITICAL issues

### 1) RTDS Chainlink subscribe payload does not match repo spec

- Spec reference: `FULL_SPEC.md` 5.3 requires:
  - topic `crypto_prices_chainlink`, type `*`, filter `{"symbol":"btc/usd"}` (object).
- Evidence: `src/clients/rtds_ws.rs::build_subscribe_message()` sends Chainlink filters as a **string**.
- Expected behavior if wrong: no Chainlink updates, permanent STALE_ORACLE behavior.
- Action: align payload to spec; add a one-shot startup assertion: “received chainlink within X ms”.

### 2) Liquidity rewards scoring integration cannot function end-to-end

- Evidence:
  - `ClobRestClient::are_orders_scoring()` exists (`src/clients/clob_rest.rs`).
  - `RewardEngine` wants to call it, but does not have live order IDs because `MarketState.orders` is never populated.
- Action: feed order IDs from OrderManager/user WS into a scoring subsystem.

### 3) Merge wallet mode RELAYER is explicitly not implemented

- Evidence: `src/inventory/onchain.rs` warns and disables merges/redeems when `MergeWalletMode::Relayer`.
- Action: implement relayer integration or fail fast at config validation.

## HIGH issues / schema correctness risks (UNKNOWN until validated)

1) **CLOB market WS subscribe/unsubscribe schema**
   - Evidence: `src/clients/clob_ws_market.rs` uses:
     - initial: `{ "type":"market", "assets_ids":[...], "custom_feature_enabled": ... }`
     - operations: `{ "operation":"subscribe|unsubscribe", "assets_ids":[...], ... }` (no `"type"` field)
   - UNKNOWN: whether the server accepts operation messages without `"type":"market"`.
   - Experiment: subscribe, then unsubscribe+resubscribe at runtime and confirm updates resume.

2) **User WS auth field casing**
   - Evidence: `src/clients/clob_ws_user.rs` uses `apiKey` (camel) and notes docs ambiguity.
   - UNKNOWN: whether `apikey` vs `apiKey` is required in all environments.
   - Experiment: intentionally send wrong casing in a test harness; verify expected auth error.

3) **User WS order “rejection” event types**
   - Evidence: parser currently ignores unknown types.
   - Experiment: trigger a known reject and record the raw frame; update parser/tests accordingly.

## Fee/signing notes (mostly good, but verify)

1) **FeeRateBps signing**
   - Evidence: `ClobRestClient` fetches `fee_rate_bps(token_id)` and calls `set_fee_rate_bps()` before signing (mutex-protected).
   - Risk: fee-rate fetch failure or stale caching can manifest as intermittent rejects and “quote holes”.
   - Action:
     - Make fee-rate fetch failure a **market-level pause** (do not place orders with unknown fee rate).
     - Add a runtime metric/log that records feeRateBps used for each signed order and correlates with rejects.

2) **Completion order construction**
   - Evidence: `build_completion_order()` uses SDK `market_order()` builder with `FAK/FOK` and an explicit `price(p_max)`.
   - UNKNOWN: confirm SDK maps this to the documented completion order shape for 15m markets.
   - Experiment: submit a tiny completion order in a sandbox market and verify:
     - rejection if `best_ask > p_max`,
     - partial fills for FAK, all-or-nothing for FOK.

## Rate limits + cancel/replace churn (Polymarket-specific failure spiral)

- Risk: aggressive cancel/replace on Polymarket leads to 429 throttling, queue loss, and intermittent quoting gaps.
- Current state: no explicit token-bucket limiter; backoff exists but currently blocks cancels too (unsafe).
- Action:
  - Implement an explicit **order ops budget** (ops/sec and ops/min) and enforce it in execution:
    - under budget pressure: widen, reduce levels/sizes, increase debounce; do not keep churning.
  - Track 429s separately and apply jittered backoff.

## Scoring endpoint usage (UNKNOWN until validated)

- `are_orders_scoring` exists, but the end-to-end feedback loop is broken due to missing order-id plumbing.
- Action:
  - Once order IDs are available, run a small experiment tool that hits the scoring endpoint for:
    - representative ladder orders,
    - near-crossing orders,
    - intentionally invalid orders,
    and record response schema + latency + pass/fail behavior.
