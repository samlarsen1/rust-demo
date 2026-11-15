# rust_portfolio_intel

Strictly **read-only** quantitative portfolio intelligence in Rust, with optional
Python (PyO3) bindings. It incrementally caches market data in SQLite, classifies
each holding's **trend territory** and **style archetype**, and produces
explainable **portfolio gap research notes** for a multi-currency (GBP/USD/EUR)
portfolio.

> It has **no** order-execution, trade-placement or broker-mutation code, and
> must never get any (see `CLAUDE.md`).

## Quick start

```bash
cargo run                 # demo: throttled cold backfill → incremental sync → same-day re-run → report
cargo run -- sync         # bring ./data/market_cache.db up to date (starts a background backfill if cold)
cargo run -- status       # per-ticker coverage + running backfill progress
cargo run -- analyze      # classify holdings from the local cache only
cargo run -- recommend    # research notes from the local cache only
cargo run -- sync --as-of 2026-01-15 --db /tmp/other.db
```

### Slow first run (rate-limit friendly)

A cold cache needs about 2 years of history per ticker. Fetching that in one
burst is how API clients get throttled or blocked, so the first `sync` doesn't
do it inline. Instead it **starts a background backfill process** and returns
straight away:

```text
$ cargo run -- sync
Cache needs 63 chunks of history (~2m06s at 1000 ms/request) — too much to fetch in one burst.
Started background backfill (pid 25498): 63 chunks, eta ~2m06s.
  progress : rust_portfolio_intel status   (or: tail -f data/market_cache.db.backfill.log)
  stop     : kill 25498   (resumes from the last chunk on the next `sync`)
```

How the backfill behaves:

- **Chunked:** at most `--chunk-days` (default 90) per request.
- **Throttled:** at least `--interval-ms` (default 1000) between requests. A
  `RateLimited` response (HTTP 429) is retried with exponential backoff, and a
  server `Retry-After` is honoured.
- **Round-robin:** one chunk per ticker per round, so every holding gets data
  early and no single symbol is hammered.
- **Resumable:** each chunk commits in the same transaction as its watermark.
  Killing the process loses nothing, and the next `sync` or `backfill` carries on
  from there.
- **Single instance:** a PID lock (`<db>.backfill.lock`) stops two backfills
  doubling the request rate. A lock left by a killed process is detected and
  replaced.

Other ways to run it:

```bash
cargo run -- sync --wait                    # run the needed backfill in the foreground
cargo run -- backfill --interval-ms 2000    # foreground, slower (Ctrl-C safe)
cargo run -- backfill --background          # detach explicitly
```

While a backfill is running, `analyze` and `recommend` refuse to run on the
incomplete history. Once the cache is warm, `sync` fetches only the few missing
days inline (still throttled).

The demo runs offline using a deterministic synthetic market (`SyntheticSource`).
Each date's bar is identical whichever range you request, so incremental syncs
stay consistent.

## Architecture

| Module | Responsibility |
|---|---|
| `domain.rs` | `Currency`, `StockArchetype`, `TrendTerritory`, `Instrument`, `DailyBar`, `DividendEvent`, `Holding`; money is always `Decimal` |
| `source.rs` | `MarketDataSource` trait (fetch-only) + `SyntheticSource` generator (drift + business cycle + mean-reverting noise, quarterly dividends, weekend/holiday calendar) |
| `storage.rs` | SQLite cache (`instruments`, `daily_bars`, `dividend_events`, `portfolio_snapshots`, `sync_watermarks`) with watermark-driven delta sync and transactional idempotent upserts |
| `classifier.rs` | 50/200 SMA, 14-day Wilder RSI, annualised volatility, drawdown from 52-week high, CAGR, trailing yield, payout consistency, cyclicality → territory and archetype |
| `recommender.rs` | GBP valuation (static FX), exposure by currency/archetype/territory, rule-based research notes |
| `throttle.rs` | `ThrottledSource`: min request interval, exponential backoff on `RateLimited`, injectable `Clock` |
| `backfill.rs` | Chunked, round-robin, resumable backfill planner/runner with progress + ETA |
| `pipeline.rs` | sync → classify → value → recommend |
| `python.rs` | PyO3 module (feature `extension-module`) |

### Incremental sync

```
watermark = SELECT last_synced_date FROM sync_watermarks WHERE ticker = ?
from      = watermark + 1 day      (or today − 730 days on a cold cache)
to        = min(today, from + chunk_days − 1)
if from > today → no source call at all
else fetch [from, to] → BEGIN; upsert bars/dividends; watermark = MAX(watermark, to); COMMIT
```

The watermark only ever moves forward, so a slower concurrent writer can't push
it back.

The watermark records the date the cache is complete *through*, not the last bar
date. A sync on a weekend or holiday asks for that gap once, gets zero bars, and
moves on without asking again.

### Classification rules

**Territory** (drawdown from the 52-week high, lower bounds inclusive): ≥ 20% `BearMarket`,
≥ 10% `Correction`, ≥ 5% `Pullback`. Otherwise it is `Uptrend` when close > SMA200 and
SMA50 > SMA200, and `Chop` when not.

**Archetype** (checked in order): `Income` (yield ≥ 4%, paid in ≥ 75% of quarters, vol < 25%) →
`Growth` (CAGR ≥ 15%, yield < 2%) → `Cyclical` (6-month-return σ ≥ 0.15, vol ≥ 22%) →
`Defensive` (vol < 20%) → fallback by yield/CAGR. The thresholds are `pub const`s in
`classifier.rs`.

## Python

```bash
pip install maturin
maturin develop --release          # builds with --features extension-module
python -c "import rust_portfolio_intel as r; print(r.analyze_sample_portfolio('data/market_cache.db')['recommendations'])"
```

`analyze_sample_portfolio(db_path, as_of=None, interval_ms=1000, chunk_days=90)` uses the same
throttled, chunked backfill, so a cold call blocks for a couple of minutes instead of sending
everything in one burst.

## Quality gates

| Gate | Command |
|---|---|
| Format | `cargo fmt --all -- --check` |
| Lints (pedantic + nursery, zero-panic, `unsafe` denied) | `cargo clippy --all-targets --all-features -- -D warnings` |
| Tests (unit + proptest + integration) | `cargo test --all-targets` |
| Advisories | `cargo audit` |
| Licenses / bans / sources / advisories | `cargo deny check` |
| Everything | `just check` (or `make check`) |

Pre-commit hooks (`pre-commit install`) run fmt, clippy and `cargo test --lib`.
They also run `detect-private-key` and block `.env*` and `*.db`/`*.sqlite*` files.
CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests, the demo, `cargo audit` and `cargo deny`.

## Optional tools

```bash
cargo install --locked cargo-audit    # RustSec advisory scan
cargo install --locked cargo-deny     # licenses, bans, duplicate crates, advisories
cargo install --locked just           # task runner (Makefile is the fallback)
pip install pre-commit maturin        # git hooks, Python wheel builds
# optional deeper secret scanning: https://github.com/gitleaks/gitleaks
```
