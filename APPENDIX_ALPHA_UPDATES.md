# APPENDIX_ALPHA_UPDATES.md
# Appendix: Alpha-First + Safety Patches (apply to existing docs, do NOT rewrite them)
Date: 2026-01-26
Audience: Codex coding agents + maintainers
Purpose: We already started coding from the prior docs, so we cannot “replace all docs”.
This appendix lists the minimum set of MUST-APPLY patch blocks and task changes, and points to which existing .md file(s) each patch belongs in.

================================================================================
0) Summary of why this appendix exists
================================================================================

We received two external reviews that correctly identified critical gaps:
1) Capital velocity: missing CTF "merge" loop => capital locked until resolution.
2) Oracle alignment: BTC 15m markets settle on Chainlink BTC/USD stream; "Binance vs Chainlink divergence => halt" is wrong/incentive-misaligned.
3) Execution safety: taker completion must use strict worst-price (FOK/FAK) rather than “marketable limit”.

This appendix upgrades the strategy to be competitive from day 1:
- Profit mechanism becomes: capture set-completion edge + immediately recycle via MERGE.
- Alpha gating aligns to settlement oracle (Chainlink), with Binance as leading indicator only.
- Completion trades use explicit limit and FOK/FAK order types.
- Heartbeats become mandatory.
- Stress/replay becomes mandatory (not optional).

IMPORTANT: PRD.md and QUESTIONS_FOR_RESEARCHER.md do not exist in this project anymore.
Therefore: any reference to them in AGENTS.md or elsewhere must be removed/updated.

================================================================================
1) Patch Index (what to change, and where)
================================================================================

Apply patches in this order:
(1) FULL_SPEC.md: add Merge + Oracle Alignment + Strict Completion + Perf/Stress + Profit clarity
(2) TECH_SPEC.md: add InventoryEngine(Merge) + OracleSource semantics + FOK/FAK completion API + mandatory heartbeats + stress harness
(3) TASK_BREAKDOWN.md: add/adjust tasks to implement Merge + Oracle alignment + Stress/Replay; fix dependencies
(4) RISK_REGISTER.md: add risks around merge/gas, oracle staleness, and program dependencies
(5) AGENTS.md: remove PRD/Questions references; add “this appendix is mandatory”

Each patch block below is ready to copy/paste into the target doc.
Use the “INSERT LOCATION” notes to place it.

================================================================================
2) FULL_SPEC.md — REQUIRED PATCHES
================================================================================

------------------------------------------------------------
FULL-PATCH-A: Settlement Oracle Alignment (Chainlink is truth)
------------------------------------------------------------
TARGET FILE: FULL_SPEC.md
INSERT LOCATION: In the “Definitions / Data sources / Market model” section (early), near anything describing BTC reference price.
RATIONALE: BTC 15m markets explicitly resolve using Chainlink BTC/USD stream. Binance is not the settlement oracle.

PASTE BLOCK:

### Settlement Oracle Source (Required)
- The resolution source for BTC “Up/Down 15-minute” markets is the Chainlink BTC/USD data stream (settlement truth).
- Therefore:
  - The bot’s probability model and S0/S_end comparisons MUST be computed on Chainlink BTC/USD whenever available.
  - Binance (RTDS `crypto_prices`) is used only as a leading indicator for toxicity and fast-move detection, not as truth.
- If Chainlink feed is stale:
  - bot must treat the market as “degraded” and reduce exposure or pause (configurable), because settlement truth is unavailable.

Operational note:
- We still subscribe to both RTDS sources:
  - Chainlink: crypto_prices_chainlink (BTC/USD)
  - Binance: crypto_prices (BTCUSDT)

------------------------------------------------------------
FULL-PATCH-B: Replace “RTDS Divergence => Halt” with Oracle-Health & Fast-Move Regimes
------------------------------------------------------------
TARGET FILE: FULL_SPEC.md
INSERT LOCATION: Replace the existing “RTDS divergence logic” section (or append a “V2: supersedes divergence logic” note right below it).
RATIONALE: Chainlink updates can be slower; divergence is expected during fast moves and should not cause constant halts.

PASTE BLOCK:

### RTDS Regimes (Replaces naive “divergence halt”)
We define regimes based on (A) feed health and (B) fast move conditions.

Inputs (RTDS):
- P_chainlink: latest Chainlink BTC/USD price (crypto_prices_chainlink)
- P_binance:   latest Binance BTCUSDT price (crypto_prices)
- ts_chainlink, ts_binance
- now_ms

Config thresholds:
- rtds_chainlink_stale_ms
- rtds_binance_stale_ms
- fast_move_window_ms
- fast_move_threshold_bps
- oracle_disagree_threshold_bps (large; used only as a “warning” unless paired with staleness)

Regimes:
1) STALE_ORACLE (hard)
- if now_ms - ts_chainlink > rtds_chainlink_stale_ms:
  - Cancel all open orders for affected markets
  - Pause quoting until Chainlink feed is fresh again
Reason: settlement oracle is unknown.

2) STALE_BINANCE (soft)
- if now_ms - ts_binance > rtds_binance_stale_ms:
  - Reduce sizes (size_scalar < 1)
  - Keep quoting if Chainlink is healthy
Reason: lose early-warning, but settlement oracle still available.

3) FAST_MOVE
- computed from Binance returns (preferred) OR Chainlink returns if Binance stale:
  - if abs(return over fast_move_window_ms) > fast_move_threshold_bps:
    - Reduce sizes (size_scalar)
    - Reduce target_total (require more edge)
    - Optionally reduce ladder_levels (less exposure)
Reason: avoid toxic fills; do not necessarily halt.

4) ORACLE_DISAGREE (warning, not automatic halt)
- if abs(P_binance_to_usd - P_chainlink)/mid > oracle_disagree_threshold_bps:
  - Mark WARNING
  - No halt unless ALSO Chainlink is stale or we detect WS staleness.
Reason: divergence is common; settlement is Chainlink.

------------------------------------------------------------
FULL-PATCH-C: Add Merge Loop (Capital Velocity Core)
------------------------------------------------------------
TARGET FILE: FULL_SPEC.md
INSERT LOCATION: In “Inventory / Settlement / Redemption” section.
RATIONALE: Full sets can be merged back into collateral any time after condition prepared; this recycles capital.

PASTE BLOCK:

### Capital Velocity: MERGE full sets (Required)
We MUST implement a merge loop to recycle collateral during the market, not only after resolution.

Definition:
- full_sets = min(pos_up_shares, pos_down_shares)

Behavior:
- If full_sets >= merge_min_sets AND merge_enabled=true:
  - Merge `merge_qty = floor(full_sets / merge_batch_sets) * merge_batch_sets`
  - After merge, reduce both pos_up and pos_down by merge_qty, and increase collateral balance by merge_qty (in USDC units).
  - Repeat periodically, rate-limited.

Constraints:
- Merge can be executed any time after a condition has been prepared on the CTF contract.
- One unit of each position in a full set is burned in return for 1 unit of collateral.
- Merge is idempotent at the state level if confirmed by onchain receipt or relayer response.

Wallet mode / gas:
- If using Polymarket proxy wallet routed through relayer: Polymarket pays gas for CTF operations (split, merge, redeem).
- If using standalone EOA: we pay gas; therefore merge must be rate-limited and thresholded to avoid death-by-gas.

Config keys:
- merge_enabled (bool, default true)
- merge_min_sets (e.g., 25)
- merge_batch_sets (e.g., 25)
- merge_interval_s (e.g., 10)
- merge_max_ops_per_minute (e.g., 6)
- merge_pause_during_fast_move (bool, default true)
- merge_wallet_mode: RELAYER | EOA

Accounting:
- Merged collateral increases “available to quote” budget; this is a core performance lever.

------------------------------------------------------------
FULL-PATCH-D: Strict Completion (FOK/FAK) replaces “marketable limit”
------------------------------------------------------------
TARGET FILE: FULL_SPEC.md
INSERT LOCATION: In “Execution / taker completion / emergency completion” section.
RATIONALE: “marketable limit” can over-cross a thin book. We want explicit worst-price.

PASTE BLOCK:

### Completion Trades MUST use explicit worst-price + FOK/FAK
When we need to complete pairs quickly (inventory risk), taker-mode is allowed ONLY under this strict model:

Inputs:
- desired completion qty Q (shares)
- computed maximum acceptable price P_max (based on EV after fees and/or emergency loss cap)
- current top-of-book ask P_ask

Rule:
- We place an immediate order with a hard price cap P_max, using:
  - FOK if we require full completion immediately (all-or-nothing)
  - FAK if partial completion is acceptable

Implementation:
- Use OrderType.FOK or OrderType.FAK (per CLOB docs) with explicit limit price = P_max.
- Never submit an unbounded “marketable” order.
- If P_ask > P_max: do NOT take; fall back to maker-aggressive completion (bid at best_ask - tick, still post-only=false only if explicitly allowed).

Fee handling:
- Before any completion trade, compute fee estimate from documented fee-equivalent formula and/or observed effective fee.
- Enforce max loss per completion burst (config).

------------------------------------------------------------
FULL-PATCH-E: Mandatory Heartbeats + Cancel-on-Disconnect Behavior
------------------------------------------------------------
TARGET FILE: FULL_SPEC.md
INSERT LOCATION: In “Execution safety / WS disconnect handling” section.
RATIONALE: If the bot dies, orders must vanish promptly.

PASTE BLOCK:

### Heartbeats are mandatory
- The bot MUST enable the official CLOB heartbeat / cancel-on-disconnect mechanism.
- If heartbeats are not active or fail for > heartbeat_grace_ms:
  - immediately cancel all open orders
  - pause quoting until heartbeats are restored
- “Watchdog-based” cancellation is not sufficient as the primary protection.

Config keys:
- heartbeats_enabled (bool, default true)
- heartbeat_interval_ms (e.g., 1000)
- heartbeat_grace_ms (e.g., 2500)

------------------------------------------------------------
FULL-PATCH-F: Profit mechanism clarified (so we’re not “bot #500”)
------------------------------------------------------------
TARGET FILE: FULL_SPEC.md
INSERT LOCATION: In “Strategy / Moat / How we make money” section.
RATIONALE: The doc must explicitly tie alpha, merge, and incentives to profit.

PASTE BLOCK:

### How we make money (explicit)
Primary deterministic mechanism:
1) Acquire paired inventory (Up + Down) at total cost < 1.0 (set-completion edge).
2) Immediately MERGE paired sets back into collateral to recycle capital and compound ROI.

Why this is not commodity:
- Many bots “hold to resolution” and suffer capital lock; merging converts edge into velocity.
- Our oracle alignment uses Chainlink (settlement truth) and uses Binance only for fast-move detection.
- Our completion trades have explicit worst-price caps (FOK/FAK), reducing tail-loss from thin books.
- Incentives (maker rebates, liquidity rewards) are treated as additive upside, never the sole profit source.

Non-negotiable operational requirement:
- We log everything for replay and calibration (see performance & data sections).

------------------------------------------------------------
FULL-PATCH-G: Performance / Stress Requirements (mandatory)
------------------------------------------------------------
TARGET FILE: FULL_SPEC.md
INSERT LOCATION: Ops / Testing / Performance section.
RATIONALE: Without explicit performance requirements, we can’t know if it survives volatility.

PASTE BLOCK:

### Performance & Stress Requirements (mandatory)
We must define and test performance targets:

Throughput targets (minimum):
- Market WS messages: sustain 2,000 msgs/sec without falling behind (no unbounded queue growth).
- RTDS updates: sustain 200 msgs/sec combined.
- User WS: sustain burst fill updates without missing order state.

Latency targets (p95 on a healthy host):
- From “book update received” -> “desired quotes computed”: <= 20ms
- From “desired quote changed” -> “REST submission attempted”: <= 100ms (excluding rate-limit waits)

Stress test harness:
- Provide a replay-driven stress test that replays recorded WS + RTDS logs at 1x and 5x speed.
- The bot must remain stable, bounded memory, and keep invariants (no cross, no post-cutoff orders).

================================================================================
3) TECH_SPEC.md — REQUIRED PATCHES
================================================================================

------------------------------------------------------------
TECH-PATCH-A: Add InventoryEngine with MERGE
------------------------------------------------------------
TARGET FILE: TECH_SPEC.md
INSERT LOCATION: Architecture section + module list.
PASTE BLOCK:

### InventoryEngine (new core module)
Add a dedicated InventoryEngine actor responsible for:
- monitoring pos_up/pos_down per market
- deciding when to MERGE full sets to collateral (capital velocity)
- deciding when to REDEEM post-resolution
- managing wallet mode differences:
  - RELAYER/PROXY: gasless, can merge more frequently
  - EOA: merge thresholding and max ops/min to cap gas burn

Interfaces:
- InventoryEngine::tick(now_ms, market_states_snapshot) -> Vec<InventoryAction>
- InventoryAction:
  - Merge { condition_id, qty_sets }
  - Redeem { condition_id, outcome, qty }
  - PauseMerges { reason }

------------------------------------------------------------
TECH-PATCH-B: OracleSource semantics (Chainlink primary)
------------------------------------------------------------
TARGET FILE: TECH_SPEC.md
INSERT LOCATION: AlphaEngine inputs + RTDS integration.
PASTE BLOCK:

### Oracle-aligned alpha (Chainlink primary)
- AlphaEngine must compute S0 and St from Chainlink BTC/USD (RTDS chainlink source) when available.
- Binance feed is used for fast-move detection, not for settlement truth.
- Regime decisions:
  - Chainlink stale => hard pause
  - Binance stale => size reduction only

------------------------------------------------------------
TECH-PATCH-C: Strict completion using OrderType FOK/FAK
------------------------------------------------------------
TARGET FILE: TECH_SPEC.md
INSERT LOCATION: Execution / OrderManager section.
PASTE BLOCK:

### Completion orders use FOK/FAK with explicit price cap
OrderManager must support:
- Maker quoting: GTC limit, postOnly=true
- Completion/taker: FOK or FAK “market order” behavior with explicit limit price P_max

This relies on CLOB order types:
- FOK: fill entirely immediately or cancel
- FAK: fill available immediately, cancel remainder

Implementation guidance:
- prefer SDK method createAndPostMarketOrder (FOK/FAK) if available,
  else use REST orderType=FOK/FAK with signed order payload and explicit price.

------------------------------------------------------------
TECH-PATCH-D: Heartbeats mandatory
------------------------------------------------------------
TARGET FILE: TECH_SPEC.md
INSERT LOCATION: WS + execution safety.
PASTE BLOCK:

### Heartbeats mandatory
- Heartbeats must be enabled and supervised by a dedicated task.
- On heartbeat failure beyond grace:
  - emit global CancelAll
  - pause quoting until restored

------------------------------------------------------------
TECH-PATCH-E: Add replay-driven stress harness (mandatory)
------------------------------------------------------------
TARGET FILE: TECH_SPEC.md
INSERT LOCATION: Testing section.
PASTE BLOCK:

### Replay + stress harness is mandatory
- Persist a unified event log (market ws + user ws + RTDS + decisions).
- Provide:
  - deterministic replay mode (feeds events into StateManager and StrategyEngine)
  - stress mode (replay at N-times speed, measure queue sizes, memory, and latency)

================================================================================
4) TASK_BREAKDOWN.md — REQUIRED PATCHES
================================================================================

We are not rewriting the breakdown; we are adding/modifying a few tasks.

------------------------------------------------------------
TASK-PATCH-A: Add a new task “InventoryEngine Merge”
------------------------------------------------------------
TARGET FILE: TASK_BREAKDOWN.md
INSERT LOCATION: After the inventory/fills tasks.
PASTE BLOCK:

## NEW TASK: InventoryEngine (Merge + Redeem)
Goal:
- Implement capital-velocity merge loop + post-resolution redeem.
Why:
- MergePositions burns full sets for collateral anytime after condition prepared.

Scope:
- Track full_sets = min(pos_up, pos_down)
- Merge when >= merge_min_sets with batching/rate limits
- Wallet-mode-aware gas policy (Relayer vs EOA)

Acceptance:
- Unit test: merge decision thresholds
- Integration test: mocked relayer/onchain call invoked
- Invariant: merged collateral increases available quoting budget

Dependencies:
- Inventory state from user WS
- Market condition_id from discovery

------------------------------------------------------------
TASK-PATCH-B: Modify RTDS task to “oracle health + regimes” (not divergence halt)
------------------------------------------------------------
TARGET FILE: TASK_BREAKDOWN.md
CHANGE:
- In the RTDS/Alpha tasks, replace “divergence halt” with:
  - Chainlink stale => halt
  - Binance stale => size down
  - fast move => size down + widen edge
  - oracle disagree => warning only

Acceptance:
- tests show no constant halts during fast move unless Chainlink is stale

------------------------------------------------------------
TASK-PATCH-C: Modify execution task: add completion orders (FOK/FAK) with explicit cap
------------------------------------------------------------
TARGET FILE: TASK_BREAKDOWN.md
CHANGE:
- Add “CompletionOrders” sub-scope to execution task.
- Must support:
  - OrderType.FOK and OrderType.FAK with explicit limit P_max.

Acceptance:
- unit tests: if ask > P_max => do not submit
- unit tests: correct orderType used for completion

------------------------------------------------------------
TASK-PATCH-D: Add replay + stress harness as non-optional task
------------------------------------------------------------
TARGET FILE: TASK_BREAKDOWN.md
INSERT LOCATION: Ops/testing tasks.
PASTE BLOCK:

## NEW TASK: Replay + Stress Harness (Mandatory)
Goal:
- Build log format + replay runner + stress runner (1x, 5x speeds)
- Validate:
  - bounded queues/memory
  - invariants (no cross, no post-cutoff, no orders during STALE_ORACLE)

Acceptance:
- CI includes a stress test on a recorded fixture
- produces latency stats (p50/p95)

------------------------------------------------------------
TASK-PATCH-E: Fix dependency graph for AlphaEngine
------------------------------------------------------------
TARGET FILE: TASK_BREAKDOWN.md
CHANGE:
- AlphaEngine depends on:
  - MarketDiscovery (needs start time + condition_id)
  - RTDS Chainlink feed (primary)
  - Inventory state (for risk) and tick sizes (for rounding)
- Update wave ordering accordingly.

================================================================================
5) RISK_REGISTER.md — REQUIRED PATCHES
================================================================================

Add these explicit risks:

- Capital velocity risk:
  - Without merge, capital is locked until resolution and ROI collapses.
- Oracle alignment risk:
  - Using Binance as truth causes systematic mispricing vs Chainlink-settled outcome.
- Gas risk (EOA mode):
  - merging too frequently bleeds MATIC; must be thresholded.
- Completion slippage risk:
  - “marketable limit” can slip; must be FOK/FAK with explicit cap.

Add mitigations:
- Merge loop with thresholds and wallet-mode policies
- Chainlink-primary oracle model
- Completion orders with explicit cap + max loss budget
- Heartbeats mandatory

================================================================================
6) AGENTS.md — REQUIRED PATCHES
================================================================================

AGENTS.md must reflect that PRD.md and QUESTIONS_FOR_RESEARCHER.md do not exist.

------------------------------------------------------------
AGENTS-PATCH-A: Update “read order” and remove non-existent docs
------------------------------------------------------------
TARGET FILE: AGENTS.md
CHANGE:
- Replace any “read PRD.md / QUESTIONS_FOR_RESEARCHER.md” references with:
  1) FULL_SPEC.md
  2) TECH_SPEC.md
  3) TASK_BREAKDOWN.md
  4) RISK_REGISTER.md
  5) APPENDIX_ALPHA_UPDATES.md (this file)

Also add:
- “If there is any conflict between existing docs and this appendix, this appendix wins for the patched sections.”

------------------------------------------------------------
AGENTS-PATCH-B: Add mandatory items checklist
------------------------------------------------------------
TARGET FILE: AGENTS.md
ADD:
- Merge loop is required.
- Chainlink is settlement truth; Binance is warning.
- Completion uses FOK/FAK with explicit cap.
- Heartbeats required.
- Replay/stress required.

================================================================================
7) Config additions (ensure existing configs get these keys)
================================================================================

These keys must be added to config schema (wherever it lives in your code/spec):

[oracle]
chainlink_stale_ms = 1500
binance_stale_ms   = 1500
fast_move_window_ms = 1000
fast_move_threshold_bps = 10
oracle_disagree_threshold_bps = 75   # warning only, not halt by itself

[merge]
enabled = true
min_sets = 25
batch_sets = 25
interval_s = 10
max_ops_per_minute = 6
pause_during_fast_move = true
wallet_mode = "RELAYER" | "EOA"

[completion]
enabled = true
order_type = "FAK" | "FOK"
max_loss_usdc = 10
min_profit_per_share = 0.0005
use_explicit_price_cap = true

[heartbeats]
enabled = true
interval_ms = 1000
grace_ms = 2500

================================================================================
8) What NOT to do (to avoid bot-500 failure mode)
================================================================================

- Do NOT halt quoting just because Binance != Chainlink (that’s normal).
- Do NOT treat Binance as settlement truth for Up/Down resolution.
- Do NOT use unbounded “marketable limit” for completion.
- Do NOT defer merging full sets until resolution; that kills capital velocity.

================================================================================
9) Quick “apply checklist” for maintainers
================================================================================

- [ ] FULL_SPEC.md patched with A..G blocks
- [ ] TECH_SPEC.md patched with A..E blocks
- [ ] TASK_BREAKDOWN.md: add tasks for merge + replay/stress; fix alpha dependencies; adjust RTDS logic
- [ ] RISK_REGISTER.md updated with merge/oracle/completion risks
- [ ] AGENTS.md updated: remove missing doc references; add “this appendix is mandatory”
- [ ] Config schema updated with oracle/merge/completion/heartbeats sections
