//! Scriptable concurrent stress runner for the sierralab-agents ACP backend.
//!
//!     acp-stress scenarios.json --concurrency 5 --out results.jsonl

mod report;
mod runner;
mod scenario;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use acp_core::BackendConfig;
use anyhow::Result;
use clap::Parser;
use futures::StreamExt;
use tokio::sync::watch;

use crate::report::Sink;
use crate::runner::RunConfig;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Scenario file (JSON).
    scenarios: PathBuf,

    /// Sessions running at once — each one is its own backend process
    /// (and loads its own copy of the backend's models), so size this to your RAM.
    #[arg(short, long, default_value_t = 4)]
    concurrency: usize,

    /// Write one JSON line per prompt-turn here.
    #[arg(short, long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// Seconds to wait for a backend to start and open its session.
    #[arg(long, default_value_t = 180)]
    startup_timeout: u64,

    /// Seconds a single turn may take before it is cancelled and reported.
    #[arg(long, default_value_t = 300)]
    turn_timeout: u64,

    /// Repo root or backend/ dir [env: ACP_TUI_BACKEND_ROOT; default: auto-discover].
    #[arg(long)]
    backend_root: Option<PathBuf>,

    /// Run a different ACP server script with the backend's python (testing).
    #[arg(long, hide = true)]
    server_script: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let scenarios = scenario::load(&args.scenarios)?;

    let mut backend = BackendConfig::discover(args.backend_root.as_deref())?;
    if let Some(script) = args.server_script {
        backend = backend.with_server_script(script);
    }
    backend.validate()?;

    let config = Arc::new(RunConfig {
        backend,
        startup_timeout: Duration::from_secs(args.startup_timeout),
        turn_timeout: Duration::from_secs(args.turn_timeout),
    });
    let sink = Arc::new(Sink::create(args.out.as_deref())?);
    let (abort_tx, abort_rx) = watch::channel(false);

    // Ctrl+C: stop launching, let every live session shut its backend down
    // cleanly (so no python processes are left behind), then report what ran.
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("interrupt — shutting sessions down…");
            let _ = abort_tx.send(true);
        }
    });

    let total = scenarios.len();
    eprintln!(
        "running {total} session(s), concurrency {}",
        args.concurrency.max(1)
    );
    let outcomes: Vec<_> = futures::stream::iter(scenarios)
        .map(|scenario| {
            let (config, sink, abort) = (config.clone(), sink.clone(), abort_rx.clone());
            async move {
                let outcome = runner::run_scenario(&config, scenario, abort, &sink).await;
                let unhealthy = outcome.turns.iter().filter(|turn| !turn.is_ok()).count();
                let status = match (&outcome.error, unhealthy) {
                    (Some(error), _) => format!("ERROR: {error}"),
                    (None, 0) => "ok".to_owned(),
                    (None, n) => format!("{n} unhealthy turn(s)"),
                };
                eprintln!("  finished {} — {status}", outcome.name);
                outcome
            }
        })
        .buffer_unordered(args.concurrency.max(1))
        .collect()
        .await;

    let summary = report::summarize(&outcomes);
    println!("{}", summary.text);
    if let Some(out) = &args.out {
        println!("results: {}", out.display());
    }
    if summary.failed {
        std::process::exit(1);
    }
    Ok(())
}
