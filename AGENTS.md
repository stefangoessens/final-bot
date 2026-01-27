# AGENTS.md
# Instructions for AI Coding Agents (Codex) - Polymarket BTC 15m MM Bot

Last updated: 2026-01-26

This repo is intended to be implemented primarily by AI coding agents.
Be explicit. No "magic". Follow the specs exactly.

------------------------------------------------------------
0) Read these docs first (required)
------------------------------------------------------------
- FULL_SPEC.md
- TECH_SPEC.md
- TASK_BREAKDOWN.md
- RISK_REGISTER.md
- APPENDIX_ALPHA_UPDATES.md
- TODO.md

If any conflict exists between existing docs and `APPENDIX_ALPHA_UPDATES.md`, `APPENDIX_ALPHA_UPDATES.md` wins for the patched sections.

------------------------------------------------------------
1) Official docs to reference (do not rely on memory)
------------------------------------------------------------
All API behaviors, urls, and schemas must be taken from the official docs:
- Data Feeds (CLOB WS URLs and example subscribe payloads):
  https://docs.polymarket.com/developers/market-makers/data-feeds
- Market WS schemas:
  https://docs.polymarket.com/developers/CLOB/websocket/market-channel
- User WS schemas:
  https://docs.polymarket.com/developers/CLOB/websocket/user-channel
- RTDS crypto prices schemas:
  https://docs.polymarket.com/developers/RTDS/RTDS-crypto-prices
- Maker rebates + fee curve formula:
  https://docs.polymarket.com/polymarket-learn/trading/maker-rebates-program
- FeeRateBps signing requirement:
  https://docs.polymarket.com/developers/market-makers/maker-rebates-program
- Order scoring endpoints:
  https://docs.polymarket.com/developers/CLOB/orders/check-scoring
- Rate limits:
  https://docs.polymarket.com/quickstart/introduction/rate-limits

If docs conflict with repo specs, add a comment and follow the official docs.

------------------------------------------------------------
2) Non-negotiable product requirements (alpha-first)
------------------------------------------------------------
- Must trade ONLY BTC 15m Up/Down markets.
- Must track current + next market.
- Must stop quoting and cancel orders 60 seconds before end.
- Must use RTDS BTC feed with Chainlink as settlement truth and pause quoting when Chainlink is stale.
- Must implement fair-value caps and dynamic target_total (alpha engine).
- Must implement multi-level ladder from the start (>=2 levels default).
- Must implement fee curve math for taker EV checks.
- Must implement data capture for replay (moat/flywheel).

Mandatory checklist (per `APPENDIX_ALPHA_UPDATES.md`):
- Merge loop is required (capital velocity).
- Chainlink BTC/USD is settlement truth; Binance is warning/fast-move only.
- Completion orders use FOK/FAK with explicit price cap (P_max).
- Heartbeats (cancel-on-disconnect) are mandatory.
- Replay + stress harness are mandatory (not optional).

------------------------------------------------------------
3) Implementation rules (parallelism and file ownership)
------------------------------------------------------------
- Each task owns a fixed set of files/modules (see TASK_BREAKDOWN.md).
- Do NOT edit files owned by other tasks unless unavoidable.
- If you must touch another task's file:
  - minimize diff
  - add a clear comment
  - document in the PR description what changed and why

State ownership rule:
- Only StateManager mutates MarketState.
- All other tasks send events to StateManager via channels.

------------------------------------------------------------
3.1) Multi-agent workflow (required)
------------------------------------------------------------
We work as:
- Orchestrator (this chat): assigns tasks, spawns subagents, integrates, runs tests.
- Coding subagents: implement ONE task each (per TASK_BREAKDOWN.md), in parallel when possible.

Rules:
- Always keep multiple subagents working concurrently on independent tasks.
- When a subagent finishes:
  - Orchestrator reviews the code in-depth.
  - If changes are needed: send the subagent back with requested edits.
  - If changes are small: orchestrator patches directly.
  - Then immediately spawn the next available task(s).
- Task status must be tracked in TODO.md:
  - `[ ]` = open, `[x]` = done.
  - When a task is fully done (tests + clippy), mark it `[x]`.

------------------------------------------------------------
3.2) When to ask GPT-5.2 Pro (optional but encouraged)
------------------------------------------------------------
Use GPT-5.2 Pro for high-leverage technical clarification/reviews:
- When docs are ambiguous or conflicting.
- When a signing/fee/rebate detail is unclear.
- When performance/latency tradeoffs need validation.

Constraints:
- Send at most 10 files or code snippets for review at a time.
- Ask for answers and/or updated `.md` documents when needed (e.g., clarifications, revised specs, runbooks).

------------------------------------------------------------
4) Coding style / quality bars
------------------------------------------------------------
Rust:
- Use tokio async tasks.
- Avoid blocking calls in async contexts.
- Prefer structured error types (thiserror/anyhow ok).
- Prefer explicit types over implicit "magic floats".
- Use f64 carefully for prices; always tick-round before sending orders.

Observability:
- Every network loop must log connect/disconnect/reconnect with backoff.
- Every order submission/cancel must log: market_slug, token_id, level, price, size, order_id (if any).
- Every fill must log and update inventory.

Safety:
- Never log secrets.
- On geoblock/restriction detection: cancel all and halt.

------------------------------------------------------------
5) Test requirements
------------------------------------------------------------
At minimum:
- Unit test: taker fee formula matches table points (price=0.5, shares=100 => ~0.78125).
- Unit test: tick rounding correctness.
- Unit test: combined cap enforcement.
- Unit test: q_up sanity checks.
- Integration test (dry-run): connect to RTDS and parse btcusdt messages.

------------------------------------------------------------
6) Definition of "Done" per task
------------------------------------------------------------
A task is only "done" when:
- Code compiles (cargo build)
- Tests pass (cargo test)
- Public interfaces match TECH_SPEC.md / TASK_BREAKDOWN.md
- Logs/metrics added for critical paths
- No silent TODOs for core behaviors

------------------------------------------------------------
7) Common pitfalls (avoid)
------------------------------------------------------------
- Quoting without RTDS gating -> likely picked off.
- Updating orders every book tick -> lose queue + throttle yourself.
- Ignoring tick size changes -> order rejections.
- Forgetting feeRateBps in signed payload on 15m markets -> order rejection.
- Assuming rebates are static -> they change; treat as optional upside.
