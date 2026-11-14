//! Sample multi-currency UK/US/EU thematic portfolio and matching synthetic
//! market profiles, so the demo runs offline out of the box.

use rust_decimal::Decimal;

use crate::domain::{Currency, Holding, Instrument, Portfolio};
use crate::source::{SyntheticProfile, SyntheticSource};

struct Spec {
    ticker: &'static str,
    name: &'static str,
    currency: Currency,
    quantity: i64,
    profile: SyntheticProfile,
}

const fn profile(
    start_price: f64,
    annual_drift: f64,
    daily_vol: f64,
    cycle_amplitude: f64,
    cycle_period_days: f64,
    dividend_yield: f64,
) -> SyntheticProfile {
    SyntheticProfile {
        start_price,
        annual_drift,
        daily_vol,
        cycle_amplitude,
        cycle_period_days,
        dividend_yield,
    }
}

fn specs() -> [Spec; 7] {
    [
        Spec {
            ticker: "NVDA",
            name: "NVIDIA Corp",
            currency: Currency::Usd,
            quantity: 100,
            profile: profile(4.0, 0.50, 0.030, 0.10, 330.0, 0.0),
        },
        Spec {
            ticker: "ASML",
            name: "ASML Holding",
            currency: Currency::Eur,
            quantity: 8,
            profile: profile(150.0, 0.22, 0.020, 0.06, 420.0, 0.009),
        },
        Spec {
            ticker: "MELI",
            name: "MercadoLibre",
            currency: Currency::Usd,
            quantity: 3,
            profile: profile(600.0, 0.20, 0.024, 0.08, 380.0, 0.0),
        },
        Spec {
            ticker: "RKLB",
            name: "Rocket Lab",
            currency: Currency::Usd,
            quantity: 60,
            profile: profile(10.0, 0.25, 0.040, 0.15, 300.0, 0.0),
        },
        Spec {
            ticker: "NG.",
            name: "National Grid",
            currency: Currency::Gbp,
            quantity: 500,
            profile: profile(8.5, 0.03, 0.010, 0.05, 365.0, 0.055),
        },
        Spec {
            ticker: "UKW",
            name: "Greencoat UK Wind",
            currency: Currency::Gbp,
            quantity: 3500,
            profile: profile(1.40, 0.0, 0.011, 0.06, 290.0, 0.065),
        },
        Spec {
            ticker: "RIO",
            name: "Rio Tinto",
            currency: Currency::Gbp,
            quantity: 120,
            profile: profile(45.0, 0.03, 0.018, 0.18, 400.0, 0.06),
        },
    ]
}

/// The sample portfolio (quantities only; weights are derived from prices).
pub fn sample_portfolio() -> Portfolio {
    Portfolio {
        holdings: specs()
            .into_iter()
            .map(|s| Holding {
                instrument: Instrument {
                    ticker: s.ticker.to_owned(),
                    name: s.name.to_owned(),
                    currency: s.currency,
                },
                quantity: Decimal::new(s.quantity, 0),
            })
            .collect(),
    }
}

/// A synthetic source with realistic profiles for the sample tickers.
pub fn sample_source() -> SyntheticSource {
    specs().into_iter().fold(SyntheticSource::new(), |src, s| {
        src.with_profile(s.ticker, s.profile)
    })
}
