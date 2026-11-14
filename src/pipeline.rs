//! End-to-end read-only pipeline: sync → classify → value → recommend.

use chrono::NaiveDate;

use crate::classifier::{Classification, analyze};
use crate::domain::Portfolio;
use crate::error::Result;
use crate::recommender::{PortfolioAnalysis, Recommendation, recommend, value_portfolio};
use crate::source::MarketDataSource;
use crate::storage::{SnapshotRow, Store, SyncReport};

/// Incrementally sync every holding up to `today`.
///
/// # Errors
/// Propagates source and storage errors.
pub fn sync_portfolio(
    store: &mut Store,
    source: &dyn MarketDataSource,
    portfolio: &Portfolio,
    today: NaiveDate,
    history_days: u64,
) -> Result<Vec<SyncReport>> {
    portfolio
        .holdings
        .iter()
        .map(|h| store.sync_ticker(source, &h.instrument, today, history_days))
        .collect()
}

/// Classify every holding from the local cache only (no network).
///
/// # Errors
/// Propagates storage errors.
pub fn classify_portfolio(store: &Store, portfolio: &Portfolio) -> Result<Vec<Classification>> {
    portfolio
        .holdings
        .iter()
        .map(|h| {
            let t = &h.instrument.ticker;
            Ok(analyze(t, &store.bars(t)?, &store.dividends(t)?))
        })
        .collect()
}

/// Full analysis result.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// Valued and classified positions.
    pub analysis: PortfolioAnalysis,
    /// Research notes.
    pub recommendations: Vec<Recommendation>,
}

/// Classify, value and generate recommendations; persists a snapshot for
/// `snapshot_date`.
///
/// # Errors
/// Propagates storage and valuation errors.
pub fn analyze_portfolio(
    store: &mut Store,
    portfolio: &Portfolio,
    snapshot_date: NaiveDate,
) -> Result<Report> {
    let classifications = classify_portfolio(store, portfolio)?;
    let analysis = value_portfolio(portfolio, &classifications)?;
    let rows: Vec<SnapshotRow> = analysis
        .positions
        .iter()
        .map(|p| SnapshotRow {
            ticker: p.ticker().to_owned(),
            quantity: p.holding.quantity,
            close: p.classification.metrics.last_close.unwrap_or_default(),
            currency: p.holding.instrument.currency,
            value_gbp: p.value_gbp,
            weight_pct: p.weight_pct,
        })
        .collect();
    store.write_snapshot(snapshot_date, &rows)?;
    let recommendations = recommend(&analysis);
    Ok(Report {
        analysis,
        recommendations,
    })
}
