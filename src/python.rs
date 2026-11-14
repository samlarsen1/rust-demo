//! Optional `PyO3` bindings (feature `extension-module`).
//!
//! ```python
//! import rust_portfolio_intel as rpi
//! report = rpi.analyze_sample_portfolio("data/market_cache.db")
//! for note in report["recommendations"]:
//!     print(note)
//! ```

use chrono::{Local, NaiveDate};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::pipeline::{analyze_portfolio, sync_portfolio};
use crate::sample::{sample_portfolio, sample_source};
use crate::storage::{DEFAULT_HISTORY_DAYS, Store};

impl From<crate::Error> for PyErr {
    fn from(e: crate::Error) -> Self {
        PyRuntimeError::new_err(e.to_string())
    }
}

/// Incrementally sync the sample portfolio into `db_path` (read-only market
/// data) and return positions plus research notes as plain Python objects.
#[pyfunction]
#[pyo3(signature = (db_path, as_of = None))]
fn analyze_sample_portfolio<'py>(
    py: Python<'py>,
    db_path: &str,
    as_of: Option<&str>,
) -> PyResult<Bound<'py, PyDict>> {
    let today = match as_of {
        Some(s) => s
            .parse::<NaiveDate>()
            .map_err(|e| PyValueError::new_err(e.to_string()))?,
        None => Local::now().date_naive(),
    };
    let portfolio = sample_portfolio();
    let mut store = Store::open(std::path::Path::new(db_path))?;
    let syncs = sync_portfolio(
        &mut store,
        &sample_source(),
        &portfolio,
        today,
        DEFAULT_HISTORY_DAYS,
    )?;
    let report = analyze_portfolio(&mut store, &portfolio, today)?;

    let positions = report
        .analysis
        .positions
        .iter()
        .map(|p| {
            let m = &p.classification.metrics;
            let d = PyDict::new(py);
            d.set_item("ticker", p.ticker())?;
            d.set_item("currency", p.holding.instrument.currency.code())?;
            d.set_item("archetype", p.classification.archetype.to_string())?;
            d.set_item("territory", p.classification.territory.to_string())?;
            d.set_item("weight_pct", p.weight_pct)?;
            d.set_item("value_gbp", p.value_gbp.to_string())?;
            d.set_item("drawdown_pct", m.drawdown_pct)?;
            d.set_item("dividend_yield_pct", m.dividend_yield_pct)?;
            d.set_item("rsi14", m.rsi14)?;
            Ok(d)
        })
        .collect::<PyResult<Vec<_>>>()?;
    let notes: Vec<String> = report
        .recommendations
        .iter()
        .map(|r| format!("[{}] {}", r.severity, r.message))
        .collect();
    let fetched: usize = syncs.iter().map(|s| s.bars_fetched).sum();

    let out = PyDict::new(py);
    out.set_item("positions", positions)?;
    out.set_item("recommendations", notes)?;
    out.set_item("bars_fetched", fetched)?;
    Ok(out)
}

/// Python module definition.
#[pymodule]
#[pyo3(name = "rust_portfolio_intel")]
fn py_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(analyze_sample_portfolio, m)?)?;
    Ok(())
}
