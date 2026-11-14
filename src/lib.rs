//! # `rust_portfolio_intel`
//!
//! Strictly **read-only** quantitative portfolio intelligence:
//!
//! * [`storage`] — incremental, watermark-driven SQLite market data cache.
//! * [`source`] — the read-only [`source::MarketDataSource`] seam plus an
//!   offline deterministic generator.
//! * [`classifier`] — SMA/RSI/volatility/drawdown/CAGR/yield metrics, trend
//!   territory and style archetype classification.
//! * [`recommender`] — exposure analysis and explainable research notes.
//!
//! This crate contains **no** order-execution, trade-placement or broker
//! mutation logic, by design.

pub mod classifier;
pub mod domain;
pub mod error;
pub mod pipeline;
pub mod recommender;
pub mod sample;
pub mod source;
pub mod storage;

#[cfg(feature = "extension-module")]
mod python;

pub use error::{Error, Result};
