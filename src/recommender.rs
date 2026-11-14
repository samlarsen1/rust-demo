//! Portfolio exposure analysis and explainable, **read-only** research notes.
//!
//! Nothing here places, sizes or routes orders. Output is plain-language
//! research prompts for a human to review.

use std::collections::BTreeMap;
use std::fmt;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::classifier::Classification;
use crate::domain::{Currency, Holding, Portfolio, StockArchetype, TrendTerritory, fx_to_gbp};
use crate::error::{Error, Result};

/// Weight (%) above which a single archetype is flagged as concentrated.
pub const ARCHETYPE_CONCENTRATION_PCT: f64 = 50.0;
/// Weight (%) above which a single currency is flagged.
pub const CURRENCY_CONCENTRATION_PCT: f64 = 60.0;
/// Weight (%) above which a single position is flagged.
pub const SINGLE_NAME_PCT: f64 = 25.0;
/// Combined Correction + `BearMarket` weight (%) that triggers a risk note.
pub const DRAWDOWN_TERRITORY_PCT: f64 = 30.0;
/// RSI above which an uptrending name is considered extended.
pub const RSI_OVERBOUGHT: f64 = 70.0;

/// One valued, classified position.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionView {
    /// The holding.
    pub holding: Holding,
    /// Its classification.
    pub classification: Classification,
    /// Market value in GBP.
    pub value_gbp: Decimal,
    /// Portfolio weight in percent.
    pub weight_pct: f64,
}

impl PositionView {
    /// Ticker shortcut.
    pub fn ticker(&self) -> &str {
        &self.holding.instrument.ticker
    }
}

/// Valued portfolio plus exposure breakdowns.
#[derive(Debug, Clone, PartialEq)]
pub struct PortfolioAnalysis {
    /// Positions in config order.
    pub positions: Vec<PositionView>,
    /// Total value in GBP.
    pub total_value_gbp: Decimal,
}

impl PortfolioAnalysis {
    fn exposure<K: Ord + Copy>(
        &self,
        keys: &[K],
        key: impl Fn(&PositionView) -> K,
    ) -> Vec<(K, f64)> {
        let mut map: BTreeMap<K, f64> = keys.iter().map(|k| (*k, 0.0)).collect();
        for p in &self.positions {
            *map.entry(key(p)).or_insert(0.0) += p.weight_pct;
        }
        keys.iter()
            .map(|k| (*k, map.get(k).copied().unwrap_or(0.0)))
            .collect()
    }

    /// Weight (%) per currency, every currency listed.
    pub fn by_currency(&self) -> Vec<(Currency, f64)> {
        self.exposure(&Currency::ALL, |p| p.holding.instrument.currency)
    }

    /// Weight (%) per archetype, every archetype listed.
    pub fn by_archetype(&self) -> Vec<(StockArchetype, f64)> {
        self.exposure(&StockArchetype::ALL, |p| p.classification.archetype)
    }

    /// Weight (%) per territory, every territory listed.
    pub fn by_territory(&self) -> Vec<(TrendTerritory, f64)> {
        self.exposure(&TrendTerritory::ALL, |p| p.classification.territory)
    }
}

/// Value each holding in GBP and attach its classification.
///
/// # Errors
/// Fails if a holding has no classification or no price history.
pub fn value_portfolio(
    portfolio: &Portfolio,
    classifications: &[Classification],
) -> Result<PortfolioAnalysis> {
    let mut rows = Vec::with_capacity(portfolio.holdings.len());
    for h in &portfolio.holdings {
        let ticker = &h.instrument.ticker;
        let c = classifications
            .iter()
            .find(|c| &c.ticker == ticker)
            .ok_or_else(|| Error::InvalidInput(format!("no classification for {ticker}")))?;
        let close = c
            .metrics
            .last_close
            .ok_or_else(|| Error::InvalidData(format!("no price history for {ticker}")))?;
        let value = (h.quantity * close * fx_to_gbp(h.instrument.currency)).round_dp(2);
        rows.push((h.clone(), c.clone(), value));
    }
    let values: Vec<Decimal> = rows.iter().map(|r| r.2).collect();
    let weights = weights_pct(&values);
    let total = values.iter().copied().sum();
    let positions = rows
        .into_iter()
        .zip(weights)
        .map(
            |((holding, classification, value_gbp), weight_pct)| PositionView {
                holding,
                classification,
                value_gbp,
                weight_pct,
            },
        )
        .collect();
    Ok(PortfolioAnalysis {
        positions,
        total_value_gbp: total,
    })
}

/// Convert monetary values into percentage weights. Returns all zeros when the
/// total is not positive.
pub fn weights_pct(values: &[Decimal]) -> Vec<f64> {
    let total: Decimal = values.iter().copied().sum();
    if total <= Decimal::ZERO {
        return vec![0.0; values.len()];
    }
    values
        .iter()
        .map(|v| (*v / total * Decimal::ONE_HUNDRED).to_f64().unwrap_or(0.0))
        .collect()
}

/// How urgently a note deserves attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Material imbalance.
    Alert,
    /// Worth researching.
    Notice,
    /// Informational.
    Info,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Alert => "ALERT",
            Self::Notice => "NOTICE",
            Self::Info => "INFO",
        })
    }
}

/// A deterministic, explainable research note (never an order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    /// Severity.
    pub severity: Severity,
    /// Short rule name.
    pub rule: &'static str,
    /// Human-readable explanation.
    pub message: String,
    /// Tickers involved.
    pub tickers: Vec<String>,
}

fn join_tickers(t: &[String]) -> String {
    match t {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// Generate research notes, ordered by severity (stable within a severity).
pub fn recommend(a: &PortfolioAnalysis) -> Vec<Recommendation> {
    let mut out = Vec::new();
    style_rules(a, &mut out);
    income_pullback_rule(a, &mut out);
    concentration_rules(a, &mut out);
    trend_rules(a, &mut out);
    if out.is_empty() {
        out.push(Recommendation {
            severity: Severity::Info,
            rule: "balanced",
            message: "No material style, currency or trend imbalances detected.".into(),
            tickers: vec![],
        });
    }
    out.sort_by_key(|r| r.severity);
    out
}

fn style_rules(a: &PortfolioAnalysis, out: &mut Vec<Recommendation>) {
    let mut styles = a.by_archetype();
    styles.sort_by(|x, y| y.1.total_cmp(&x.1));
    if styles
        .first()
        .is_some_and(|(_, w)| *w > ARCHETYPE_CONCENTRATION_PCT)
    {
        let mix: Vec<String> = styles
            .iter()
            .filter(|(_, w)| *w > 0.0)
            .map(|(k, w)| format!("{w:.0}% {k}"))
            .collect();
        out.push(Recommendation {
            severity: Severity::Alert,
            rule: "style-concentration",
            message: format!(
                "Portfolio is {} — style concentration risk.",
                mix.join(" / ")
            ),
            tickers: vec![],
        });
    }
    for (k, w) in &styles {
        if *w <= 0.0 {
            let hint = match k {
                StockArchetype::Defensive => {
                    "research low-volatility candidates (utilities, staples, baseload infrastructure)"
                }
                StockArchetype::Income => "research consistent dividend payers",
                StockArchetype::Growth => "research structural compounders",
                StockArchetype::Cyclical => "research commodity/industrial cycle exposure",
            };
            out.push(Recommendation {
                severity: Severity::Notice,
                rule: "zero-exposure",
                message: format!("Zero {k} exposure detected — {hint}."),
                tickers: vec![],
            });
        }
    }
}

fn income_pullback_rule(a: &PortfolioAnalysis, out: &mut Vec<Recommendation>) {
    for territory in [
        TrendTerritory::Pullback,
        TrendTerritory::Correction,
        TrendTerritory::BearMarket,
    ] {
        let tickers: Vec<String> = a
            .positions
            .iter()
            .filter(|p| {
                p.classification.archetype == StockArchetype::Income
                    && p.classification.territory == territory
                    && p.classification.metrics.below_sma50()
            })
            .map(|p| p.ticker().to_owned())
            .collect();
        if tickers.is_empty() {
            continue;
        }
        let verb = if tickers.len() == 1 { "is" } else { "are" };
        out.push(Recommendation {
            severity: Severity::Notice,
            rule: "income-pullback",
            message: format!(
                "{} {verb} in {territory} territory below 50-SMA — candidate for income rebalancing research.",
                join_tickers(&tickers)
            ),
            tickers,
        });
    }
}

fn concentration_rules(a: &PortfolioAnalysis, out: &mut Vec<Recommendation>) {
    for (ccy, w) in a.by_currency() {
        if w > CURRENCY_CONCENTRATION_PCT {
            out.push(Recommendation {
                severity: Severity::Alert,
                rule: "currency-concentration",
                message: format!(
                    "{w:.0}% of value is {ccy}-denominated — concentrated FX exposure vs GBP base."
                ),
                tickers: vec![],
            });
        }
    }
    for p in &a.positions {
        if p.weight_pct > SINGLE_NAME_PCT {
            out.push(Recommendation {
                severity: Severity::Alert,
                rule: "single-name",
                message: format!(
                    "{} is {:.0}% of the portfolio — single-name concentration.",
                    p.ticker(),
                    p.weight_pct
                ),
                tickers: vec![p.ticker().to_owned()],
            });
        }
    }
}

fn trend_rules(a: &PortfolioAnalysis, out: &mut Vec<Recommendation>) {
    let stressed: f64 = a
        .by_territory()
        .iter()
        .filter(|(t, _)| matches!(t, TrendTerritory::Correction | TrendTerritory::BearMarket))
        .map(|(_, w)| w)
        .sum();
    if stressed > DRAWDOWN_TERRITORY_PCT {
        out.push(Recommendation {
            severity: Severity::Alert,
            rule: "drawdown-exposure",
            message: format!("{stressed:.0}% of value sits in Correction/BearMarket territory — review theses and risk budget."),
            tickers: vec![],
        });
    }
    for p in &a.positions {
        let c = &p.classification;
        if c.territory == TrendTerritory::BearMarket && c.archetype != StockArchetype::Income {
            out.push(Recommendation {
                severity: Severity::Notice,
                rule: "bear-market",
                message: format!(
                    "{} is in BearMarket territory ({:.1}% below 52w high) — revisit the investment thesis.",
                    p.ticker(),
                    c.metrics.drawdown_pct
                ),
                tickers: vec![p.ticker().to_owned()],
            });
        }
        if let Some(rsi) = c.metrics.rsi14.filter(|r| *r > RSI_OVERBOUGHT)
            && c.territory == TrendTerritory::Uptrend
        {
            out.push(Recommendation {
                severity: Severity::Info,
                rule: "extended",
                message: format!(
                    "{} RSI {rsi:.0} in Uptrend — extended; avoid chasing when researching adds.",
                    p.ticker()
                ),
                tickers: vec![p.ticker().to_owned()],
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use rust_decimal::prelude::FromPrimitive;

    use super::*;
    use crate::classifier::Metrics;
    use crate::domain::Instrument;

    fn pos(
        t: &str,
        ccy: Currency,
        a: StockArchetype,
        terr: TrendTerritory,
        value: i64,
        below50: bool,
    ) -> (Holding, Classification) {
        let close = Decimal::new(value, 0);
        let metrics = Metrics {
            last_close: Some(close),
            sma50: Some(if below50 {
                value as f64 * 1.1
            } else {
                value as f64 * 0.9
            }),
            drawdown_pct: 7.0,
            ..Metrics::default()
        };
        (
            Holding {
                instrument: Instrument {
                    ticker: t.into(),
                    name: t.into(),
                    currency: ccy,
                },
                quantity: Decimal::ONE,
            },
            Classification {
                ticker: t.into(),
                metrics,
                archetype: a,
                territory: terr,
            },
        )
    }

    fn build(rows: Vec<(Holding, Classification)>) -> PortfolioAnalysis {
        let (holdings, cls): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        value_portfolio(&Portfolio { holdings }, &cls).unwrap()
    }

    #[test]
    fn flags_growth_concentration_zero_defensive_and_income_pullback() {
        use StockArchetype::*;
        use TrendTerritory::*;
        let a = build(vec![
            pos("NVDA", Currency::Gbp, Growth, Uptrend, 720, false),
            pos("UKW", Currency::Gbp, Income, Pullback, 50, true),
            pos("NG.", Currency::Gbp, Income, Pullback, 30, true),
            pos("RIO", Currency::Gbp, Cyclical, Chop, 200, false),
        ]);
        let recs = recommend(&a);
        let msgs: Vec<&str> = recs.iter().map(|r| r.message.as_str()).collect();
        assert!(
            msgs.iter()
                .any(|m| m.starts_with("Portfolio is 72% Growth / 20% Cyclical / 8% Income")),
            "{msgs:#?}"
        );
        assert!(msgs.contains(&"Zero Defensive exposure detected — research low-volatility candidates (utilities, staples, baseload infrastructure)."));
        assert!(msgs.contains(&"UKW and NG. are in Pullback territory below 50-SMA — candidate for income rebalancing research."));
        // Sorted by severity.
        assert!(recs.windows(2).all(|w| w[0].severity <= w[1].severity));
    }

    #[test]
    fn income_above_sma50_not_flagged() {
        let a = build(vec![pos(
            "UKW",
            Currency::Gbp,
            StockArchetype::Income,
            TrendTerritory::Pullback,
            50,
            false,
        )]);
        assert!(recommend(&a).iter().all(|r| r.rule != "income-pullback"));
    }

    #[test]
    fn currency_and_single_name() {
        let a = build(vec![
            pos(
                "NVDA",
                Currency::Usd,
                StockArchetype::Growth,
                TrendTerritory::Uptrend,
                900,
                false,
            ),
            pos(
                "ASML",
                Currency::Eur,
                StockArchetype::Defensive,
                TrendTerritory::Uptrend,
                100,
                false,
            ),
        ]);
        let rules: Vec<_> = recommend(&a).into_iter().map(|r| r.rule).collect();
        assert!(rules.contains(&"currency-concentration"));
        assert!(rules.contains(&"single-name"));
    }

    #[test]
    fn missing_classification_errors() {
        let (h, _) = pos(
            "X",
            Currency::Gbp,
            StockArchetype::Growth,
            TrendTerritory::Chop,
            1,
            false,
        );
        assert!(value_portfolio(&Portfolio { holdings: vec![h] }, &[]).is_err());
    }

    #[test]
    fn join_formats() {
        assert_eq!(join_tickers(&["A".into()]), "A");
        assert_eq!(
            join_tickers(&["A".into(), "B".into(), "C".into()]),
            "A, B and C"
        );
    }

    proptest! {
        /// Archetype (and currency/territory) weights always sum to 100%.
        #[test]
        fn exposures_sum_to_100(
            rows in prop::collection::vec((1_u32..1_000_000, 0_usize..4, 0_usize..3, 0_usize..5), 1..20)
        ) {
            let rows: Vec<_> = rows.iter().enumerate().map(|(i, (v, a, c, t))| {
                let close = Decimal::from_u32(*v).unwrap() / Decimal::ONE_HUNDRED;
                let metrics = Metrics { last_close: Some(close), ..Metrics::default() };
                let ticker = format!("T{i}");
                (
                    Holding { instrument: Instrument { ticker: ticker.clone(), name: ticker.clone(), currency: Currency::ALL[*c] }, quantity: Decimal::ONE },
                    Classification { ticker, metrics, archetype: StockArchetype::ALL[*a], territory: TrendTerritory::ALL[*t] },
                )
            }).collect();
            let a = build(rows);
            for total in [
                a.by_archetype().iter().map(|x| x.1).sum::<f64>(),
                a.by_currency().iter().map(|x| x.1).sum::<f64>(),
                a.by_territory().iter().map(|x| x.1).sum::<f64>(),
            ] {
                prop_assert!((total - 100.0).abs() < 1e-6, "sum = {}", total);
            }
        }

        #[test]
        fn weights_non_negative(values in prop::collection::vec(0_i64..10_000_000, 0..30)) {
            let decs: Vec<Decimal> = values.iter().map(|v| Decimal::new(*v, 2)).collect();
            prop_assert!(weights_pct(&decs).iter().all(|w| *w >= 0.0));
        }
    }
}
