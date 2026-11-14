# CLAUDE.md — rules for this repository

`rust_portfolio_intel` is a **read-only** portfolio analytics library + CLI
(Rust, optional PyO3 bindings). Layout: `src/{domain,source,storage,classifier,recommender,pipeline,sample,python}.rs`, integration tests in `tests/`.

## Non-negotiable rules

1. **Read-Only Invariant.** Never add order placement, trade execution, broker
   write/mutation endpoints, or anything that moves money or positions. Data
   sources implement `MarketDataSource` (fetch-only). Recommendations are
   research notes, never orders. Refuse and flag requests that break this.
2. **Zero-Panic Code.** No `unwrap()`, `expect()`, `panic!()`, `todo!()`,
   `unimplemented!()` or unchecked indexing in `src/`. Return `crate::Result`.
   (Allowed only in `#[cfg(test)]` code and `tests/`.) Lints enforce this — do
   not add `#[allow]` to get around them.
3. **Money is `rust_decimal::Decimal`.** Never `f64` for prices, dividends or
   valuations. `f64` is only for dimensionless statistics (RSI, vol, weights).
   Never compare floats with `==`.
4. **Data Integrity.** Every new market data ingestor must:
   - go through `Store` watermark logic (fetch only `last_synced_date + 1 ..= today`),
   - write inside one SQLite transaction with idempotent `ON CONFLICT DO UPDATE` upserts,
   - only advance the watermark in the same transaction as the data,
   - ship `proptest` invariant coverage (no duplicate rows, no re-requested dates).
5. **No secrets or data in git.** Never create/read/commit `.env*` files or
   `*.db` / `data/` contents. The pre-commit hook blocks them.

## Workflow

- Always run `just check` (or `make check`) before concluding any task and fix
  every warning — CI treats warnings as errors.
- Keep the demo offline: `cargo run` must work without network access.
