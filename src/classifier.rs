//! Technical indicators plus trend-territory and archetype classification.
//!
//! Indicators are dimensionless statistics and therefore computed in `f64`
//! from the stored [`Decimal`] prices. Monetary inputs (dividend amounts, the
//! last close) stay in `Decimal` until a ratio is formed.

use chrono::{Days, NaiveDate};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::domain::{DailyBar, DividendEvent, StockArchetype, TrendTerritory};

/// Trading days per year used for annualisation.
pub const TRADING_DAYS: usize = 252;

/// Drawdown (%) from the 52-week high at which a `Pullback` starts.
pub const PULLBACK_DD: f64 = 5.0;
/// Drawdown (%) at which a `Correction` starts.
pub const CORRECTION_DD: f64 = 10.0;
/// Drawdown (%) at which a `BearMarket` starts.
pub const BEAR_DD: f64 = 20.0;

/// Minimum trailing yield (%) for `Income`.
pub const INCOME_MIN_YIELD: f64 = 4.0;
/// Minimum payout consistency (fraction of quarters paid) for `Income`.
pub const INCOME_MIN_CONSISTENCY: f64 = 0.75;
/// Maximum annualised volatility (%) for `Income`.
pub const INCOME_MAX_VOL: f64 = 25.0;
/// Minimum price CAGR (%) for `Growth`.
pub const GROWTH_MIN_CAGR: f64 = 15.0;
/// Maximum trailing yield (%) for `Growth`.
pub const GROWTH_MAX_YIELD: f64 = 2.0;
/// Minimum cyclicality (std-dev of 6-month log returns) for `Cyclical`.
pub const CYCLICAL_MIN_CYCLICALITY: f64 = 0.15;
/// Minimum annualised volatility (%) for `Cyclical`.
pub const CYCLICAL_MIN_VOL: f64 = 22.0;
/// Maximum annualised volatility (%) for `Defensive`.
pub const DEFENSIVE_MAX_VOL: f64 = 20.0;

const EPS: f64 = 1e-12;

/// Quantitative features of one instrument as of its latest bar.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Metrics {
    /// Date of the latest bar.
    pub as_of: Option<NaiveDate>,
    /// Number of bars analysed.
    pub bars: usize,
    /// Latest close.
    pub last_close: Option<Decimal>,
    /// 50-day simple moving average.
    pub sma50: Option<f64>,
    /// 200-day simple moving average.
    pub sma200: Option<f64>,
    /// 14-day Wilder RSI in `[0, 100]`.
    pub rsi14: Option<f64>,
    /// Annualised volatility of daily log returns, in percent.
    pub annual_vol_pct: f64,
    /// Drawdown from the trailing 52-week high, in percent (`[0, 100]`).
    pub drawdown_pct: f64,
    /// Compound annual price growth over the available history, in percent.
    pub cagr_pct: Option<f64>,
    /// Trailing 12-month dividend yield, in percent.
    pub dividend_yield_pct: f64,
    /// Fraction of the last 8 quarters with a dividend (`[0, 1]`).
    pub payout_consistency: f64,
    /// Standard deviation of rolling 6-month log returns (price cyclicality).
    pub cyclicality: f64,
}

impl Metrics {
    /// Latest close as `f64`.
    pub fn last_close_f64(&self) -> Option<f64> {
        self.last_close.and_then(|c| c.to_f64())
    }

    /// `true` when the close is below the 50-day SMA.
    pub fn below_sma50(&self) -> bool {
        matches!((self.last_close_f64(), self.sma50), (Some(c), Some(s)) if c < s)
    }

    /// `true` when the close is below the 200-day SMA.
    pub fn below_sma200(&self) -> bool {
        matches!((self.last_close_f64(), self.sma200), (Some(c), Some(s)) if c < s)
    }
}

/// Full classification of one instrument.
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    /// Ticker.
    pub ticker: String,
    /// Underlying features.
    pub metrics: Metrics,
    /// Style archetype.
    pub archetype: StockArchetype,
    /// Current trend territory.
    pub territory: TrendTerritory,
}

/// Simple moving average of the last `window` values.
pub fn sma(values: &[f64], window: usize) -> Option<f64> {
    if window == 0 || values.len() < window {
        return None;
    }
    let tail = values.get(values.len() - window..)?;
    Some(tail.iter().sum::<f64>() / window as f64)
}

/// Wilder-smoothed RSI over `period`, clamped to `[0, 100]`.
///
/// A perfectly flat series returns `50.0`.
pub fn wilder_rsi(closes: &[f64], period: usize) -> Option<f64> {
    if period == 0 || closes.len() <= period {
        return None;
    }
    let changes: Vec<f64> = closes.windows(2).map(|w| w[1] - w[0]).collect();
    let (seed, rest) = changes.split_at(period);
    let p = period as f64;
    let mut avg_gain = seed.iter().map(|c| c.max(0.0)).sum::<f64>() / p;
    let mut avg_loss = seed.iter().map(|c| (-c).max(0.0)).sum::<f64>() / p;
    for c in rest {
        avg_gain = avg_gain.mul_add(p - 1.0, c.max(0.0)) / p;
        avg_loss = avg_loss.mul_add(p - 1.0, (-c).max(0.0)) / p;
    }
    let rsi = if avg_loss < EPS {
        if avg_gain < EPS { 50.0 } else { 100.0 }
    } else {
        100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
    };
    Some(rsi.clamp(0.0, 100.0))
}

fn log_returns(closes: &[f64]) -> Vec<f64> {
    closes
        .windows(2)
        .filter(|w| w[0] > 0.0 && w[1] > 0.0)
        .map(|w| (w[1] / w[0]).ln())
        .collect()
}

fn std_dev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    var.max(0.0).sqrt()
}

/// Annualised volatility of daily log returns, in percent.
pub fn annualized_volatility(closes: &[f64]) -> f64 {
    std_dev(&log_returns(closes)) * (TRADING_DAYS as f64).sqrt() * 100.0
}

/// Drawdown of the last close from the trailing 52-week high, in `[0, 100]`.
pub fn drawdown_from_high(closes: &[f64]) -> f64 {
    let start = closes.len().saturating_sub(TRADING_DAYS);
    let window = closes.get(start..).unwrap_or_default();
    let (Some(last), Some(high)) = (window.last(), window.iter().copied().reduce(f64::max)) else {
        return 0.0;
    };
    if high <= 0.0 {
        return 0.0;
    }
    ((high - last) / high * 100.0).clamp(0.0, 100.0)
}

/// Compound annual growth rate in percent; `None` for spans under ~6 months.
pub fn cagr(first: f64, last: f64, years: f64) -> Option<f64> {
    if years < 0.5 || first <= 0.0 || last <= 0.0 {
        return None;
    }
    Some(((last / first).powf(1.0 / years) - 1.0) * 100.0)
}

/// Std-dev of 126-day log returns sampled monthly — a proxy for how strongly
/// the price swings with the business cycle.
pub fn cyclicality(closes: &[f64]) -> f64 {
    const HORIZON: usize = 126;
    const STEP: usize = 21;
    let samples: Vec<f64> = (HORIZON..closes.len())
        .step_by(STEP)
        .filter_map(|i| {
            let (a, b) = (*closes.get(i - HORIZON)?, *closes.get(i)?);
            (a > 0.0 && b > 0.0).then(|| (b / a).ln())
        })
        .collect();
    if samples.len() < 3 {
        0.0
    } else {
        std_dev(&samples)
    }
}

/// Trailing 12-month dividend yield in percent.
pub fn trailing_yield(dividends: &[DividendEvent], last_close: Decimal, as_of: NaiveDate) -> f64 {
    if last_close <= Decimal::ZERO {
        return 0.0;
    }
    let start = as_of
        .checked_sub_days(Days::new(365))
        .unwrap_or(NaiveDate::MIN);
    let paid: Decimal = dividends
        .iter()
        .filter(|d| d.ex_date > start && d.ex_date <= as_of)
        .map(|d| d.amount)
        .sum();
    (paid / last_close * Decimal::ONE_HUNDRED)
        .to_f64()
        .unwrap_or(0.0)
}

/// Fraction of the last 8 quarterly windows (that lie within the available
/// history) containing at least one dividend.
pub fn payout_consistency(
    dividends: &[DividendEvent],
    history_start: NaiveDate,
    as_of: NaiveDate,
) -> f64 {
    let mut covered = 0_u32;
    let mut paid = 0_u32;
    for q in 0..8_u64 {
        let (Some(end), Some(start)) = (
            as_of.checked_sub_days(Days::new(91 * q)),
            as_of.checked_sub_days(Days::new(91 * (q + 1))),
        ) else {
            break;
        };
        if start < history_start {
            break;
        }
        covered += 1;
        if dividends
            .iter()
            .any(|d| d.ex_date > start && d.ex_date <= end)
        {
            paid += 1;
        }
    }
    if covered == 0 {
        0.0
    } else {
        f64::from(paid) / f64::from(covered)
    }
}

/// Compute all metrics from cached bars (oldest first) and dividends.
pub fn compute_metrics(bars: &[DailyBar], dividends: &[DividendEvent]) -> Metrics {
    let (Some(first), Some(last)) = (bars.first(), bars.last()) else {
        return Metrics::default();
    };
    let closes: Vec<f64> = bars.iter().filter_map(|b| b.close.to_f64()).collect();
    let years = (last.date - first.date).num_days() as f64 / 365.25;
    Metrics {
        as_of: Some(last.date),
        bars: bars.len(),
        last_close: Some(last.close),
        sma50: sma(&closes, 50),
        sma200: sma(&closes, 200),
        rsi14: wilder_rsi(&closes, 14),
        annual_vol_pct: annualized_volatility(&closes),
        drawdown_pct: drawdown_from_high(&closes),
        cagr_pct: cagr(
            first.close.to_f64().unwrap_or(0.0),
            last.close.to_f64().unwrap_or(0.0),
            years,
        ),
        dividend_yield_pct: trailing_yield(dividends, last.close, last.date),
        payout_consistency: payout_consistency(dividends, first.date, last.date),
        cyclicality: cyclicality(&closes),
    }
}

/// Map drawdown and moving averages to a [`TrendTerritory`].
///
/// Lower bounds are inclusive: exactly 5% → `Pullback`, 10% → `Correction`,
/// 20% → `BearMarket`.
pub fn classify_territory(m: &Metrics) -> TrendTerritory {
    let dd = m.drawdown_pct;
    if dd >= BEAR_DD {
        TrendTerritory::BearMarket
    } else if dd >= CORRECTION_DD {
        TrendTerritory::Correction
    } else if dd >= PULLBACK_DD {
        TrendTerritory::Pullback
    } else {
        match (m.last_close_f64(), m.sma50, m.sma200) {
            (Some(c), Some(s50), Some(s200)) if c > s200 && s50 > s200 => TrendTerritory::Uptrend,
            _ => TrendTerritory::Chop,
        }
    }
}

/// Rule-based archetype classification (rules evaluated in order):
///
/// 1. **Income** — yield ≥ 4%, paid in ≥ 75% of quarters, volatility < 25%.
/// 2. **Growth** — price CAGR ≥ 15% and yield < 2%.
/// 3. **Cyclical** — cyclicality ≥ 0.15 and volatility ≥ 22%.
/// 4. **Defensive** — volatility < 20%.
/// 5. Fallback — yield ≥ 3% → Income, CAGR ≥ 8% → Growth, else Cyclical.
pub fn classify_archetype(m: &Metrics) -> StockArchetype {
    let cagr = m.cagr_pct.unwrap_or(0.0);
    let (y, vol) = (m.dividend_yield_pct, m.annual_vol_pct);
    if y >= INCOME_MIN_YIELD
        && m.payout_consistency >= INCOME_MIN_CONSISTENCY
        && vol < INCOME_MAX_VOL
    {
        StockArchetype::Income
    } else if cagr >= GROWTH_MIN_CAGR && y < GROWTH_MAX_YIELD {
        StockArchetype::Growth
    } else if m.cyclicality >= CYCLICAL_MIN_CYCLICALITY && vol >= CYCLICAL_MIN_VOL {
        StockArchetype::Cyclical
    } else if vol < DEFENSIVE_MAX_VOL {
        StockArchetype::Defensive
    } else if y >= 3.0 {
        StockArchetype::Income
    } else if cagr >= 8.0 {
        StockArchetype::Growth
    } else {
        StockArchetype::Cyclical
    }
}

/// Compute metrics and classify one instrument.
pub fn analyze(ticker: &str, bars: &[DailyBar], dividends: &[DividendEvent]) -> Classification {
    let metrics = compute_metrics(bars, dividends);
    Classification {
        ticker: ticker.to_owned(),
        archetype: classify_archetype(&metrics),
        territory: classify_territory(&metrics),
        metrics,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use rust_decimal::prelude::FromPrimitive;

    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn bars_from(closes: &[f64]) -> Vec<DailyBar> {
        closes
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let p = Decimal::from_f64(*c).unwrap().round_dp(4);
                DailyBar {
                    ticker: "T".into(),
                    date: d(2023, 1, 2) + Days::new(i as u64),
                    open: p,
                    high: p,
                    low: p,
                    close: p,
                    adj_close: p,
                    volume: 1,
                }
            })
            .collect()
    }

    fn metrics_with(dd: f64) -> Metrics {
        Metrics {
            drawdown_pct: dd,
            ..Metrics::default()
        }
    }

    #[test]
    fn territory_drawdown_boundaries() {
        assert_eq!(
            classify_territory(&metrics_with(4.99)),
            TrendTerritory::Chop
        );
        assert_eq!(
            classify_territory(&metrics_with(5.0)),
            TrendTerritory::Pullback
        );
        assert_eq!(
            classify_territory(&metrics_with(9.99)),
            TrendTerritory::Pullback
        );
        assert_eq!(
            classify_territory(&metrics_with(10.0)),
            TrendTerritory::Correction
        );
        assert_eq!(
            classify_territory(&metrics_with(19.99)),
            TrendTerritory::Correction
        );
        assert_eq!(
            classify_territory(&metrics_with(20.0)),
            TrendTerritory::BearMarket
        );
        assert_eq!(
            classify_territory(&metrics_with(85.0)),
            TrendTerritory::BearMarket
        );
    }

    #[test]
    fn steady_rise_is_uptrend() {
        let closes: Vec<f64> = (0..300).map(|i| 100.0 + f64::from(i)).collect();
        let c = analyze("T", &bars_from(&closes), &[]);
        assert_eq!(c.territory, TrendTerritory::Uptrend);
        assert!(c.metrics.drawdown_pct < 1e-9);
        assert!((c.metrics.rsi14.unwrap() - 100.0).abs() < 1e-9);
    }

    fn base() -> Metrics {
        Metrics {
            cagr_pct: Some(5.0),
            annual_vol_pct: 21.0,
            dividend_yield_pct: 1.0,
            payout_consistency: 1.0,
            cyclicality: 0.05,
            ..Metrics::default()
        }
    }

    #[test]
    fn archetype_income() {
        let m = Metrics {
            dividend_yield_pct: 6.0,
            annual_vol_pct: 15.0,
            ..base()
        };
        assert_eq!(classify_archetype(&m), StockArchetype::Income);
        // Inconsistent payer is not Income.
        let m = Metrics {
            payout_consistency: 0.5,
            ..m
        };
        assert_ne!(classify_archetype(&m), StockArchetype::Income);
    }

    #[test]
    fn archetype_growth() {
        let m = Metrics {
            cagr_pct: Some(40.0),
            annual_vol_pct: 45.0,
            dividend_yield_pct: 0.0,
            ..base()
        };
        assert_eq!(classify_archetype(&m), StockArchetype::Growth);
    }

    #[test]
    fn archetype_cyclical_high_yield_high_vol() {
        // A miner: big yield but too volatile/cyclical to count as Income.
        let m = Metrics {
            dividend_yield_pct: 6.5,
            annual_vol_pct: 29.0,
            cyclicality: 0.22,
            cagr_pct: Some(2.0),
            ..base()
        };
        assert_eq!(classify_archetype(&m), StockArchetype::Cyclical);
    }

    #[test]
    fn archetype_defensive() {
        let m = Metrics {
            annual_vol_pct: 12.0,
            dividend_yield_pct: 2.5,
            ..base()
        };
        assert_eq!(classify_archetype(&m), StockArchetype::Defensive);
    }

    #[test]
    fn empty_history_is_safe() {
        let c = analyze("T", &[], &[]);
        assert_eq!(c.metrics, Metrics::default());
        assert_eq!(c.territory, TrendTerritory::Chop);
        assert!(c.metrics.rsi14.is_none() && c.metrics.cagr_pct.is_none());
    }

    #[test]
    fn zero_dividend_stock() {
        let closes: Vec<f64> = (0..400).map(|i| 50.0 + f64::from(i % 7)).collect();
        let m = compute_metrics(&bars_from(&closes), &[]);
        assert!(m.dividend_yield_pct.abs() < 1e-12);
        assert!(m.payout_consistency.abs() < 1e-12);
    }

    #[test]
    fn flat_series() {
        let m = compute_metrics(&bars_from(&[42.0; 300]), &[]);
        assert!((m.rsi14.unwrap() - 50.0).abs() < 1e-12);
        assert!(m.annual_vol_pct.abs() < 1e-12);
        assert!(m.drawdown_pct.abs() < 1e-12);
        assert!(m.cagr_pct.unwrap().abs() < 1e-9);
        assert!(m.cyclicality.abs() < 1e-12);
        assert_eq!(classify_territory(&m), TrendTerritory::Chop);
        assert_eq!(classify_archetype(&m), StockArchetype::Defensive);
    }

    #[test]
    fn yield_and_consistency() {
        let as_of = d(2025, 6, 30);
        let divs: Vec<_> = [
            d(2024, 9, 16),
            d(2024, 12, 16),
            d(2025, 3, 17),
            d(2025, 6, 16),
        ]
        .into_iter()
        .map(|ex_date| DividendEvent {
            ticker: "T".into(),
            ex_date,
            amount: Decimal::new(25, 2),
        })
        .collect();
        let y = trailing_yield(&divs, Decimal::new(20, 0), as_of);
        assert!((y - 5.0).abs() < 1e-9, "{y}");
        // Only 1 year of payments inside a 2-year history → 4/8 quarters.
        let c = payout_consistency(&divs, d(2023, 6, 1), as_of);
        assert!((c - 0.5).abs() < 1e-9, "{c}");
        // With only 1 year of history every covered quarter paid.
        let c = payout_consistency(&divs, d(2024, 6, 1), as_of);
        assert!((c - 1.0).abs() < 1e-9, "{c}");
    }

    #[test]
    fn rsi_known_value() {
        // Alternating +1/-1 has equal average gain and loss → RSI ≈ 50.
        let closes: Vec<f64> = (0..100)
            .map(|i| if i % 2 == 0 { 10.0 } else { 11.0 })
            .collect();
        let rsi = wilder_rsi(&closes, 14).unwrap();
        assert!((rsi - 50.0).abs() < 5.0, "{rsi}");
        assert!(wilder_rsi(&closes[..14], 14).is_none());
    }

    proptest! {
        #[test]
        fn rsi_in_bounds(closes in prop::collection::vec(0.01_f64..10_000.0, 15..300)) {
            let rsi = wilder_rsi(&closes, 14).unwrap();
            prop_assert!((0.0..=100.0).contains(&rsi));
        }

        #[test]
        fn drawdown_in_bounds(closes in prop::collection::vec(0.01_f64..10_000.0, 0..400)) {
            let dd = drawdown_from_high(&closes);
            prop_assert!((0.0..=100.0).contains(&dd));
        }

        #[test]
        fn volatility_non_negative(closes in prop::collection::vec(0.01_f64..10_000.0, 0..300)) {
            prop_assert!(annualized_volatility(&closes) >= 0.0);
        }

        #[test]
        fn consistency_in_unit_interval(offsets in prop::collection::vec(0_u64..900, 0..20)) {
            let as_of = d(2025, 6, 30);
            let divs: Vec<_> = offsets.iter().map(|o| DividendEvent {
                ticker: "T".into(),
                ex_date: as_of - Days::new(*o),
                amount: Decimal::ONE,
            }).collect();
            let c = payout_consistency(&divs, d(2023, 1, 1), as_of);
            prop_assert!((0.0..=1.0).contains(&c));
        }
    }
}
