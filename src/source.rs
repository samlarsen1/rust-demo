//! Market data sources.
//!
//! [`MarketDataSource`] is the single seam through which external data enters
//! the system. It is deliberately **read-only**: it exposes historical bars and
//! dividends and nothing else. A future Yahoo Finance or Trading 212 metadata
//! adapter only has to implement these two methods.
//!
//! [`SyntheticSource`] is a deterministic, offline generator so the demo and the
//! test-suite work without network access.

use std::collections::HashMap;
use std::f64::consts::PI;

use chrono::{Datelike, Days, NaiveDate, Weekday};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;

use crate::domain::{DailyBar, DividendEvent, Instrument};
use crate::error::{Error, Result};

/// Read-only provider of historical market data.
pub trait MarketDataSource {
    /// Short identifier for logs and reports.
    fn name(&self) -> &str;

    /// Daily bars for `instrument` with `from <= date <= to` (inclusive).
    ///
    /// # Errors
    /// Returns an error when the upstream provider fails.
    fn fetch_bars(
        &self,
        instrument: &Instrument,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<DailyBar>>;

    /// Dividends for `instrument` with `from <= ex_date <= to` (inclusive).
    ///
    /// # Errors
    /// Returns an error when the upstream provider fails.
    fn fetch_dividends(
        &self,
        instrument: &Instrument,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<DividendEvent>>;
}

/// Simplified exchange calendar: weekdays except 1 January and 25/26 December.
pub fn is_trading_day(date: NaiveDate) -> bool {
    let weekend = matches!(date.weekday(), Weekday::Sat | Weekday::Sun);
    let holiday = matches!((date.month(), date.day()), (1, 1) | (12, 25 | 26));
    !weekend && !holiday
}

/// Parameters of a synthetic price process for one ticker.
#[derive(Debug, Clone, PartialEq)]
pub struct SyntheticProfile {
    /// Price on the generator epoch.
    pub start_price: f64,
    /// Annual log drift (0.20 = +20%/yr trend).
    pub annual_drift: f64,
    /// Daily shock standard deviation of the noise process.
    pub daily_vol: f64,
    /// Amplitude of the deterministic business cycle in log space.
    pub cycle_amplitude: f64,
    /// Business cycle period in calendar days.
    pub cycle_period_days: f64,
    /// Annual dividend yield as a fraction (0.05 = 5%); `0.0` = no dividends.
    pub dividend_yield: f64,
}

impl SyntheticProfile {
    /// Fallback profile for tickers without explicit configuration.
    fn fallback(seed: u64) -> Self {
        let mut rng = SplitMix64::new(seed);
        Self {
            start_price: 20.0 + rng.next_f64() * 180.0,
            annual_drift: 0.02 + rng.next_f64() * 0.15,
            daily_vol: 0.010 + rng.next_f64() * 0.015,
            cycle_amplitude: 0.05 + rng.next_f64() * 0.10,
            cycle_period_days: 250.0 + rng.next_f64() * 300.0,
            dividend_yield: if rng.next_f64() < 0.5 {
                0.0
            } else {
                0.02 + rng.next_f64() * 0.04
            },
        }
    }
}

/// Deterministic synthetic market data generator.
///
/// Prices follow `ln P(t) = ln P0 + drift·t + A·sin(2πt/T + φ) + X(t)` where
/// `X` is a mean-reverting (Ornstein–Uhlenbeck) noise process seeded from the
/// ticker. The process is always simulated from a fixed epoch, so the bar for a
/// given date is identical regardless of which range is requested — a property
/// the incremental sync relies on.
#[derive(Debug, Clone, Default)]
pub struct SyntheticSource {
    profiles: HashMap<String, SyntheticProfile>,
}

/// Mean reversion speed of the noise process (per trading day).
const OU_THETA: f64 = 0.03;

impl SyntheticSource {
    /// First simulated day.
    pub const EPOCH: NaiveDate = match NaiveDate::from_ymd_opt(2019, 1, 2) {
        Some(d) => d,
        None => NaiveDate::MIN,
    };

    /// Empty source; every ticker uses a seeded fallback profile.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) the profile for `ticker`.
    #[must_use]
    pub fn with_profile(mut self, ticker: &str, profile: SyntheticProfile) -> Self {
        self.profiles.insert(ticker.to_owned(), profile);
        self
    }

    fn profile(&self, ticker: &str) -> SyntheticProfile {
        self.profiles
            .get(ticker)
            .cloned()
            .unwrap_or_else(|| SyntheticProfile::fallback(seed_for(ticker)))
    }

    /// Simulate from [`Self::EPOCH`] to `to`, returning all bars and dividends.
    fn simulate(
        &self,
        instrument: &Instrument,
        to: NaiveDate,
    ) -> Result<(Vec<DailyBar>, Vec<DividendEvent>)> {
        let p = self.profile(&instrument.ticker);
        if !(p.start_price > 0.0 && p.cycle_period_days > 0.0) {
            return Err(Error::Source(format!(
                "invalid profile for {}",
                instrument.ticker
            )));
        }
        let seed = seed_for(&instrument.ticker);
        let mut rng = SplitMix64::new(seed);
        let phase = (seed % 360) as f64 / 360.0 * 2.0 * PI;

        let mut bars = Vec::new();
        let mut dividends = Vec::new();
        let mut noise = 0.0_f64;
        let mut prev_close = p.start_price;
        let mut adj_factor = 1.0_f64;
        let mut paid_quarters: Vec<(i32, u32)> = Vec::new();

        let mut date = Self::EPOCH;
        while date <= to {
            if is_trading_day(date) {
                let t_days =
                    f64::from(i32::try_from((date - Self::EPOCH).num_days()).unwrap_or(i32::MAX));
                noise = noise.mul_add(1.0 - OU_THETA, p.daily_vol * rng.next_gaussian());
                let log_price = p.start_price.ln()
                    + p.annual_drift * t_days / 365.25
                    + p.cycle_amplitude * (2.0 * PI * t_days / p.cycle_period_days + phase).sin()
                    + noise;
                let close = log_price.exp();
                let open = prev_close * p.daily_vol.mul_add(0.25 * rng.next_gaussian(), 1.0);
                let wick = 1.0 + (p.daily_vol * 0.5 * rng.next_gaussian()).abs();
                let high = open.max(close) * wick;
                let low = open.min(close) / wick;
                let volume_base = 1_000_000.0 * (1.0 + rng.next_gaussian().abs());

                // Quarterly dividends: first trading day on/after the 15th of
                // Mar/Jun/Sep/Dec.
                let quarter_key = (date.year(), date.month());
                if p.dividend_yield > 0.0
                    && date.month() % 3 == 0
                    && date.day() >= 15
                    && !paid_quarters.contains(&quarter_key)
                {
                    paid_quarters.push(quarter_key);
                    let amount = close * p.dividend_yield / 4.0;
                    adj_factor *= 1.0 + amount / close;
                    dividends.push(DividendEvent {
                        ticker: instrument.ticker.clone(),
                        ex_date: date,
                        amount: money(amount)?,
                    });
                }

                bars.push(DailyBar {
                    ticker: instrument.ticker.clone(),
                    date,
                    open: money(open)?,
                    high: money(high)?,
                    low: money(low)?,
                    close: money(close)?,
                    adj_close: money(close * adj_factor)?,
                    // Truncation intended: volume is a positive whole number.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    volume: volume_base.round() as u64,
                });
                prev_close = close;
            }
            date = date
                .checked_add_days(Days::new(1))
                .ok_or_else(|| Error::Source("date overflow".into()))?;
        }
        Ok((bars, dividends))
    }
}

impl MarketDataSource for SyntheticSource {
    fn name(&self) -> &'static str {
        "synthetic"
    }

    fn fetch_bars(
        &self,
        instrument: &Instrument,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<DailyBar>> {
        if from > to {
            return Ok(Vec::new());
        }
        let (bars, _) = self.simulate(instrument, to)?;
        Ok(bars.into_iter().filter(|b| b.date >= from).collect())
    }

    fn fetch_dividends(
        &self,
        instrument: &Instrument,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<DividendEvent>> {
        if from > to {
            return Ok(Vec::new());
        }
        let (_, divs) = self.simulate(instrument, to)?;
        Ok(divs.into_iter().filter(|d| d.ex_date >= from).collect())
    }
}

/// Convert a model price to a 4dp [`Decimal`].
fn money(value: f64) -> Result<Decimal> {
    Decimal::from_f64(value)
        .map(|d| d.round_dp(4))
        .ok_or_else(|| Error::Source(format!("non-finite price {value}")))
}

/// FNV-1a hash of the ticker, used as RNG seed.
fn seed_for(ticker: &str) -> u64 {
    ticker.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Tiny deterministic PRNG (`SplitMix64`) — avoids a `rand` dependency.
#[derive(Debug, Clone)]
struct SplitMix64(u64);

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `(0, 1]`.
    fn next_f64(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 1.0) / (1_u64 << 53) as f64
    }

    /// Standard normal via Box–Muller.
    fn next_gaussian(&mut self) -> f64 {
        let u1 = self.next_f64();
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Currency;

    fn inst(t: &str) -> Instrument {
        Instrument {
            ticker: t.into(),
            name: t.into(),
            currency: Currency::Usd,
        }
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn calendar_skips_weekends_and_holidays() {
        assert!(!is_trading_day(d(2025, 1, 4))); // Saturday
        assert!(!is_trading_day(d(2025, 12, 25)));
        assert!(is_trading_day(d(2025, 1, 6)));
    }

    #[test]
    fn bars_are_range_independent() {
        let src = SyntheticSource::new();
        let i = inst("TEST");
        let full = src.fetch_bars(&i, d(2024, 1, 1), d(2024, 3, 31)).unwrap();
        let tail = src.fetch_bars(&i, d(2024, 3, 1), d(2024, 3, 31)).unwrap();
        let overlap: Vec<_> = full
            .into_iter()
            .filter(|b| b.date >= d(2024, 3, 1))
            .collect();
        assert_eq!(overlap, tail);
        assert!(tail.iter().all(|b| is_trading_day(b.date)));
    }

    #[test]
    fn ohlc_is_consistent() {
        let src = SyntheticSource::new();
        for b in src
            .fetch_bars(&inst("OHLC"), d(2023, 1, 1), d(2024, 12, 31))
            .unwrap()
        {
            assert!(b.low <= b.open && b.low <= b.close, "{b:?}");
            assert!(b.high >= b.open && b.high >= b.close, "{b:?}");
            assert!(b.low > Decimal::ZERO);
        }
    }

    #[test]
    fn inverted_range_is_empty() {
        let src = SyntheticSource::new();
        let bars = src
            .fetch_bars(&inst("X"), d(2024, 2, 1), d(2024, 1, 1))
            .unwrap();
        assert_eq!(bars.len(), 0);
    }
}
