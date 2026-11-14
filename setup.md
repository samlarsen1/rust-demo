Set up this repository as a production-grade, strictly READ-ONLY Rust + Python (PyO3) quantitative portfolio intelligence and analytics demo (`rust_portfolio_intel`) tailored to my multi-currency (`GBP`, `USD`, `EUR`) investment tracking workflow.

CRITICAL DOMAIN CONSTRAINT: This system must be 100% READ-ONLY. It must NEVER contain order-execution, trade-placement, or broker-mutation logic. Its sole purpose is incremental local data caching, historical trend analysis, stock regime/style classification, and portfolio gap recommendations.

Please execute all of the following steps, verify every tool runs cleanly, and fix any warnings before finishing:

1. **Domain Model & Read-Only Analytics Engine (`src/`)**
   - Initialize a Cargo library crate (`src/lib.rs`) with a CLI demo binary (`src/main.rs`) and optional `pyo3` bindings feature flag (`extension-module`) so my Python agents can call it directly.
   - Use strongly typed financial domain models (`rust_decimal` for prices/dividends/valuations—never `f64` for money; `Currency` enum for `GBP`, `USD`, `EUR`; `StockArchetype` enum for `Growth`, `Income`, `Cyclical`, `Defensive`; and `TrendTerritory` enum for `Uptrend`, `Pullback`, `Correction`, `BearMarket`, `Chop`).
   - **Incremental Local Data Store (`src/storage.rs`):**
     - Implement a local persistent store using **SQLite** (`rusqlite` with bundled SQLite, stored in `./data/market_cache.db` and gitignored) with tables for `instruments`, `daily_bars` (OHLCV + adjusted close), `dividend_events`, `portfolio_snapshots`, and `sync_watermarks`.
     - Implement **delta-sync / watermark logic**: before fetching data for any ticker, query `sync_watermarks` for the latest stored timestamp (`last_synced_date`) and request *only* the missing date range (`last_synced_date + 1 day` through `today`), upserting idempotently so re-runs do zero redundant work.
     - Include a deterministic mock/synthetic market data provider trait (`MarketDataSource`) with a realistic historical generator (spanning 2+ years of daily bars + quarterly dividends) so `cargo run` works offline out-of-the-box, while keeping the trait clean to plug into Yahoo Finance / Trading 212 read-only metadata endpoints later.
   - **Trend, Territory & Archetype Classifier (`src/classifier.rs`):**
     - Compute rolling **50-day / 200-day SMAs**, **14-day Wilder RSI**, **annualized volatility**, **max drawdown from 52-week high**, **CAGR**, and **trailing dividend yield / payout consistency**.
     - Classify each holding into its current **Trend Territory** (e.g., `Pullback` -5% to -10%, `Correction` -10% to -20%, `BearMarket` < -20%, above/below 200-SMA) and **Stock Archetype** (`Growth`, `Income`, `Cyclical`, `Defensive`) based on measurable quantitative rules (yield, earnings/price cyclicality, beta/volatility, revenue/price growth).
   - **Portfolio Analyzer & Recommendation Engine (`src/recommender.rs`):**
     - Ingest a sample portfolio config (mirroring my multi-currency UK/US/EU thematic mix: e.g., `NVDA`, `ASML`, `NG.`, `UKW`, `RIO`, `RKLB`, `MELI`) alongside current holdings weights.
     - Detect concentration imbalances (by `Currency`, `StockArchetype`, and `TrendTerritory`) and generate deterministic, explainable **read-only research recommendations** (e.g., *"Portfolio is 72% Growth / 8% Income; UKW and NG. are in Pullback territory below 50-SMA—candidate for income rebalancing"* or *"Zero Defensive/Baseload exposure detected"*).
   - **CLI Demo (`src/main.rs`):**
     - Demonstrate two consecutive runs in `main.rs` (or via subcommands `sync`, `analyze`, `recommend`):
       - **Run 1:** Cold cache -> fetches full 2-year history for sample tickers and writes to SQLite.
       - **Run 2:** Incremental sync -> proves only the newest delta days are fetched while historical rows are served from local cache.
       - Prints a clean, structured terminal report showing each ticker's Archetype, Current Territory, Drawdown %, Yield %, and Portfolio Gap Recommendations.

2. **Strict Compiler & Linter Guardrails ("Shift-Left")**
   - Configure `[lints.rust]` and `[lints.clippy]` in `Cargo.toml`:
     - Deny `unsafe_code`, `clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`, `clippy::todo`, `clippy::unimplemented`, and `clippy::float_cmp` in library code (allow `unwrap`/`expect` only inside `#[cfg(test)]`).
     - Enable `clippy::pedantic` and `clippy::nursery` with sensible overrides documented in `Cargo.toml`.
   - Add a `rustfmt.toml` and `rust-toolchain.toml` pinning stable Rust + `rustfmt` + `clippy`.

3. **Supply-Chain & Vulnerability Scanning**
   - Create a `deny.toml` configured for `cargo-deny` (checking RustSec advisories/CVEs, license allowlist: MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Unicode-3.0, Zlib, and duplicate crate bans).
   - Check if `cargo-audit` and `cargo-deny` are installed locally; run whichever are available and document installation commands in `README.md` for any missing tools.

4. **Test Automation & Benchmarks**
   - Write comprehensive unit tests (`#[cfg(test)]`) covering:
     - Incremental watermark sync (verifying Run 2 requests *zero* already-cached dates and handles weekend/holiday gaps cleanly).
     - Archetype classification (`Income` vs. `Growth` vs. `Cyclical`) and `TrendTerritory` drawdown boundaries.
     - Edge cases (empty bar history, zero-dividend stocks, flat price series).
   - Add property-based tests using `proptest` verifying invariants (e.g., RSI always within `[0.0, 100.0]`, drawdown in `[0.0, 100.0]`, portfolio archetype weights always sum to `100%`, idempotent SQLite upserts never duplicate bars).
   - Add an integration test in `tests/` testing the full end-to-end SQLite incremental sync + analysis pipeline in a temporary directory.

5. **Pre-Commit Hooks & CI Pipeline**
   - Create a `.pre-commit-config.yaml` running:
     - `cargo fmt --all -- --check`
     - `cargo clippy --all-targets --all-features -- -D warnings`
     - `cargo test --lib`
     - Secret/credential leak detection (`gitleaks` or `detect-private-key`) and blocking `.env` or `*.db` files from ever being committed.
   - Add `.gitignore` excluding `target/`, `data/`, `*.db`, `*.sqlite*`, and `.env*`.
   - Install git hooks if `pre-commit` is available locally.
   - Create `.github/workflows/ci.yml` running format check, strict clippy, full test suite, `cargo audit`, and `cargo deny check`.

6. **Agent Guardrails (`CLAUDE.md` & `justfile`)**
   - Create a concise `CLAUDE.md` codifying strict rules for future Claude Code sessions:
     - **Read-Only Invariant:** Never add order placement, broker write endpoints, or trade execution logic to this repository.
     - **Zero-Panic Code:** No `unwrap()`/`expect()`/`panic!()` in `src/`.
     - **Data Integrity:** All new market data ingestors must use transactional watermark upserts and have `proptest` invariant coverage.
     - Always run `just check` before concluding any task.
   - Create a `justfile` (and fallback `Makefile`) with targets: `fmt`, `lint`, `test`, `audit`, `demo`, and `check`.

Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo run` to verify everything compiles, caches incrementally, and passes all checks cleanly, then summarize the repository structure and any optional tools I should install globally.
