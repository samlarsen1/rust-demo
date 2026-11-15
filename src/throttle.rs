//! Client-side rate limiting for market data sources.
//!
//! [`ThrottledSource`] wraps any [`MarketDataSource`] and
//!
//! * enforces a minimum interval between consecutive upstream requests, and
//! * retries [`Error::RateLimited`] responses with exponential backoff (or the
//!   server's `retry_after`), up to `max_retries`.
//!
//! Time is abstracted behind [`Clock`] so tests run instantly with
//! [`ManualClock`].

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use chrono::NaiveDate;

use crate::domain::{DailyBar, DividendEvent, Instrument};
use crate::error::{Error, Result};
use crate::source::MarketDataSource;

/// Source of time and sleeping.
pub trait Clock {
    /// Monotonic time since an arbitrary origin.
    fn now(&self) -> Duration;
    /// Block for `d`.
    fn sleep(&self, d: Duration);
}

/// Real wall-clock time.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Simulated clock: `sleep` advances time instantly and is recorded.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: Cell<Duration>,
    sleeps: RefCell<Vec<Duration>>,
}

impl ManualClock {
    /// New clock at time zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance time without recording a sleep (simulates work).
    pub fn advance(&self, d: Duration) {
        self.now.set(self.now.get() + d);
    }

    /// Every sleep requested so far.
    pub fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.borrow().clone()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.now.get()
    }

    fn sleep(&self, d: Duration) {
        self.sleeps.borrow_mut().push(d);
        self.advance(d);
    }
}

/// Rate-limit policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleConfig {
    /// Minimum gap between the *start* of consecutive upstream requests.
    pub min_interval: Duration,
    /// Retries after a [`Error::RateLimited`] before giving up.
    pub max_retries: u32,
    /// First backoff delay when the server gives no `retry_after`.
    pub initial_backoff: Duration,
    /// Cap for the exponential backoff (server `retry_after` is always honoured).
    pub max_backoff: Duration,
}

impl Default for ThrottleConfig {
    /// Conservative defaults suitable for free public APIs (~60 requests/min).
    fn default() -> Self {
        Self {
            min_interval: Duration::from_secs(1),
            max_retries: 5,
            initial_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(300),
        }
    }
}

impl ThrottleConfig {
    /// Same policy with a different request interval.
    #[must_use]
    pub const fn with_interval(mut self, min_interval: Duration) -> Self {
        self.min_interval = min_interval;
        self
    }
}

/// Counters exposed by [`ThrottledSource`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThrottleStats {
    /// Upstream requests attempted (including retried ones).
    pub requests: u64,
    /// Rate-limit responses that were retried.
    pub retries: u64,
    /// Total time spent waiting (interval spacing + backoff).
    pub waited: Duration,
}

/// A [`MarketDataSource`] decorator that spaces out requests and backs off
/// when the upstream rate-limits.
pub struct ThrottledSource<'a, C: Clock> {
    inner: &'a dyn MarketDataSource,
    clock: &'a C,
    config: ThrottleConfig,
    last_request: Cell<Option<Duration>>,
    stats: Cell<ThrottleStats>,
}

impl<'a, C: Clock> ThrottledSource<'a, C> {
    /// Wrap `inner` with `config`, using `clock` for time.
    pub fn new(inner: &'a dyn MarketDataSource, clock: &'a C, config: ThrottleConfig) -> Self {
        Self {
            inner,
            clock,
            config,
            last_request: Cell::new(None),
            stats: Cell::new(ThrottleStats::default()),
        }
    }

    /// Counters so far.
    pub fn stats(&self) -> ThrottleStats {
        self.stats.get()
    }

    /// The policy in force.
    pub const fn config(&self) -> ThrottleConfig {
        self.config
    }

    fn update(&self, f: impl FnOnce(&mut ThrottleStats)) {
        let mut s = self.stats.get();
        f(&mut s);
        self.stats.set(s);
    }

    fn pause(&self, d: Duration) {
        if !d.is_zero() {
            self.clock.sleep(d);
            self.update(|s| s.waited += d);
        }
    }

    /// Sleep until `min_interval` has passed since the previous request.
    fn wait_turn(&self) {
        if let Some(last) = self.last_request.get() {
            let elapsed = self.clock.now().saturating_sub(last);
            self.pause(self.config.min_interval.saturating_sub(elapsed));
        }
        self.last_request.set(Some(self.clock.now()));
        self.update(|s| s.requests += 1);
    }

    fn call<T>(&self, f: impl Fn() -> Result<T>) -> Result<T> {
        let mut backoff = self.config.initial_backoff;
        let mut attempt = 0;
        loop {
            self.wait_turn();
            match f() {
                Err(Error::RateLimited { retry_after }) if attempt < self.config.max_retries => {
                    attempt += 1;
                    self.update(|s| s.retries += 1);
                    self.pause(retry_after.unwrap_or(backoff));
                    backoff = backoff.saturating_mul(2).min(self.config.max_backoff);
                }
                result => return result,
            }
        }
    }
}

impl<C: Clock> MarketDataSource for ThrottledSource<'_, C> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn fetch_bars(
        &self,
        instrument: &Instrument,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<DailyBar>> {
        self.call(|| self.inner.fetch_bars(instrument, from, to))
    }

    fn fetch_dividends(
        &self,
        instrument: &Instrument,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<DividendEvent>> {
        self.call(|| self.inner.fetch_dividends(instrument, from, to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Currency;

    /// Fails with `RateLimited` for the first `fail` calls.
    struct Flaky {
        fail: Cell<u32>,
        retry_after: Option<Duration>,
    }

    impl MarketDataSource for Flaky {
        fn name(&self) -> &'static str {
            "flaky"
        }
        fn fetch_bars(&self, _: &Instrument, _: NaiveDate, _: NaiveDate) -> Result<Vec<DailyBar>> {
            if self.fail.get() > 0 {
                self.fail.set(self.fail.get() - 1);
                return Err(Error::RateLimited {
                    retry_after: self.retry_after,
                });
            }
            Ok(vec![])
        }
        fn fetch_dividends(
            &self,
            _: &Instrument,
            _: NaiveDate,
            _: NaiveDate,
        ) -> Result<Vec<DividendEvent>> {
            Ok(vec![])
        }
    }

    fn inst() -> Instrument {
        Instrument {
            ticker: "X".into(),
            name: "X".into(),
            currency: Currency::Usd,
        }
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2025, 1, 2).unwrap()
    }

    fn cfg() -> ThrottleConfig {
        ThrottleConfig {
            min_interval: Duration::from_secs(2),
            max_retries: 3,
            initial_backoff: Duration::from_secs(10),
            max_backoff: Duration::from_secs(25),
        }
    }

    #[test]
    fn spaces_requests_by_min_interval() {
        let clock = ManualClock::new();
        let inner = Flaky {
            fail: Cell::new(0),
            retry_after: None,
        };
        let src = ThrottledSource::new(&inner, &clock, cfg());
        src.fetch_bars(&inst(), day(), day()).unwrap(); // first call: no wait
        clock.advance(Duration::from_millis(500)); // simulated work
        src.fetch_bars(&inst(), day(), day()).unwrap();
        src.fetch_dividends(&inst(), day(), day()).unwrap();
        assert_eq!(
            clock.sleeps(),
            vec![Duration::from_millis(1500), Duration::from_secs(2)]
        );
        assert_eq!(src.stats().requests, 3);
    }

    #[test]
    fn retries_with_exponential_backoff_then_succeeds() {
        let clock = ManualClock::new();
        let inner = Flaky {
            fail: Cell::new(3),
            retry_after: None,
        };
        let src = ThrottledSource::new(&inner, &clock, cfg());
        src.fetch_bars(&inst(), day(), day()).unwrap();
        let backoffs: Vec<_> = clock
            .sleeps()
            .into_iter()
            .filter(|d| *d > Duration::from_secs(2))
            .collect();
        // 10s, 20s, then capped at 25s.
        assert_eq!(
            backoffs,
            vec![10, 20, 25]
                .into_iter()
                .map(Duration::from_secs)
                .collect::<Vec<_>>()
        );
        assert_eq!(src.stats().retries, 3);
        assert_eq!(src.stats().requests, 4);
    }

    #[test]
    fn honours_server_retry_after() {
        let clock = ManualClock::new();
        let inner = Flaky {
            fail: Cell::new(1),
            retry_after: Some(Duration::from_secs(90)),
        };
        let src = ThrottledSource::new(&inner, &clock, cfg());
        src.fetch_bars(&inst(), day(), day()).unwrap();
        assert!(clock.sleeps().contains(&Duration::from_secs(90)));
    }

    #[test]
    fn gives_up_after_max_retries() {
        let clock = ManualClock::new();
        let inner = Flaky {
            fail: Cell::new(10),
            retry_after: None,
        };
        let src = ThrottledSource::new(&inner, &clock, cfg());
        let err = src.fetch_bars(&inst(), day(), day()).unwrap_err();
        assert!(matches!(err, Error::RateLimited { .. }));
        assert_eq!(src.stats().requests, 4); // 1 + 3 retries
    }
}
