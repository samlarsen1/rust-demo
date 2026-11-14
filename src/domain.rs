//! Strongly typed financial domain model.
//!
//! Money (prices, dividends, valuations) is always [`Decimal`]; `f64` is only
//! used for dimensionless statistics such as RSI, volatility or weights.

use std::fmt;
use std::str::FromStr;

use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::error::Error;

/// Settlement / quote currency of an instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Currency {
    /// Pound sterling.
    Gbp,
    /// US dollar.
    Usd,
    /// Euro.
    Eur,
}

impl Currency {
    /// All supported currencies, in display order.
    pub const ALL: [Self; 3] = [Self::Gbp, Self::Usd, Self::Eur];

    /// ISO 4217 code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Gbp => "GBP",
            Self::Usd => "USD",
            Self::Eur => "EUR",
        }
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl FromStr for Currency {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "GBP" => Ok(Self::Gbp),
            "USD" => Ok(Self::Usd),
            "EUR" => Ok(Self::Eur),
            other => Err(Error::InvalidData(format!("unknown currency {other:?}"))),
        }
    }
}

/// Investment style classification derived from measurable quantitative rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StockArchetype {
    /// High price growth, little or no yield.
    Growth,
    /// High, consistent dividend yield.
    Income,
    /// Large, regime-driven price swings (commodities, industrials, ...).
    Cyclical,
    /// Low volatility, steady compounding.
    Defensive,
}

impl StockArchetype {
    /// All archetypes, in display order.
    pub const ALL: [Self; 4] = [Self::Growth, Self::Income, Self::Cyclical, Self::Defensive];
}

impl fmt::Display for StockArchetype {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Growth => "Growth",
            Self::Income => "Income",
            Self::Cyclical => "Cyclical",
            Self::Defensive => "Defensive",
        })
    }
}

/// Current trend regime ("territory") of a price series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TrendTerritory {
    /// Within 5% of the 52-week high, above a rising 200-day SMA.
    Uptrend,
    /// 5% – 10% below the 52-week high.
    Pullback,
    /// 10% – 20% below the 52-week high.
    Correction,
    /// More than 20% below the 52-week high.
    BearMarket,
    /// Near highs but without a confirmed trend (or insufficient history).
    Chop,
}

impl TrendTerritory {
    /// All territories, in display order.
    pub const ALL: [Self; 5] = [
        Self::Uptrend,
        Self::Pullback,
        Self::Correction,
        Self::BearMarket,
        Self::Chop,
    ];
}

impl fmt::Display for TrendTerritory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Uptrend => "Uptrend",
            Self::Pullback => "Pullback",
            Self::Correction => "Correction",
            Self::BearMarket => "BearMarket",
            Self::Chop => "Chop",
        })
    }
}

/// A tradable instrument (metadata only — this crate never trades).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instrument {
    /// Ticker symbol as used by the user's broker (e.g. `NG.`, `NVDA`).
    pub ticker: String,
    /// Human-readable name.
    pub name: String,
    /// Quote currency.
    pub currency: Currency,
}

/// One daily OHLCV bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyBar {
    /// Ticker symbol.
    pub ticker: String,
    /// Trading date.
    pub date: NaiveDate,
    /// Opening price.
    pub open: Decimal,
    /// Session high.
    pub high: Decimal,
    /// Session low.
    pub low: Decimal,
    /// Closing price.
    pub close: Decimal,
    /// Dividend-adjusted close (forward total-return adjusted, stable over time).
    pub adj_close: Decimal,
    /// Shares traded.
    pub volume: u64,
}

/// A cash dividend with its ex-dividend date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DividendEvent {
    /// Ticker symbol.
    pub ticker: String,
    /// Ex-dividend date.
    pub ex_date: NaiveDate,
    /// Cash amount per share in the instrument currency.
    pub amount: Decimal,
}

/// A position in the user's portfolio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holding {
    /// Instrument held.
    pub instrument: Instrument,
    /// Number of shares (fractional shares allowed).
    pub quantity: Decimal,
}

/// A read-only snapshot of a portfolio.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Portfolio {
    /// Positions.
    pub holdings: Vec<Holding>,
}

/// Static FX table converting instrument currencies into a GBP base.
///
/// Rates are illustrative constants so the demo is deterministic and offline.
pub fn fx_to_gbp(currency: Currency) -> Decimal {
    match currency {
        Currency::Gbp => Decimal::ONE,
        Currency::Usd => Decimal::new(79, 2),
        Currency::Eur => Decimal::new(85, 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn currency_roundtrip() {
        for c in Currency::ALL {
            assert_eq!(c.code().parse::<Currency>().unwrap(), c);
        }
        assert!("JPY".parse::<Currency>().is_err());
    }

    #[test]
    fn fx_base_is_identity_for_gbp() {
        assert_eq!(fx_to_gbp(Currency::Gbp), Decimal::ONE);
        assert!(fx_to_gbp(Currency::Usd) < Decimal::ONE);
    }
}
