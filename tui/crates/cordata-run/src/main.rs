use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cordata_run::cli::{self, RunArgs};

#[derive(Parser)]
#[command(name = "cordata-run", about = "run, inspect and resume orchestrated coding tasks (native)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// run an orchestrated coding task
    Run {
        /// task goal / instructions
        goal: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// comma-separated worker families (claude_code,antigravity)
        #[arg(long)]
        workers: Option<String>,
        #[arg(long, value_parser = ["off", "shadow", "live"])]
        dispatch: Option<String>,
        #[arg(long)]
        allow_direct: bool,
        #[arg(long)]
        max_usd: Option<f64>,
        #[arg(long)]
        max_rounds: Option<i64>,
        #[arg(long)]
        max_parallel: Option<usize>,
        #[arg(long)]
        no_harden: bool,
        #[arg(long)]
        no_conformance: bool,
        #[arg(long)]
        no_tier2: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
    },
    /// pool headroom, recent runs and handoffs
    Status {
        #[arg(long)]
        json: bool,
    },
    /// show one run's plan, history, and audit
    Show {
        run_id: String,
        #[arg(long)]
        json: bool,
    },
    /// continue a paused or interrupted run
    Resume {
        run_id: String,
        #[arg(long)]
        allow_direct: bool,
        #[arg(long)]
        max_usd: Option<f64>,
        #[arg(long)]
        json: bool,
    },
}

// The machine's futures borrow the SQLite-backed ledger, so they are not `Send`: a current-thread runtime.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let code = match Cli::parse().command {
        Command::Run { goal, cwd, workers, dispatch, allow_direct, max_usd, max_rounds, max_parallel, no_harden, no_conformance, no_tier2, json, run_id, session_id } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            cli::run(RunArgs { goal, cwd, workers, dispatch, allow_direct, max_usd, max_rounds, max_parallel, no_harden, no_conformance, no_tier2, json, run_id, session_id }).await
        }
        Command::Status { json } => cli::status_cmd(json),
        Command::Show { run_id, json } => cli::show_cmd(&run_id, json),
        Command::Resume { run_id, allow_direct, max_usd, json } => cli::resume(&run_id, allow_direct, max_usd, json).await,
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
