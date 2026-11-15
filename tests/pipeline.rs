//! End-to-end: on-disk SQLite cache in a temp dir → incremental sync →
//! analysis → recommendations → reopen and verify persistence.

// Integration tests may panic on failure; the zero-panic policy is for `src/`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chrono::NaiveDate;
use rust_portfolio_intel::domain::StockArchetype;
use rust_portfolio_intel::pipeline::{analyze_portfolio, classify_portfolio, sync_portfolio};
use rust_portfolio_intel::sample::{sample_portfolio, sample_source};
use rust_portfolio_intel::storage::{DEFAULT_HISTORY_DAYS, Store};

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

#[test]
fn full_incremental_pipeline_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("nested/market_cache.db");
    let portfolio = sample_portfolio();
    let source = sample_source();

    // Run 1: cold cache.
    let run1 = {
        let mut store = Store::open(&db).unwrap();
        sync_portfolio(
            &mut store,
            &source,
            &portfolio,
            d(2025, 6, 6),
            DEFAULT_HISTORY_DAYS,
        )
        .unwrap()
    };
    assert!(db.exists());
    assert_eq!(run1.len(), portfolio.holdings.len());
    assert!(run1.iter().all(|r| r.bars_fetched > 500));
    let cold_total: usize = run1.iter().map(|r| r.cached_bars).sum();

    // Run 2: new process (reopen), one trading week later → only 5 new bars each.
    let mut store = Store::open(&db).unwrap();
    let run2 = sync_portfolio(
        &mut store,
        &source,
        &portfolio,
        d(2025, 6, 13),
        DEFAULT_HISTORY_DAYS,
    )
    .unwrap();
    for r in &run2 {
        assert_eq!(r.requested, Some((d(2025, 6, 7), d(2025, 6, 13))), "{r:?}");
        assert_eq!(r.bars_fetched, 5);
    }
    let warm_total: usize = run2.iter().map(|r| r.cached_bars).sum();
    assert_eq!(warm_total, cold_total + 5 * portfolio.holdings.len());

    // Run 3: same day → no source requests at all.
    let run3 = sync_portfolio(
        &mut store,
        &source,
        &portfolio,
        d(2025, 6, 13),
        DEFAULT_HISTORY_DAYS,
    )
    .unwrap();
    assert!(
        run3.iter()
            .all(|r| r.requested.is_none() && r.bars_fetched == 0)
    );

    // Analysis from cache.
    let report = analyze_portfolio(&mut store, &portfolio, d(2025, 6, 13)).unwrap();
    assert_eq!(report.analysis.positions.len(), portfolio.holdings.len());
    let weight_sum: f64 = report.analysis.by_archetype().iter().map(|x| x.1).sum();
    assert!((weight_sum - 100.0).abs() < 1e-6);
    assert_ne!(report.recommendations.len(), 0);
    for p in &report.analysis.positions {
        let m = &p.classification.metrics;
        assert!((0.0..=100.0).contains(&m.drawdown_pct));
        assert!(m.rsi14.is_some_and(|r| (0.0..=100.0).contains(&r)));
        assert!(m.sma200.is_some(), "2y history must yield a 200-SMA");
    }
    // Income names pay dividends; NVDA does not.
    let by_ticker = |t: &str| {
        report
            .analysis
            .positions
            .iter()
            .find(|p| p.ticker() == t)
            .unwrap()
    };
    assert!(by_ticker("UKW").classification.metrics.dividend_yield_pct > 4.0);
    assert_eq!(
        by_ticker("UKW").classification.archetype,
        StockArchetype::Income
    );
    assert!(
        by_ticker("NVDA")
            .classification
            .metrics
            .dividend_yield_pct
            .abs()
            < 1e-12
    );
    assert_eq!(
        by_ticker("NVDA").classification.archetype,
        StockArchetype::Growth
    );
    assert_eq!(
        store.snapshot_len(d(2025, 6, 13)).unwrap(),
        portfolio.holdings.len()
    );

    // Reopen: classification from persisted data is identical.
    drop(store);
    let reopened = Store::open(&db).unwrap();
    let again = classify_portfolio(&reopened, &portfolio).unwrap();
    let before: Vec<_> = report
        .analysis
        .positions
        .iter()
        .map(|p| p.classification.clone())
        .collect();
    assert_eq!(again, before);
}

#[test]
fn throttled_backfill_on_disk_matches_oneshot_and_resumes_after_reopen() {
    use rust_portfolio_intel::backfill::{BackfillConfig, plan, run_backfill};
    use rust_portfolio_intel::throttle::{ManualClock, ThrottleConfig};
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.db");
    let portfolio = sample_portfolio();
    let source = sample_source();
    let today = d(2025, 6, 13);
    let cfg = BackfillConfig {
        chunk_days: 120,
        throttle: ThrottleConfig::default().with_interval(Duration::from_secs(3)),
        ..BackfillConfig::default()
    };

    // Process 1: backfill only part of the way (through an earlier date), exit.
    let partial = d(2024, 6, 1);
    {
        let mut store = Store::open(&db).unwrap();
        run_backfill(
            &mut store,
            &source,
            &portfolio,
            partial,
            &cfg,
            &ManualClock::new(),
            &mut |_| {},
        )
        .unwrap();
    }

    // Process 2: reopen and continue to `today`; it must start after `partial`.
    let mut store = Store::open(&db).unwrap();
    let mut first_requests = vec![];
    let clock = ManualClock::new();
    let s = run_backfill(
        &mut store,
        &source,
        &portfolio,
        today,
        &cfg,
        &clock,
        &mut |p| {
            if p.round == 1 {
                first_requests.push(p.report.requested.unwrap().0);
            }
        },
    )
    .unwrap();
    assert!(
        first_requests.iter().all(|f| *f == d(2024, 6, 2)),
        "{first_requests:?}"
    );
    assert!(plan(&store, &portfolio, today, &cfg).unwrap().is_complete());

    // Throttle: with an instant source, total wait = (requests − 1) × interval.
    let waited: Duration = clock.sleeps().iter().sum();
    assert_eq!(
        waited,
        Duration::from_secs(3) * u32::try_from(s.requests - 1).unwrap()
    );

    // Compare with an unthrottled one-shot sync.
    let mut reference = Store::open_in_memory().unwrap();
    sync_portfolio(
        &mut reference,
        &source,
        &portfolio,
        today,
        DEFAULT_HISTORY_DAYS,
    )
    .unwrap();
    for h in &portfolio.holdings {
        let t = &h.instrument.ticker;
        // Process 1 started 2 years before `partial`, so the cache holds *more*
        // history than a fresh one-shot sync; over the shared range it is identical.
        let want = reference.bars(t).unwrap();
        let start = want.first().unwrap().date;
        let all = store.bars(t).unwrap();
        assert!(all.first().unwrap().date < start);
        let got: Vec<_> = all.into_iter().filter(|b| b.date >= start).collect();
        assert_eq!(got, want, "{t}");
    }
}
