//! Incremental local market data cache backed by SQLite.
//!
//! Every sync is **watermark driven**: before touching a [`MarketDataSource`]
//! the store looks up `sync_watermarks.last_synced_date` for the ticker and only
//! requests `last_synced_date + 1 ..= today`. All writes for one ticker happen
//! inside a single transaction using idempotent `ON CONFLICT DO UPDATE`
//! upserts, so re-running a sync never duplicates rows and a crash mid-sync
//! never advances the watermark past data that was not committed.

use std::path::Path;
use std::str::FromStr;

use chrono::{Days, NaiveDate};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use rust_decimal::Decimal;

use crate::domain::{Currency, DailyBar, DividendEvent, Instrument};
use crate::error::{Error, Result};
use crate::source::MarketDataSource;

/// Default backfill window for a cold cache (≈ 2 years).
pub const DEFAULT_HISTORY_DAYS: u64 = 730;

/// Default on-disk location of the cache (gitignored).
pub const DEFAULT_DB_PATH: &str = "data/market_cache.db";

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS instruments (
    ticker      TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    currency    TEXT NOT NULL CHECK (currency IN ('GBP','USD','EUR'))
);
CREATE TABLE IF NOT EXISTS daily_bars (
    ticker      TEXT NOT NULL REFERENCES instruments(ticker),
    date        TEXT NOT NULL,
    open        TEXT NOT NULL,
    high        TEXT NOT NULL,
    low         TEXT NOT NULL,
    close       TEXT NOT NULL,
    adj_close   TEXT NOT NULL,
    volume      INTEGER NOT NULL,
    PRIMARY KEY (ticker, date)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS dividend_events (
    ticker      TEXT NOT NULL REFERENCES instruments(ticker),
    ex_date     TEXT NOT NULL,
    amount      TEXT NOT NULL,
    PRIMARY KEY (ticker, ex_date)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS portfolio_snapshots (
    snapshot_date TEXT NOT NULL,
    ticker        TEXT NOT NULL REFERENCES instruments(ticker),
    quantity      TEXT NOT NULL,
    close         TEXT NOT NULL,
    currency      TEXT NOT NULL,
    value_gbp     TEXT NOT NULL,
    weight_pct    REAL NOT NULL,
    PRIMARY KEY (snapshot_date, ticker)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS sync_watermarks (
    ticker           TEXT PRIMARY KEY REFERENCES instruments(ticker),
    last_synced_date TEXT NOT NULL,
    last_bar_date    TEXT,
    source           TEXT NOT NULL
);
";

/// Outcome of syncing one ticker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// Ticker synced.
    pub ticker: String,
    /// Inclusive date range requested from the source; `None` = cache was current.
    pub requested: Option<(NaiveDate, NaiveDate)>,
    /// Bars returned by the source and upserted.
    pub bars_fetched: usize,
    /// Dividends returned by the source and upserted.
    pub dividends_fetched: usize,
    /// Total bars held locally after the sync.
    pub cached_bars: usize,
    /// `true` once the cache for this ticker is complete through `today`.
    pub complete: bool,
}

/// One row of a persisted portfolio snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotRow {
    /// Ticker.
    pub ticker: String,
    /// Shares held.
    pub quantity: Decimal,
    /// Close price used for valuation.
    pub close: Decimal,
    /// Instrument currency.
    pub currency: Currency,
    /// Value converted to GBP.
    pub value_gbp: Decimal,
    /// Portfolio weight in percent.
    pub weight_pct: f64,
}

/// SQLite-backed local cache.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the cache at `path`.
    ///
    /// # Errors
    /// Fails when the directory cannot be created or SQLite cannot open the file.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    /// Open a transient in-memory cache (tests, Python notebooks).
    ///
    /// # Errors
    /// Fails if SQLite cannot initialise the schema.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        // A background backfill may hold a write lock; wait rather than fail.
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Date through which `ticker` is fully synced, if ever synced.
    ///
    /// # Errors
    /// Fails on SQLite errors or a corrupt date value.
    pub fn watermark(&self, ticker: &str) -> Result<Option<NaiveDate>> {
        self.conn
            .query_row(
                "SELECT last_synced_date FROM sync_watermarks WHERE ticker = ?1",
                [ticker],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|s| parse_date(&s))
            .transpose()
    }

    /// The next uncached date range for `ticker`, capped at `max_days` days.
    ///
    /// Cold cache: starts at `today - history_days`. Warm cache: starts at
    /// `watermark + 1`. Returns `None` when the cache is current.
    ///
    /// # Errors
    /// Fails on SQLite errors or date overflow.
    pub fn next_window(
        &self,
        ticker: &str,
        today: NaiveDate,
        history_days: u64,
        max_days: u64,
    ) -> Result<Option<(NaiveDate, NaiveDate)>> {
        let from = match self.watermark(ticker)? {
            Some(wm) => wm.checked_add_days(Days::new(1)),
            None => today.checked_sub_days(Days::new(history_days)),
        }
        .ok_or_else(|| Error::InvalidInput("date out of range".into()))?;
        if from > today {
            return Ok(None);
        }
        let to = from
            .checked_add_days(Days::new(max_days.saturating_sub(1)))
            .map_or(today, |end| end.min(today));
        Ok(Some((from, to)))
    }

    /// Incrementally sync one instrument up to and including `today`.
    ///
    /// Cold cache: fetches `today - history_days ..= today`.
    /// Warm cache: fetches only `watermark + 1 ..= today` (possibly nothing).
    ///
    /// # Errors
    /// Fails if the source fails or the transaction cannot be committed; in
    /// that case the watermark is left untouched.
    pub fn sync_ticker(
        &mut self,
        source: &dyn MarketDataSource,
        instrument: &Instrument,
        today: NaiveDate,
        history_days: u64,
    ) -> Result<SyncReport> {
        self.sync_ticker_window(source, instrument, today, history_days, u64::MAX)
    }

    /// Like [`Self::sync_ticker`] but fetches at most `max_days` calendar days
    /// per call, advancing the watermark to the end of that chunk. Repeated
    /// calls walk forward chunk by chunk until the cache reaches `today`, so an
    /// interrupted backfill resumes exactly where it stopped.
    ///
    /// # Errors
    /// Fails if the source fails or the transaction cannot be committed; in
    /// that case the watermark is left untouched.
    pub fn sync_ticker_window(
        &mut self,
        source: &dyn MarketDataSource,
        instrument: &Instrument,
        today: NaiveDate,
        history_days: u64,
        max_days: u64,
    ) -> Result<SyncReport> {
        let Some((from, to)) =
            self.next_window(&instrument.ticker, today, history_days, max_days)?
        else {
            return Ok(SyncReport {
                ticker: instrument.ticker.clone(),
                requested: None,
                bars_fetched: 0,
                dividends_fetched: 0,
                cached_bars: self.bar_count(&instrument.ticker)?,
                complete: true,
            });
        };

        // Defensive: never trust a provider to honour the requested range.
        let in_range = |d: NaiveDate| d >= from && d <= to;
        let bars: Vec<_> = source
            .fetch_bars(instrument, from, to)?
            .into_iter()
            .filter(|b| b.ticker == instrument.ticker && in_range(b.date))
            .collect();
        let dividends: Vec<_> = source
            .fetch_dividends(instrument, from, to)?
            .into_iter()
            .filter(|d| d.ticker == instrument.ticker && in_range(d.ex_date))
            .collect();

        let tx = self.conn.transaction()?;
        upsert_instrument(&tx, instrument)?;
        upsert_bars(&tx, &bars)?;
        upsert_dividends(&tx, &dividends)?;
        let last_bar = bars.iter().map(|b| b.date).max();
        tx.execute(
            "INSERT INTO sync_watermarks (ticker, last_synced_date, last_bar_date, source)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(ticker) DO UPDATE SET
                -- Monotonic: a slower concurrent writer can never move it back.
                -- (ISO-8601 dates compare correctly as text.)
                last_synced_date = MAX(sync_watermarks.last_synced_date, excluded.last_synced_date),
                last_bar_date    = MAX(COALESCE(excluded.last_bar_date, ''),
                                       COALESCE(sync_watermarks.last_bar_date, '')),
                source           = excluded.source",
            params![
                instrument.ticker,
                to.to_string(),
                last_bar.map(|d| d.to_string()),
                source.name()
            ],
        )?;
        tx.commit()?;

        Ok(SyncReport {
            ticker: instrument.ticker.clone(),
            requested: Some((from, to)),
            bars_fetched: bars.len(),
            dividends_fetched: dividends.len(),
            cached_bars: self.bar_count(&instrument.ticker)?,
            complete: to >= today,
        })
    }

    /// Upsert bars directly (idempotent). Exposed for custom ingestors and tests.
    ///
    /// # Errors
    /// Fails on SQLite errors.
    pub fn upsert_bars(&mut self, instrument: &Instrument, bars: &[DailyBar]) -> Result<()> {
        let tx = self.conn.transaction()?;
        upsert_instrument(&tx, instrument)?;
        upsert_bars(&tx, bars)?;
        tx.commit()?;
        Ok(())
    }

    /// Number of cached bars for `ticker`.
    ///
    /// # Errors
    /// Fails on SQLite errors.
    pub fn bar_count(&self, ticker: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM daily_bars WHERE ticker = ?1",
            [ticker],
            |r| r.get(0),
        )?;
        usize::try_from(n).map_err(|e| Error::InvalidData(e.to_string()))
    }

    /// All cached bars for `ticker`, oldest first.
    ///
    /// # Errors
    /// Fails on SQLite errors or corrupt values.
    pub fn bars(&self, ticker: &str) -> Result<Vec<DailyBar>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT date, open, high, low, close, adj_close, volume
             FROM daily_bars WHERE ticker = ?1 ORDER BY date",
        )?;
        let rows = stmt.query_map([ticker], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        rows.map(|row| {
            let (date, open, high, low, close, adj, volume) = row?;
            Ok(DailyBar {
                ticker: ticker.to_owned(),
                date: parse_date(&date)?,
                open: parse_dec(&open)?,
                high: parse_dec(&high)?,
                low: parse_dec(&low)?,
                close: parse_dec(&close)?,
                adj_close: parse_dec(&adj)?,
                volume: u64::try_from(volume).map_err(|e| Error::InvalidData(e.to_string()))?,
            })
        })
        .collect()
    }

    /// All cached dividends for `ticker`, oldest first.
    ///
    /// # Errors
    /// Fails on SQLite errors or corrupt values.
    pub fn dividends(&self, ticker: &str) -> Result<Vec<DividendEvent>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ex_date, amount FROM dividend_events WHERE ticker = ?1 ORDER BY ex_date",
        )?;
        let rows = stmt.query_map([ticker], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (date, amount) = row?;
            Ok(DividendEvent {
                ticker: ticker.to_owned(),
                ex_date: parse_date(&date)?,
                amount: parse_dec(&amount)?,
            })
        })
        .collect()
    }

    /// Persist a portfolio snapshot (idempotent per date + ticker).
    ///
    /// # Errors
    /// Fails on SQLite errors.
    pub fn write_snapshot(&mut self, date: NaiveDate, rows: &[SnapshotRow]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO portfolio_snapshots
                    (snapshot_date, ticker, quantity, close, currency, value_gbp, weight_pct)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(snapshot_date, ticker) DO UPDATE SET
                    quantity = excluded.quantity, close = excluded.close,
                    currency = excluded.currency, value_gbp = excluded.value_gbp,
                    weight_pct = excluded.weight_pct",
            )?;
            for r in rows {
                stmt.execute(params![
                    date.to_string(),
                    r.ticker,
                    r.quantity.to_string(),
                    r.close.to_string(),
                    r.currency.code(),
                    r.value_gbp.to_string(),
                    r.weight_pct
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Number of snapshot rows stored for `date`.
    ///
    /// # Errors
    /// Fails on SQLite errors.
    pub fn snapshot_len(&self, date: NaiveDate) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM portfolio_snapshots WHERE snapshot_date = ?1",
            [date.to_string()],
            |r| r.get(0),
        )?;
        usize::try_from(n).map_err(|e| Error::InvalidData(e.to_string()))
    }
}

fn upsert_instrument(tx: &Transaction<'_>, i: &Instrument) -> Result<()> {
    tx.execute(
        "INSERT INTO instruments (ticker, name, currency) VALUES (?1, ?2, ?3)
         ON CONFLICT(ticker) DO UPDATE SET name = excluded.name, currency = excluded.currency",
        params![i.ticker, i.name, i.currency.code()],
    )?;
    Ok(())
}

fn upsert_bars(tx: &Transaction<'_>, bars: &[DailyBar]) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "INSERT INTO daily_bars (ticker, date, open, high, low, close, adj_close, volume)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(ticker, date) DO UPDATE SET
            open = excluded.open, high = excluded.high, low = excluded.low,
            close = excluded.close, adj_close = excluded.adj_close, volume = excluded.volume",
    )?;
    for b in bars {
        let volume = i64::try_from(b.volume).map_err(|e| Error::InvalidData(e.to_string()))?;
        stmt.execute(params![
            b.ticker,
            b.date.to_string(),
            b.open.to_string(),
            b.high.to_string(),
            b.low.to_string(),
            b.close.to_string(),
            b.adj_close.to_string(),
            volume
        ])?;
    }
    Ok(())
}

fn upsert_dividends(tx: &Transaction<'_>, divs: &[DividendEvent]) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "INSERT INTO dividend_events (ticker, ex_date, amount) VALUES (?1, ?2, ?3)
         ON CONFLICT(ticker, ex_date) DO UPDATE SET amount = excluded.amount",
    )?;
    for d in divs {
        stmt.execute(params![
            d.ticker,
            d.ex_date.to_string(),
            d.amount.to_string()
        ])?;
    }
    Ok(())
}

fn parse_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::from_str(s).map_err(|e| Error::InvalidData(format!("bad date {s:?}: {e}")))
}

fn parse_dec(s: &str) -> Result<Decimal> {
    Decimal::from_str(s).map_err(|e| Error::InvalidData(format!("bad decimal {s:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use proptest::prelude::*;

    use super::*;
    use crate::source::{SyntheticSource, is_trading_day};

    /// Wraps a source and records every requested range.
    struct Recording<'a> {
        inner: &'a SyntheticSource,
        calls: RefCell<Vec<(NaiveDate, NaiveDate)>>,
    }

    impl MarketDataSource for Recording<'_> {
        fn name(&self) -> &'static str {
            "recording"
        }
        fn fetch_bars(&self, i: &Instrument, f: NaiveDate, t: NaiveDate) -> Result<Vec<DailyBar>> {
            self.calls.borrow_mut().push((f, t));
            self.inner.fetch_bars(i, f, t)
        }
        fn fetch_dividends(
            &self,
            i: &Instrument,
            f: NaiveDate,
            t: NaiveDate,
        ) -> Result<Vec<DividendEvent>> {
            self.inner.fetch_dividends(i, f, t)
        }
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn inst() -> Instrument {
        Instrument {
            ticker: "UKW".into(),
            name: "Greencoat UK Wind".into(),
            currency: Currency::Gbp,
        }
    }

    #[test]
    fn cold_sync_backfills_history_and_sets_watermark() {
        let src = SyntheticSource::new();
        let mut store = Store::open_in_memory().unwrap();
        let today = d(2025, 6, 13);
        let r = store
            .sync_ticker(&src, &inst(), today, DEFAULT_HISTORY_DAYS)
            .unwrap();
        assert_eq!(r.requested, Some((d(2023, 6, 14), today)));
        assert!(r.bars_fetched > 500, "{r:?}");
        assert_eq!(r.cached_bars, r.bars_fetched);
        assert_eq!(store.watermark("UKW").unwrap(), Some(today));
    }

    #[test]
    fn second_run_same_day_requests_nothing() {
        let inner = SyntheticSource::new();
        let src = Recording {
            inner: &inner,
            calls: RefCell::new(vec![]),
        };
        let mut store = Store::open_in_memory().unwrap();
        let today = d(2025, 6, 13);
        store
            .sync_ticker(&src, &inst(), today, DEFAULT_HISTORY_DAYS)
            .unwrap();
        let before = store.bar_count("UKW").unwrap();
        let r2 = store
            .sync_ticker(&src, &inst(), today, DEFAULT_HISTORY_DAYS)
            .unwrap();
        assert_eq!(r2.requested, None);
        assert_eq!(r2.bars_fetched, 0);
        assert_eq!(
            src.calls.borrow().len(),
            1,
            "source must not be called again"
        );
        assert_eq!(store.bar_count("UKW").unwrap(), before);
    }

    #[test]
    fn incremental_run_requests_only_uncached_dates() {
        let inner = SyntheticSource::new();
        let src = Recording {
            inner: &inner,
            calls: RefCell::new(vec![]),
        };
        let mut store = Store::open_in_memory().unwrap();
        store
            .sync_ticker(&src, &inst(), d(2025, 6, 10), DEFAULT_HISTORY_DAYS)
            .unwrap();
        let cached: Vec<_> = store
            .bars("UKW")
            .unwrap()
            .into_iter()
            .map(|b| b.date)
            .collect();
        let r = store
            .sync_ticker(&src, &inst(), d(2025, 6, 13), DEFAULT_HISTORY_DAYS)
            .unwrap();
        let (from, to) = r.requested.unwrap();
        assert_eq!((from, to), (d(2025, 6, 11), d(2025, 6, 13)));
        assert!(
            cached.iter().all(|c| *c < from),
            "re-requested a cached date"
        );
        assert_eq!(r.bars_fetched, 3);
    }

    #[test]
    fn weekend_gap_is_handled_without_refetch() {
        let inner = SyntheticSource::new();
        let src = Recording {
            inner: &inner,
            calls: RefCell::new(vec![]),
        };
        let mut store = Store::open_in_memory().unwrap();
        // Friday sync, then Sunday: a request is made but yields zero bars and
        // still advances the watermark so Monday only asks for Monday.
        store
            .sync_ticker(&src, &inst(), d(2025, 6, 13), DEFAULT_HISTORY_DAYS)
            .unwrap();
        let sun = store
            .sync_ticker(&src, &inst(), d(2025, 6, 15), DEFAULT_HISTORY_DAYS)
            .unwrap();
        assert_eq!(sun.requested, Some((d(2025, 6, 14), d(2025, 6, 15))));
        assert_eq!(sun.bars_fetched, 0);
        assert_eq!(store.watermark("UKW").unwrap(), Some(d(2025, 6, 15)));
        let mon = store
            .sync_ticker(&src, &inst(), d(2025, 6, 16), DEFAULT_HISTORY_DAYS)
            .unwrap();
        assert_eq!(mon.requested, Some((d(2025, 6, 16), d(2025, 6, 16))));
        assert_eq!(mon.bars_fetched, 1);
    }

    #[test]
    fn holiday_gap_produces_no_bar() {
        let src = SyntheticSource::new();
        let mut store = Store::open_in_memory().unwrap();
        store
            .sync_ticker(&src, &inst(), d(2024, 12, 24), DEFAULT_HISTORY_DAYS)
            .unwrap();
        let r = store
            .sync_ticker(&src, &inst(), d(2024, 12, 27), DEFAULT_HISTORY_DAYS)
            .unwrap();
        assert_eq!(r.bars_fetched, 1); // 25th + 26th are holidays
        assert!(
            store
                .bars("UKW")
                .unwrap()
                .iter()
                .all(|b| is_trading_day(b.date))
        );
    }

    #[test]
    fn decimal_roundtrip_is_lossless() {
        let src = SyntheticSource::new();
        let mut store = Store::open_in_memory().unwrap();
        let today = d(2025, 3, 31);
        store.sync_ticker(&src, &inst(), today, 60).unwrap();
        let expected = src.fetch_bars(&inst(), d(2025, 1, 30), today).unwrap();
        assert_eq!(store.bars("UKW").unwrap(), expected);
    }

    #[test]
    fn snapshot_upsert_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        store.upsert_bars(&inst(), &[]).unwrap();
        let row = SnapshotRow {
            ticker: "UKW".into(),
            quantity: Decimal::new(100, 0),
            close: Decimal::new(150, 2),
            currency: Currency::Gbp,
            value_gbp: Decimal::new(150, 0),
            weight_pct: 100.0,
        };
        store
            .write_snapshot(d(2025, 1, 2), std::slice::from_ref(&row))
            .unwrap();
        store.write_snapshot(d(2025, 1, 2), &[row]).unwrap();
        assert_eq!(store.snapshot_len(d(2025, 1, 2)).unwrap(), 1);
    }

    #[test]
    fn chunked_sync_walks_forward_without_gaps_or_overlap() {
        let src = SyntheticSource::new();
        let today = d(2025, 6, 13);
        let mut chunked = Store::open_in_memory().unwrap();
        let mut windows = vec![];
        loop {
            let r = chunked
                .sync_ticker_window(&src, &inst(), today, DEFAULT_HISTORY_DAYS, 90)
                .unwrap();
            let Some((from, to)) = r.requested else { break };
            assert!((to - from).num_days() < 90);
            assert_eq!(chunked.watermark("UKW").unwrap(), Some(to));
            windows.push((from, to));
            assert_eq!(r.complete, to == today);
        }
        assert_eq!(windows.first().unwrap().0, d(2023, 6, 14));
        assert_eq!(windows.last().unwrap().1, today);
        assert_eq!(windows.len(), 9); // ceil(731 / 90)
        for w in windows.windows(2) {
            assert_eq!(w[0].1 + Days::new(1), w[1].0, "gap or overlap");
        }
        let mut oneshot = Store::open_in_memory().unwrap();
        oneshot
            .sync_ticker(&src, &inst(), today, DEFAULT_HISTORY_DAYS)
            .unwrap();
        assert_eq!(chunked.bars("UKW").unwrap(), oneshot.bars("UKW").unwrap());
        assert_eq!(
            chunked.dividends("UKW").unwrap(),
            oneshot.dividends("UKW").unwrap()
        );
    }

    /// While connection A is mid-fetch, connection B (another process) syncs
    /// further ahead. A's later commit must not move the watermark backwards.
    #[test]
    fn concurrent_writer_cannot_regress_watermark() {
        struct Racing<'a> {
            inner: &'a SyntheticSource,
            path: std::path::PathBuf,
            raced: std::cell::Cell<bool>,
        }
        impl MarketDataSource for Racing<'_> {
            fn name(&self) -> &'static str {
                "racing"
            }
            fn fetch_bars(
                &self,
                i: &Instrument,
                f: NaiveDate,
                t: NaiveDate,
            ) -> Result<Vec<DailyBar>> {
                if !self.raced.replace(true) {
                    let mut other = Store::open(&self.path).unwrap();
                    while other
                        .sync_ticker_window(
                            self.inner,
                            i,
                            d(2025, 6, 13),
                            DEFAULT_HISTORY_DAYS,
                            90,
                        )?
                        .requested
                        .is_some()
                    {}
                }
                self.inner.fetch_bars(i, f, t)
            }
            fn fetch_dividends(
                &self,
                i: &Instrument,
                f: NaiveDate,
                t: NaiveDate,
            ) -> Result<Vec<DividendEvent>> {
                self.inner.fetch_dividends(i, f, t)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.db");
        let inner = SyntheticSource::new();
        let racing = Racing {
            inner: &inner,
            path: path.clone(),
            raced: std::cell::Cell::new(false),
        };
        let mut a = Store::open(&path).unwrap();
        let r = a
            .sync_ticker_window(&racing, &inst(), d(2025, 6, 13), DEFAULT_HISTORY_DAYS, 90)
            .unwrap();
        assert_eq!(r.requested.unwrap().1, d(2023, 9, 11)); // A wrote only its first chunk...
        assert_eq!(a.watermark("UKW").unwrap(), Some(d(2025, 6, 13))); // ...but B's progress stands.
        let mut oneshot = Store::open_in_memory().unwrap();
        oneshot
            .sync_ticker(&inner, &inst(), d(2025, 6, 13), DEFAULT_HISTORY_DAYS)
            .unwrap();
        assert_eq!(a.bars("UKW").unwrap(), oneshot.bars("UKW").unwrap());
    }

    proptest! {
        /// Upserting arbitrary (overlapping, repeated) batches never duplicates bars.
        #[test]
        fn upserts_never_duplicate(offsets in prop::collection::vec(0_u64..60, 1..80), repeats in 1_usize..4) {
            let mut store = Store::open_in_memory().unwrap();
            let base = d(2024, 1, 1);
            let bars: Vec<DailyBar> = offsets.iter().map(|o| DailyBar {
                ticker: "UKW".into(),
                date: base + Days::new(*o),
                open: Decimal::ONE, high: Decimal::TWO, low: Decimal::ONE,
                close: Decimal::ONE, adj_close: Decimal::ONE, volume: *o,
            }).collect();
            for _ in 0..repeats {
                store.upsert_bars(&inst(), &bars).unwrap();
            }
            let mut unique = offsets;
            unique.sort_unstable();
            unique.dedup();
            prop_assert_eq!(store.bar_count("UKW").unwrap(), unique.len());
        }

    }

    proptest! {
        // Each case simulates years of bars; fewer cases keep `cargo test` fast.
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Any sequence of sync dates never re-requests a cached date.
        #[test]
        fn sync_sequence_never_overlaps(steps in prop::collection::vec(0_u64..10, 1..8)) {
            let inner = SyntheticSource::new();
            let src = Recording { inner: &inner, calls: RefCell::new(vec![]) };
            let mut store = Store::open_in_memory().unwrap();
            let mut today = d(2025, 1, 6);
            for s in steps {
                today = today + Days::new(s);
                store.sync_ticker(&src, &inst(), today, 30).unwrap();
            }
            let calls = src.calls.borrow();
            for w in calls.windows(2) {
                prop_assert!(w[1].0 > w[0].1, "overlapping requests {:?}", w);
            }
        }
    }
}
