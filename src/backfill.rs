//! Slow, polite, resumable history backfill.
//!
//! A cold cache needs ~2 years of history per ticker. Requesting it all at once
//! for every holding is exactly what gets API clients throttled or banned, so
//! the backfill instead:
//!
//! * splits each ticker's missing range into `chunk_days` windows,
//! * walks the portfolio **round-robin** (one chunk per ticker per round), so
//!   every holding gets data early and no single symbol is hammered,
//! * sends every request through a [`ThrottledSource`] (min interval + backoff),
//! * commits each chunk together with its watermark, so killing the process at
//!   any point loses nothing — the next run resumes from the watermark.

use std::time::Duration;

use chrono::NaiveDate;

use crate::domain::Portfolio;
use crate::error::Result;
use crate::source::MarketDataSource;
use crate::storage::{DEFAULT_HISTORY_DAYS, Store, SyncReport};
use crate::throttle::{Clock, ThrottleConfig, ThrottledSource};

/// Upstream requests made per chunk (bars + dividends).
pub const REQUESTS_PER_CHUNK: u64 = 2;

/// Backfill policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillConfig {
    /// Calendar days fetched per request.
    pub chunk_days: u64,
    /// History loaded for a cold ticker.
    pub history_days: u64,
    /// Request spacing and retry policy.
    pub throttle: ThrottleConfig,
}

impl Default for BackfillConfig {
    fn default() -> Self {
        Self {
            chunk_days: 90,
            history_days: DEFAULT_HISTORY_DAYS,
            throttle: ThrottleConfig::default(),
        }
    }
}

/// Remaining work for one ticker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickerPlan {
    /// Ticker.
    pub ticker: String,
    /// Date the cache is complete through (`None` = never synced).
    pub watermark: Option<NaiveDate>,
    /// Chunks still to fetch to reach `today`.
    pub remaining_chunks: u64,
}

/// Remaining work for the whole portfolio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillPlan {
    /// Per-ticker breakdown, in portfolio order.
    pub tickers: Vec<TickerPlan>,
}

impl BackfillPlan {
    /// Chunks left across all tickers.
    pub fn remaining_chunks(&self) -> u64 {
        self.tickers.iter().map(|t| t.remaining_chunks).sum()
    }

    /// `true` when every ticker is synced through `today`.
    pub fn is_complete(&self) -> bool {
        self.remaining_chunks() == 0
    }

    /// `true` when more than one chunk is outstanding for some ticker — i.e.
    /// the gap is too large to sync inline and should be backfilled slowly.
    pub fn needs_backfill(&self) -> bool {
        self.tickers.iter().any(|t| t.remaining_chunks > 1)
    }

    /// Lower-bound time to finish at `min_interval` spacing.
    pub fn eta(&self, min_interval: Duration) -> Duration {
        let requests = self.remaining_chunks().saturating_mul(REQUESTS_PER_CHUNK);
        min_interval.saturating_mul(u32::try_from(requests).unwrap_or(u32::MAX))
    }
}

/// Work out how many chunks each holding still needs.
///
/// # Errors
/// Fails on SQLite errors.
pub fn plan(
    store: &Store,
    portfolio: &Portfolio,
    today: NaiveDate,
    config: &BackfillConfig,
) -> Result<BackfillPlan> {
    let chunk = config.chunk_days.max(1);
    let tickers = portfolio
        .holdings
        .iter()
        .map(|h| {
            let ticker = h.instrument.ticker.clone();
            let window = store.next_window(&ticker, today, config.history_days, u64::MAX)?;
            let remaining_chunks = window.map_or(0, |(from, to)| {
                let days = u64::try_from((to - from).num_days() + 1).unwrap_or(0);
                days.div_ceil(chunk)
            });
            Ok(TickerPlan {
                watermark: store.watermark(&ticker)?,
                ticker,
                remaining_chunks,
            })
        })
        .collect::<Result<_>>()?;
    Ok(BackfillPlan { tickers })
}

/// Progress event emitted after every committed chunk.
#[derive(Debug)]
pub struct BackfillProgress<'a> {
    /// 1-based round number.
    pub round: u64,
    /// Chunks committed so far (this run).
    pub chunks_done: u64,
    /// Chunks committed + remaining (this run).
    pub chunks_total: u64,
    /// The chunk just committed.
    pub report: &'a SyncReport,
    /// Estimated time left at the configured request interval.
    pub eta: Duration,
}

/// Outcome of a backfill run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackfillSummary {
    /// Chunks committed.
    pub chunks: u64,
    /// Rounds over the portfolio.
    pub rounds: u64,
    /// Bars written.
    pub bars: usize,
    /// Upstream requests (including retries).
    pub requests: u64,
    /// Rate-limit retries.
    pub retries: u64,
    /// Time spent waiting on the throttle.
    pub waited: Duration,
}

/// Backfill every holding up to `today`, slowly and resumably.
///
/// # Errors
/// Stops at the first non-retryable source or storage error. Everything
/// committed before that point is kept; re-running resumes from there.
pub fn run_backfill<C: Clock>(
    store: &mut Store,
    source: &dyn MarketDataSource,
    portfolio: &Portfolio,
    today: NaiveDate,
    config: &BackfillConfig,
    clock: &C,
    on_progress: &mut dyn FnMut(&BackfillProgress<'_>),
) -> Result<BackfillSummary> {
    let throttled = ThrottledSource::new(source, clock, config.throttle);
    let chunks_total = plan(store, portfolio, today, config)?.remaining_chunks();
    let mut summary = BackfillSummary::default();
    let mut pending: Vec<_> = portfolio.holdings.iter().map(|h| &h.instrument).collect();

    while !pending.is_empty() {
        summary.rounds += 1;
        let mut still_pending = Vec::with_capacity(pending.len());
        for instrument in pending {
            let report = store.sync_ticker_window(
                &throttled,
                instrument,
                today,
                config.history_days,
                config.chunk_days.max(1),
            )?;
            if report.requested.is_some() {
                summary.chunks += 1;
                summary.bars += report.bars_fetched;
                let left = chunks_total.saturating_sub(summary.chunks);
                on_progress(&BackfillProgress {
                    round: summary.rounds,
                    chunks_done: summary.chunks,
                    chunks_total,
                    report: &report,
                    eta: config.throttle.min_interval.saturating_mul(
                        u32::try_from(left * REQUESTS_PER_CHUNK).unwrap_or(u32::MAX),
                    ),
                });
            }
            if !report.complete {
                still_pending.push(instrument);
            }
        }
        pending = still_pending;
    }

    let stats = throttled.stats();
    summary.requests = stats.requests;
    summary.retries = stats.retries;
    summary.waited = stats.waited;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use proptest::prelude::*;

    use super::*;
    use crate::domain::{DailyBar, DividendEvent, Instrument};
    use crate::error::Error;
    use crate::sample::{sample_portfolio, sample_source};
    use crate::source::SyntheticSource;
    use crate::throttle::ManualClock;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    const TODAY: NaiveDate = match NaiveDate::from_ymd_opt(2025, 6, 13) {
        Some(x) => x,
        None => NaiveDate::MIN,
    };

    fn cfg(chunk_days: u64) -> BackfillConfig {
        BackfillConfig {
            chunk_days,
            history_days: DEFAULT_HISTORY_DAYS,
            throttle: ThrottleConfig::default().with_interval(Duration::from_secs(2)),
        }
    }

    /// Records request timestamps; can fail hard or rate-limit on chosen calls.
    struct Probe<'a> {
        inner: &'a SyntheticSource,
        clock: &'a ManualClock,
        times: RefCell<Vec<(Duration, String)>>,
        hard_fail_at: Option<usize>,
        rate_limit_every: Option<usize>,
        calls: Cell<usize>,
    }

    impl<'a> Probe<'a> {
        fn new(inner: &'a SyntheticSource, clock: &'a ManualClock) -> Self {
            Self {
                inner,
                clock,
                times: RefCell::new(vec![]),
                hard_fail_at: None,
                rate_limit_every: None,
                calls: Cell::new(0),
            }
        }
        fn hit(&self, ticker: &str) -> Result<()> {
            let n = self.calls.get() + 1;
            self.calls.set(n);
            self.times
                .borrow_mut()
                .push((self.clock.now(), ticker.to_owned()));
            if self.hard_fail_at == Some(n) {
                return Err(Error::Source("connection reset".into()));
            }
            if self.rate_limit_every.is_some_and(|k| n.is_multiple_of(k)) {
                return Err(Error::RateLimited { retry_after: None });
            }
            Ok(())
        }
    }

    impl MarketDataSource for Probe<'_> {
        fn name(&self) -> &'static str {
            "probe"
        }
        fn fetch_bars(&self, i: &Instrument, f: NaiveDate, t: NaiveDate) -> Result<Vec<DailyBar>> {
            self.hit(&i.ticker)?;
            self.inner.fetch_bars(i, f, t)
        }
        fn fetch_dividends(
            &self,
            i: &Instrument,
            f: NaiveDate,
            t: NaiveDate,
        ) -> Result<Vec<DividendEvent>> {
            self.hit(&i.ticker)?;
            self.inner.fetch_dividends(i, f, t)
        }
    }

    fn oneshot_bars(src: &SyntheticSource) -> Vec<Vec<DailyBar>> {
        let mut store = Store::open_in_memory().unwrap();
        let p = sample_portfolio();
        crate::pipeline::sync_portfolio(&mut store, src, &p, TODAY, DEFAULT_HISTORY_DAYS).unwrap();
        p.holdings
            .iter()
            .map(|h| store.bars(&h.instrument.ticker).unwrap())
            .collect()
    }

    #[test]
    fn plan_counts_chunks() {
        let store = Store::open_in_memory().unwrap();
        let p = plan(&store, &sample_portfolio(), TODAY, &cfg(90)).unwrap();
        assert!(
            p.tickers
                .iter()
                .all(|t| t.remaining_chunks == 9 && t.watermark.is_none())
        );
        assert!(p.needs_backfill());
        assert_eq!(
            p.eta(Duration::from_secs(1)),
            Duration::from_secs(7 * 9 * 2)
        );
    }

    #[test]
    fn backfill_is_throttled_round_robin_and_complete() {
        let src = sample_source();
        let clock = ManualClock::new();
        let probe = Probe::new(&src, &clock);
        let mut store = Store::open_in_memory().unwrap();
        let portfolio = sample_portfolio();
        let mut events = vec![];
        let s = run_backfill(
            &mut store,
            &probe,
            &portfolio,
            TODAY,
            &cfg(90),
            &clock,
            &mut |e| {
                events.push((e.round, e.report.ticker.clone()));
            },
        )
        .unwrap();

        assert_eq!(s.chunks, 63);
        assert_eq!(s.rounds, 9);
        assert_eq!(s.requests, 126);
        // Never faster than the configured interval.
        let times = probe.times.borrow();
        for w in times.windows(2) {
            assert!(
                w[1].0.checked_sub(w[0].0).unwrap() >= Duration::from_secs(2),
                "{w:?}"
            );
        }
        // Round 1 touches every ticker once before any ticker gets a 2nd chunk.
        let round1: Vec<_> = events
            .iter()
            .filter(|e| e.0 == 1)
            .map(|e| e.1.clone())
            .collect();
        let tickers: Vec<_> = portfolio
            .holdings
            .iter()
            .map(|h| h.instrument.ticker.clone())
            .collect();
        assert_eq!(round1, tickers);
        // Same data as an unthrottled one-shot sync.
        let got: Vec<_> = tickers.iter().map(|t| store.bars(t).unwrap()).collect();
        assert_eq!(got, oneshot_bars(&src));
        assert!(
            plan(&store, &portfolio, TODAY, &cfg(90))
                .unwrap()
                .is_complete()
        );
    }

    #[test]
    fn interrupted_backfill_resumes_without_refetching() {
        let src = sample_source();
        let clock = ManualClock::new();
        let portfolio = sample_portfolio();
        let mut store = Store::open_in_memory().unwrap();

        let mut failing = Probe::new(&src, &clock);
        failing.hard_fail_at = Some(41);
        let err = run_backfill(
            &mut store,
            &failing,
            &portfolio,
            TODAY,
            &cfg(90),
            &clock,
            &mut |_| {},
        );
        assert!(matches!(err, Err(Error::Source(_))));
        let committed = plan(&store, &portfolio, TODAY, &cfg(90))
            .unwrap()
            .remaining_chunks();
        assert_eq!(
            committed,
            63 - 20,
            "20 full chunks committed before the failure"
        );

        let resumed = Probe::new(&src, &clock);
        let s = run_backfill(
            &mut store,
            &resumed,
            &portfolio,
            TODAY,
            &cfg(90),
            &clock,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(s.chunks, 43);
        let tickers: Vec<_> = portfolio
            .holdings
            .iter()
            .map(|h| h.instrument.ticker.clone())
            .collect();
        let got: Vec<_> = tickers.iter().map(|t| store.bars(t).unwrap()).collect();
        assert_eq!(got, oneshot_bars(&src));
    }

    #[test]
    fn rate_limits_are_retried_transparently() {
        let src = sample_source();
        let clock = ManualClock::new();
        let mut probe = Probe::new(&src, &clock);
        probe.rate_limit_every = Some(10);
        let mut store = Store::open_in_memory().unwrap();
        let s = run_backfill(
            &mut store,
            &probe,
            &sample_portfolio(),
            TODAY,
            &cfg(90),
            &clock,
            &mut |_| {},
        )
        .unwrap();
        assert!(s.retries > 0);
        assert_eq!(s.requests, 126 + s.retries);
        assert_eq!(s.chunks, 63);
    }

    #[test]
    fn warm_cache_needs_no_backfill() {
        let src = sample_source();
        let clock = ManualClock::new();
        let mut store = Store::open_in_memory().unwrap();
        let p = sample_portfolio();
        run_backfill(
            &mut store,
            &src,
            &p,
            d(2025, 6, 6),
            &cfg(90),
            &clock,
            &mut |_| {},
        )
        .unwrap();
        let next = plan(&store, &p, TODAY, &cfg(90)).unwrap();
        assert!(!next.needs_backfill());
        assert_eq!(next.remaining_chunks(), 7); // one small chunk each
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(12))]

        /// Any chunk size yields exactly the same cache as a one-shot sync.
        #[test]
        fn chunk_size_does_not_change_result(chunk in 1_u64..400) {
            let src = SyntheticSource::new();
            let clock = ManualClock::new();
            let inst = Instrument { ticker: "PROP".into(), name: "P".into(), currency: crate::domain::Currency::Usd };
            let p = Portfolio { holdings: vec![crate::domain::Holding { instrument: inst.clone(), quantity: rust_decimal::Decimal::ONE }] };
            let c = BackfillConfig { chunk_days: chunk, history_days: 120, throttle: ThrottleConfig::default() };
            let mut chunked = Store::open_in_memory().unwrap();
            run_backfill(&mut chunked, &src, &p, TODAY, &c, &clock, &mut |_| {}).unwrap();
            let mut oneshot = Store::open_in_memory().unwrap();
            oneshot.sync_ticker(&src, &inst, TODAY, 120).unwrap();
            prop_assert_eq!(chunked.bars("PROP").unwrap(), oneshot.bars("PROP").unwrap());
            prop_assert_eq!(chunked.dividends("PROP").unwrap(), oneshot.dividends("PROP").unwrap());
        }
    }
}
