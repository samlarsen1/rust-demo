//! CLI demo for `rust_portfolio_intel` (read-only).
//!
//! ```text
//! rust_portfolio_intel [demo|sync|analyze|recommend] [--db PATH] [--as-of YYYY-MM-DD]
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use chrono::{Days, Local, NaiveDate};
use rust_portfolio_intel::pipeline::{Report, analyze_portfolio, sync_portfolio};
use rust_portfolio_intel::sample::{sample_portfolio, sample_source};
use rust_portfolio_intel::storage::{DEFAULT_DB_PATH, DEFAULT_HISTORY_DAYS, Store, SyncReport};
use rust_portfolio_intel::{Error, Result};

const USAGE: &str = "\
usage: rust_portfolio_intel [COMMAND] [--db PATH] [--as-of YYYY-MM-DD]

commands:
  demo       (default) reset the cache, run a cold sync then an incremental
             sync, and print the full report
  sync       incrementally sync the sample portfolio up to --as-of (today)
  analyze    classify holdings from the local cache only
  recommend  print portfolio gap research notes from the local cache only";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Demo,
    Sync,
    Analyze,
    Recommend,
}

struct Args {
    command: Command,
    db: PathBuf,
    as_of: NaiveDate,
}

fn parse_args() -> Result<Option<Args>> {
    let mut command = Command::Demo;
    let mut db = PathBuf::from(DEFAULT_DB_PATH);
    let mut as_of = Local::now().date_naive();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "demo" => command = Command::Demo,
            "sync" => command = Command::Sync,
            "analyze" => command = Command::Analyze,
            "recommend" => command = Command::Recommend,
            "--db" => {
                db = it
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| missing("--db"))?;
            }
            "--as-of" => {
                let v = it.next().ok_or_else(|| missing("--as-of"))?;
                as_of = v
                    .parse()
                    .map_err(|e| Error::InvalidInput(format!("--as-of {v:?}: {e}")))?;
            }
            "-h" | "--help" => return Ok(None),
            other => return Err(Error::InvalidInput(format!("unknown argument {other:?}"))),
        }
    }
    Ok(Some(Args { command, db, as_of }))
}

fn missing(flag: &str) -> Error {
    Error::InvalidInput(format!("{flag} requires a value"))
}

fn main() -> ExitCode {
    let result = parse_args().and_then(|args| {
        if let Some(a) = args {
            run(&a)
        } else {
            println!("{USAGE}");
            Ok(())
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<()> {
    let portfolio = sample_portfolio();
    let source = sample_source();
    match args.command {
        Command::Demo => {
            reset_cache(&args.db)?;
            let mut store = Store::open(&args.db)?;
            let run1_date = args
                .as_of
                .checked_sub_days(Days::new(7))
                .ok_or_else(|| Error::InvalidInput("date out of range".into()))?;

            banner(&format!("RUN 1 — cold cache, sync through {run1_date}"));
            timed_sync(&mut store, &source, &portfolio, run1_date)?;

            banner(&format!("RUN 2 — incremental sync through {}", args.as_of));
            timed_sync(&mut store, &source, &portfolio, args.as_of)?;

            banner("RUN 3 — re-run same day (expect zero source calls)");
            timed_sync(&mut store, &source, &portfolio, args.as_of)?;

            let report = analyze_portfolio(&mut store, &portfolio, args.as_of)?;
            print_positions(&report);
            print_recommendations(&report);
        }
        Command::Sync => {
            let mut store = Store::open(&args.db)?;
            banner(&format!("SYNC through {}", args.as_of));
            timed_sync(&mut store, &source, &portfolio, args.as_of)?;
        }
        Command::Analyze | Command::Recommend => {
            let mut store = Store::open(&args.db)?;
            if portfolio
                .holdings
                .iter()
                .any(|h| store.bar_count(&h.instrument.ticker).is_ok_and(|n| n == 0))
            {
                return Err(Error::InvalidInput(format!(
                    "cache at {} is empty — run `sync` first",
                    args.db.display()
                )));
            }
            let report = analyze_portfolio(&mut store, &portfolio, args.as_of)?;
            if args.command == Command::Analyze {
                print_positions(&report);
            } else {
                print_recommendations(&report);
            }
        }
    }
    Ok(())
}

/// Remove the demo cache (and SQLite side files) so Run 1 is genuinely cold.
fn reset_cache(db: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = db.as_os_str().to_owned();
        p.push(suffix);
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn banner(title: &str) {
    println!("\n{}\n{title}\n{}", "═".repeat(96), "═".repeat(96));
}

fn timed_sync(
    store: &mut Store,
    source: &rust_portfolio_intel::source::SyntheticSource,
    portfolio: &rust_portfolio_intel::domain::Portfolio,
    today: NaiveDate,
) -> Result<Vec<SyncReport>> {
    let start = Instant::now();
    let reports = sync_portfolio(store, source, portfolio, today, DEFAULT_HISTORY_DAYS)?;
    println!(
        "{:<6} {:<25} {:>8} {:>6} {:>8}",
        "Ticker", "Requested range", "Fetched", "Divs", "Cached"
    );
    println!("{}", "─".repeat(57));
    for r in &reports {
        let range = r.requested.map_or_else(
            || "— (cache current)".to_owned(),
            |(f, t)| format!("{f} → {t}"),
        );
        println!(
            "{:<6} {:<25} {:>8} {:>6} {:>8}",
            r.ticker, range, r.bars_fetched, r.dividends_fetched, r.cached_bars
        );
    }
    let fetched: usize = reports.iter().map(|r| r.bars_fetched).sum();
    let cached: usize = reports.iter().map(|r| r.cached_bars).sum();
    let calls = reports.iter().filter(|r| r.requested.is_some()).count();
    println!(
        "→ {calls} source requests, {fetched} bars fetched, {} bars served from cache ({:.1} ms)",
        cached.saturating_sub(fetched),
        start.elapsed().as_secs_f64() * 1000.0
    );
    Ok(reports)
}

fn opt(v: Option<f64>, prec: usize) -> String {
    v.map_or_else(|| "n/a".to_owned(), |x| format!("{x:.prec$}"))
}

fn print_positions(report: &Report) {
    let a = &report.analysis;
    banner(&format!(
        "PORTFOLIO ANALYSIS — total £{} (GBP base, static FX)",
        a.total_value_gbp
    ));
    println!(
        "{:<6} {:<4} {:<10} {:<11} {:>11} {:>7} {:>7} {:>6} {:>5} {:>6} {:>7}",
        "Ticker",
        "Ccy",
        "Archetype",
        "Territory",
        "Close",
        "Wgt%",
        "DD%",
        "Yld%",
        "RSI",
        "Vol%",
        "CAGR%"
    );
    println!("{}", "─".repeat(96));
    for p in &a.positions {
        let m = &p.classification.metrics;
        let close = m
            .last_close
            .map_or_else(|| "n/a".to_owned(), |c| c.round_dp(2).to_string());
        let sma_flag = if m.below_sma200() {
            "↓200"
        } else if m.below_sma50() {
            "↓50"
        } else {
            ""
        };
        println!(
            "{:<6} {:<4} {:<10} {:<11} {:>11} {:>7.1} {:>7.1} {:>6.2} {:>5} {:>6.1} {:>7} {sma_flag}",
            p.ticker(),
            p.holding.instrument.currency.code(),
            p.classification.archetype.to_string(),
            p.classification.territory.to_string(),
            close,
            p.weight_pct,
            m.drawdown_pct,
            m.dividend_yield_pct,
            opt(m.rsi14, 0),
            m.annual_vol_pct,
            opt(m.cagr_pct, 1),
        );
    }
    let fmt_exposure = |items: Vec<(String, f64)>| {
        items
            .into_iter()
            .filter(|(_, w)| *w > 0.0)
            .map(|(k, w)| format!("{k} {w:.1}%"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    println!(
        "\nBy currency : {}",
        fmt_exposure(
            a.by_currency()
                .into_iter()
                .map(|(k, w)| (k.to_string(), w))
                .collect()
        )
    );
    println!(
        "By archetype: {}",
        fmt_exposure(
            a.by_archetype()
                .into_iter()
                .map(|(k, w)| (k.to_string(), w))
                .collect()
        )
    );
    println!(
        "By territory: {}",
        fmt_exposure(
            a.by_territory()
                .into_iter()
                .map(|(k, w)| (k.to_string(), w))
                .collect()
        )
    );
    println!("(↓50 / ↓200 = close below 50-/200-day SMA)");
}

fn print_recommendations(report: &Report) {
    banner("PORTFOLIO GAP RESEARCH NOTES (read-only — no orders are ever placed)");
    for r in &report.recommendations {
        println!(
            "[{:<6}] {:<22} {}",
            r.severity.to_string(),
            r.rule,
            r.message
        );
    }
}
