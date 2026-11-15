//! CLI demo for `rust_portfolio_intel` (read-only).
//!
//! ```text
//! rust_portfolio_intel [COMMAND] [OPTIONS]
//! ```

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode, Stdio};
use std::time::{Duration, Instant};

use chrono::{Days, Local, NaiveDate};
use rust_portfolio_intel::backfill::{BackfillConfig, plan, run_backfill};
use rust_portfolio_intel::domain::Portfolio;
use rust_portfolio_intel::pipeline::{Report, analyze_portfolio};
use rust_portfolio_intel::sample::{sample_portfolio, sample_source};
use rust_portfolio_intel::source::MarketDataSource;
use rust_portfolio_intel::storage::{DEFAULT_DB_PATH, DEFAULT_HISTORY_DAYS, Store, SyncReport};
use rust_portfolio_intel::throttle::{SystemClock, ThrottleConfig};
use rust_portfolio_intel::{Error, Result};

const USAGE: &str = "\
usage: rust_portfolio_intel [COMMAND] [OPTIONS]

commands:
  demo       (default) reset the cache, run a throttled cold backfill, an
             incremental sync and a same-day re-run, then print the report
  sync       bring the cache up to date. On a cold/far-behind cache this starts
             a slow background backfill and returns immediately (use --wait to
             run it in the foreground instead)
  backfill   slowly populate history in chunks (resumable; safe to Ctrl-C)
  status     show per-ticker cache coverage and any running backfill
  analyze    classify holdings from the local cache only
  recommend  print portfolio gap research notes from the local cache only

options:
  --db PATH           cache file (default data/market_cache.db)
  --as-of DATE        sync/analyse as of YYYY-MM-DD (default today)
  --chunk-days N      days requested per API call (default 90)
  --interval-ms MS    minimum gap between API requests (default 1000; demo 50)
  --background        (backfill) detach and log to <db>.backfill.log
  --wait              (sync) run a needed backfill in the foreground";

/// Default request spacing for real-world use.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
/// Request spacing used by `demo` so it finishes in seconds.
const DEMO_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Demo,
    Sync,
    Backfill,
    Status,
    Analyze,
    Recommend,
}

struct Args {
    command: Command,
    db: PathBuf,
    as_of: NaiveDate,
    chunk_days: u64,
    interval: Option<Duration>,
    background: bool,
    wait: bool,
}

impl Args {
    fn backfill_config(&self, default_interval: Duration) -> BackfillConfig {
        BackfillConfig {
            chunk_days: self.chunk_days,
            history_days: DEFAULT_HISTORY_DAYS,
            throttle: ThrottleConfig::default()
                .with_interval(self.interval.unwrap_or(default_interval)),
        }
    }

    fn lock_path(&self) -> PathBuf {
        sidecar(&self.db, ".backfill.lock")
    }

    fn log_path(&self) -> PathBuf {
        sidecar(&self.db, ".backfill.log")
    }
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args {
        command: Command::Demo,
        db: PathBuf::from(DEFAULT_DB_PATH),
        as_of: Local::now().date_naive(),
        chunk_days: BackfillConfig::default().chunk_days,
        interval: None,
        background: false,
        wait: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| it.next().ok_or_else(|| missing(flag));
        match arg.as_str() {
            "demo" => args.command = Command::Demo,
            "sync" => args.command = Command::Sync,
            "backfill" => args.command = Command::Backfill,
            "status" => args.command = Command::Status,
            "analyze" => args.command = Command::Analyze,
            "recommend" => args.command = Command::Recommend,
            "--db" => args.db = PathBuf::from(value("--db")?),
            "--as-of" => args.as_of = parse_flag("--as-of", &value("--as-of")?)?,
            "--chunk-days" => {
                args.chunk_days =
                    parse_flag::<u64>("--chunk-days", &value("--chunk-days")?)?.max(1);
            }
            "--interval-ms" => {
                let ms: u64 = parse_flag("--interval-ms", &value("--interval-ms")?)?;
                args.interval = Some(Duration::from_millis(ms));
            }
            "--background" => args.background = true,
            "--wait" => args.wait = true,
            "-h" | "--help" => return Ok(None),
            other => return Err(Error::InvalidInput(format!("unknown argument {other:?}"))),
        }
    }
    Ok(Some(args))
}

fn parse_flag<T: std::str::FromStr>(flag: &str, v: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    v.parse()
        .map_err(|e| Error::InvalidInput(format!("{flag} {v:?}: {e}")))
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
            eprintln!("error: {e}\n\nrun with --help for usage");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<()> {
    let portfolio = sample_portfolio();
    let source = sample_source();
    match args.command {
        Command::Demo => demo(args, &portfolio, &source),
        Command::Sync => sync(args, &portfolio, &source),
        Command::Backfill if args.background => start_background(args, &portfolio),
        Command::Backfill => backfill_foreground(args, &portfolio, &source),
        Command::Status => status(args, &portfolio),
        Command::Analyze | Command::Recommend => analyze(args, &portfolio),
    }
}

fn demo(args: &Args, portfolio: &Portfolio, source: &dyn MarketDataSource) -> Result<()> {
    if let Some(pid) = running_backfill(&args.lock_path()) {
        return Err(Error::InvalidInput(format!(
            "a backfill (pid {pid}) is using {}; use --db to demo elsewhere",
            args.db.display()
        )));
    }
    reset_cache(&args.db)?;
    let mut store = Store::open(&args.db)?;
    let cfg = args.backfill_config(DEMO_INTERVAL);
    let run1_date = args
        .as_of
        .checked_sub_days(Days::new(7))
        .ok_or_else(|| Error::InvalidInput("date out of range".into()))?;

    banner(&format!(
        "RUN 1 — cold cache: throttled backfill through {run1_date} ({}-day chunks, ≥{} ms between requests)",
        cfg.chunk_days,
        cfg.throttle.min_interval.as_millis()
    ));
    throttled_sync(
        &mut store,
        source,
        portfolio,
        run1_date,
        &cfg,
        Detail::PerRound,
    )?;

    banner(&format!("RUN 2 — incremental sync through {}", args.as_of));
    throttled_sync(
        &mut store,
        source,
        portfolio,
        args.as_of,
        &cfg,
        Detail::PerChunk,
    )?;

    banner("RUN 3 — re-run same day (expect zero source calls)");
    throttled_sync(
        &mut store,
        source,
        portfolio,
        args.as_of,
        &cfg,
        Detail::PerChunk,
    )?;

    let report = analyze_portfolio(&mut store, portfolio, args.as_of)?;
    print_positions(&report);
    print_recommendations(&report);
    Ok(())
}

fn sync(args: &Args, portfolio: &Portfolio, source: &dyn MarketDataSource) -> Result<()> {
    if let Some(pid) = running_backfill(&args.lock_path()) {
        println!(
            "A backfill is already running (pid {pid}); nothing to do. Progress: `status` or `tail -f {}`",
            args.log_path().display()
        );
        return Ok(());
    }
    let mut store = Store::open(&args.db)?;
    let cfg = args.backfill_config(DEFAULT_INTERVAL);
    let todo = plan(&store, portfolio, args.as_of, &cfg)?;
    if todo.needs_backfill() {
        println!(
            "Cache needs {} chunks of history (~{} at {} ms/request) — too much to fetch in one burst.",
            todo.remaining_chunks(),
            fmt_duration(todo.eta(cfg.throttle.min_interval)),
            cfg.throttle.min_interval.as_millis()
        );
        drop(store);
        return if args.wait {
            backfill_foreground(args, portfolio, source)
        } else {
            start_background(args, portfolio)
        };
    }
    banner(&format!("SYNC through {}", args.as_of));
    throttled_sync(
        &mut store,
        source,
        portfolio,
        args.as_of,
        &cfg,
        Detail::PerChunk,
    )
}

fn backfill_foreground(
    args: &Args,
    portfolio: &Portfolio,
    source: &dyn MarketDataSource,
) -> Result<()> {
    let cfg = args.backfill_config(DEFAULT_INTERVAL);
    let _lock = BackfillLock::acquire(&args.lock_path(), cfg.throttle.min_interval)?;
    let mut store = Store::open(&args.db)?;
    let todo = plan(&store, portfolio, args.as_of, &cfg)?;
    println!(
        "[{}] backfill through {} — {} tickers, {} chunks of ≤{} days, ≥{} ms between requests (eta ~{}).",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        args.as_of,
        todo.tickers.len(),
        todo.remaining_chunks(),
        cfg.chunk_days,
        cfg.throttle.min_interval.as_millis(),
        fmt_duration(todo.eta(cfg.throttle.min_interval))
    );
    println!("Safe to interrupt: each chunk commits with its watermark; re-running resumes.");
    let start = Instant::now();
    let summary = run_backfill(
        &mut store,
        source,
        portfolio,
        args.as_of,
        &cfg,
        &SystemClock::default(),
        &mut |p| {
            let r = p.report;
            let range = r
                .requested
                .map(|(f, t)| format!("{f} → {t}"))
                .unwrap_or_default();
            println!(
                "[{:>3}/{}] round {:<2} {:<5} {range}  {:>3} bars  cached {:>4}  eta {}",
                p.chunks_done,
                p.chunks_total,
                p.round,
                r.ticker,
                r.bars_fetched,
                r.cached_bars,
                fmt_duration(p.eta)
            );
        },
    )?;
    println!(
        "[{}] done: {} chunks, {} bars, {} requests ({} rate-limit retries) in {}.",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        summary.chunks,
        summary.bars,
        summary.requests,
        summary.retries,
        fmt_duration(start.elapsed())
    );
    Ok(())
}

/// Re-launch this binary as a detached `backfill` process logging to a file.
fn start_background(args: &Args, portfolio: &Portfolio) -> Result<()> {
    if let Some(pid) = running_backfill(&args.lock_path()) {
        println!("A backfill is already running (pid {pid}).");
        return Ok(());
    }
    let cfg = args.backfill_config(DEFAULT_INTERVAL);
    let todo = plan(&Store::open(&args.db)?, portfolio, args.as_of, &cfg)?;
    if todo.is_complete() {
        println!(
            "Cache is already complete through {}; no backfill needed.",
            args.as_of
        );
        return Ok(());
    }
    let log_path = args.log_path();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let mut cmd = Process::new(std::env::current_exe()?);
    cmd.arg("backfill")
        .arg("--db")
        .arg(&args.db)
        .arg("--as-of")
        .arg(args.as_of.to_string())
        .arg("--chunk-days")
        .arg(cfg.chunk_days.to_string())
        .arg("--interval-ms")
        .arg(cfg.throttle.min_interval.as_millis().to_string())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        // Own process group: Ctrl-C in this terminal won't stop the backfill.
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    // Intentionally not waited on: the CLI exits and the OS reparents the
    // child, which keeps running and reaps normally.
    #[allow(clippy::zombie_processes)]
    let pid = cmd.spawn()?.id();
    println!(
        "Started background backfill (pid {pid}): {} chunks, eta ~{}.",
        todo.remaining_chunks(),
        fmt_duration(todo.eta(cfg.throttle.min_interval))
    );
    println!(
        "  progress : rust_portfolio_intel status   (or: tail -f {})",
        log_path.display()
    );
    println!("  stop     : kill {pid}   (resumes from the last chunk on the next `sync`)");
    Ok(())
}

fn status(args: &Args, portfolio: &Portfolio) -> Result<()> {
    let store = Store::open(&args.db)?;
    let cfg = args.backfill_config(DEFAULT_INTERVAL);
    let todo = plan(&store, portfolio, args.as_of, &cfg)?;
    banner(&format!(
        "CACHE STATUS — {} as of {}",
        args.db.display(),
        args.as_of
    ));
    println!(
        "{:<6} {:<14} {:>8} {:>10}",
        "Ticker", "Synced through", "Bars", "Chunks left"
    );
    println!("{}", "─".repeat(42));
    for t in &todo.tickers {
        println!(
            "{:<6} {:<14} {:>8} {:>10}",
            t.ticker,
            t.watermark
                .map_or_else(|| "never".to_owned(), |d| d.to_string()),
            store.bar_count(&t.ticker)?,
            t.remaining_chunks
        );
    }
    println!();
    match lock_info(&args.lock_path()) {
        Some((pid, interval)) => println!(
            "Backfill RUNNING (pid {pid}) — ~{} left. Log: {}",
            fmt_duration(todo.eta(interval.unwrap_or(cfg.throttle.min_interval))),
            args.log_path().display()
        ),
        None if todo.is_complete() => println!("Cache complete. No backfill running."),
        None if todo.needs_backfill() => {
            println!("Cache incomplete and no backfill running — run `sync` to start one.");
        }
        None => println!("Cache slightly behind — `sync` will fetch the gap inline."),
    }
    Ok(())
}

fn analyze(args: &Args, portfolio: &Portfolio) -> Result<()> {
    if let Some(pid) = running_backfill(&args.lock_path()) {
        return Err(Error::InvalidInput(format!(
            "backfill in progress (pid {pid}); history is incomplete — check `status`"
        )));
    }
    let mut store = Store::open(&args.db)?;
    let todo = plan(
        &store,
        portfolio,
        args.as_of,
        &args.backfill_config(DEFAULT_INTERVAL),
    )?;
    if todo.needs_backfill() {
        return Err(Error::InvalidInput(format!(
            "cache at {} is missing history — run `sync` first (it backfills in the background)",
            args.db.display()
        )));
    }
    if !todo.is_complete() {
        println!(
            "note: cache is a few days behind {}; run `sync` for the latest bars.",
            args.as_of
        );
    }
    let report = analyze_portfolio(&mut store, portfolio, args.as_of)?;
    if args.command == Command::Analyze {
        print_positions(&report);
    } else {
        print_recommendations(&report);
    }
    Ok(())
}

// --- backfill lock ---------------------------------------------------------

/// PID lock file preventing two backfills from doubling the request rate.
struct BackfillLock {
    path: PathBuf,
}

impl BackfillLock {
    /// Create the lock, recording our PID and request interval (for `status`).
    fn acquire(path: &Path, interval: Duration) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut f) => {
                    writeln!(f, "{}\n{}", std::process::id(), interval.as_millis())?;
                    return Ok(Self {
                        path: path.to_owned(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some((pid, _)) = lock_info(path) {
                        return Err(Error::InvalidInput(format!(
                            "a backfill is already running (pid {pid}); see `status`"
                        )));
                    }
                    remove_if_exists(path)?; // stale lock from a killed run
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::InvalidInput(format!(
            "could not acquire lock {}",
            path.display()
        )))
    }
}

impl Drop for BackfillLock {
    fn drop(&mut self) {
        // Only remove the lock if it is still ours (a stale-lock takeover by
        // another process must not be undone). Best effort: a leftover lock is
        // detected as stale by the PID check.
        let ours = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| t.lines().next().and_then(|l| l.trim().parse::<u32>().ok()))
            == Some(std::process::id());
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// PID of a live backfill holding `lock`, if any.
fn running_backfill(lock: &Path) -> Option<u32> {
    lock_info(lock).map(|(pid, _)| pid)
}

/// PID and request interval of a live backfill holding `lock`, if any.
fn lock_info(lock: &Path) -> Option<(u32, Option<Duration>)> {
    let text = std::fs::read_to_string(lock).ok()?;
    let mut lines = text.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let interval = lines
        .next()
        .and_then(|l| l.trim().parse().ok())
        .map(Duration::from_millis);
    pid_alive(pid).then_some((pid, interval))
}

fn pid_alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        Path::new(&format!("/proc/{pid}")).exists()
    } else {
        // No cheap safe check elsewhere: assume alive; delete the lock to override.
        true
    }
}

// --- helpers ----------------------------------------------------------------

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

fn remove_if_exists(p: &Path) -> Result<()> {
    match std::fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Remove the demo cache (and SQLite side files) so Run 1 is genuinely cold.
fn reset_cache(db: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        remove_if_exists(&sidecar(db, suffix))?;
    }
    Ok(())
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{}.{}s", s, d.subsec_millis() / 100),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn banner(title: &str) {
    println!("\n{}\n{title}\n{}", "═".repeat(96), "═".repeat(96));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detail {
    PerChunk,
    PerRound,
}

/// Throttled sync used by `demo` and inline `sync`.
fn throttled_sync(
    store: &mut Store,
    source: &dyn MarketDataSource,
    portfolio: &Portfolio,
    today: NaiveDate,
    cfg: &BackfillConfig,
    detail: Detail,
) -> Result<()> {
    let start = Instant::now();
    let mut reports: Vec<(u64, SyncReport)> = Vec::new();
    let summary = run_backfill(
        store,
        source,
        portfolio,
        today,
        cfg,
        &SystemClock::default(),
        &mut |p| reports.push((p.round, p.report.clone())),
    )?;
    match detail {
        Detail::PerChunk => {
            println!(
                "{:<6} {:<25} {:>8} {:>6} {:>8}",
                "Ticker", "Requested range", "Fetched", "Divs", "Cached"
            );
            println!("{}", "─".repeat(57));
            for (_, r) in &reports {
                let range = r
                    .requested
                    .map(|(f, t)| format!("{f} → {t}"))
                    .unwrap_or_default();
                println!(
                    "{:<6} {:<25} {:>8} {:>6} {:>8}",
                    r.ticker, range, r.bars_fetched, r.dividends_fetched, r.cached_bars
                );
            }
            if reports.is_empty() {
                println!(
                    "(cache current for all {} tickers)",
                    portfolio.holdings.len()
                );
            }
        }
        Detail::PerRound => {
            println!(
                "{:<6} {:<25} {:>7} {:>8}",
                "Round", "Window", "Chunks", "Bars"
            );
            println!("{}", "─".repeat(49));
            for round in 1..=summary.rounds {
                let rs: Vec<&SyncReport> = reports
                    .iter()
                    .filter(|(n, _)| *n == round)
                    .map(|(_, r)| r)
                    .collect();
                let window = rs
                    .first()
                    .and_then(|r| r.requested)
                    .map(|(f, t)| format!("{f} → {t}"))
                    .unwrap_or_default();
                let bars: usize = rs.iter().map(|r| r.bars_fetched).sum();
                println!("{round:<6} {window:<25} {:>7} {bars:>8}", rs.len());
            }
        }
    }
    let cached: usize = portfolio
        .holdings
        .iter()
        .map(|h| store.bar_count(&h.instrument.ticker))
        .sum::<Result<usize>>()?;
    println!(
        "→ {} source requests ({} retries, {} throttled wait), {} bars fetched, {} served from cache — {}",
        summary.requests,
        summary.retries,
        fmt_duration(summary.waited),
        summary.bars,
        cached.saturating_sub(summary.bars),
        fmt_duration(start.elapsed())
    );
    Ok(())
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
