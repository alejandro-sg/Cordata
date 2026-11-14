use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cordata_run::cli::{self, RunArgs};
use cordata_run::detach;

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
        #[arg(long, value_parser = ["off", "shadow", "live", "auto"])]
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
        /// stop after planning and show what the plan expects to build; `cordata-run resume <id>` then builds it
        #[arg(long)]
        review_plan: bool,
        /// skip the research brief before planning
        #[arg(long)]
        no_research: bool,
        /// write a research brief before planning (the default)
        #[arg(long)]
        research: bool,
        #[arg(long)]
        no_conformance: bool,
        #[arg(long)]
        no_tier2: bool,
        /// skip the build check after each step
        #[arg(long)]
        no_step_check: bool,
        /// skip the model screen of the audit's commands (the pattern guard still runs)
        #[arg(long)]
        no_screen: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, value_parser = run_id_arg)]
        run_id: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
        /// how changes are landed: "all" directly into working tree, or "review" to finish in to_review state
        #[arg(long)]
        land: Option<String>,
        /// detach run into background session (setsid) so it survives terminal closure
        #[arg(long)]
        detach: bool,
    },
    /// pool headroom, recent runs and handoffs
    Status {
        #[arg(long)]
        json: bool,
    },
    /// show one run's plan, history, audit, or a step's live tape
    Show {
        #[arg(value_parser = run_id_arg)]
        run_id: String,
        #[arg(long)]
        json: bool,
        /// step id to inspect its live activity / tape
        #[arg(long)]
        step: Option<String>,
        /// attempt number (defaults to latest)
        #[arg(long)]
        attempt: Option<usize>,
        /// tail / follow the step tape live
        #[arg(long)]
        follow: bool,
        /// show timeline waterfall of GenAI spans
        #[arg(long)]
        timeline: bool,
    },
    /// continue a paused or interrupted run
    Resume {
        #[arg(value_parser = run_id_arg)]
        run_id: String,
        /// worker families to use (comma-separated; defaults to orchestrator.workers)
        #[arg(long, value_name = "FAMILIES")]
        workers: Option<String>,
        #[arg(long)]
        allow_direct: bool,
        #[arg(long)]
        max_usd: Option<f64>,
        /// continue even when the settings now turn off a safety guard the run was started with
        #[arg(long)]
        allow_weaker_guards: bool,
        /// words for the next step, added to its task (what a person says when they hold a handoff)
        #[arg(long, value_name = "TEXT")]
        direction: Option<String>,
        /// run this step again (even a completed one) with its worker and acceptance checks; --direction then goes on it
        #[arg(long, value_name = "STEP_ID")]
        redo_step: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// follow a detached run's output until it stops
    Attach {
        #[arg(value_parser = run_id_arg)]
        run_id: String,
    },
    /// export a run as OpenTelemetry GenAI spans
    Export {
        #[arg(value_parser = run_id_arg)]
        run_id: String,
        #[arg(long)]
        otlp: bool,
    },
    /// recursive self-improvement cycle across Iron Gates
    Evolve {
        /// target subsystem to optimize (repomap, economizer, guard, custom)
        #[arg(default_value = "repomap")]
        target: String,
        /// minimum fitness improvement threshold to promote
        #[arg(long, default_value_t = 0.001, allow_hyphen_values = true)]
        threshold: f64,
        /// run gates without harvesting changes
        #[arg(long)]
        dry_run: bool,
        /// shell command that proposes a change by editing files in its cwd (the CoW sandbox, never the project)
        #[arg(long)]
        mutate: Option<String>,
        /// let a model propose the change (runs the orchestrator on the sandbox); optional direction text
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        propose: Option<String>,
        /// dollar cap for the model proposal
        #[arg(long, default_value_t = 1.0)]
        propose_usd: f64,
        /// worker families the model proposal may use, comma-separated (default: those enabled in settings)
        #[arg(long, value_name = "FAMILIES")]
        propose_workers: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

// The machine's futures borrow the SQLite-backed ledger, so they are not `Send`: a current-thread runtime.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    acp_core::policy::enforce_cordata_home();
    let code = match Cli::parse().command {
        Command::Run { goal, cwd, workers, dispatch, allow_direct, max_usd, max_rounds, max_parallel, no_harden, review_plan, no_research, research, no_conformance, no_tier2, no_step_check, no_screen, json, run_id, session_id, land, detach } => {
            if detach {
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cordata-run"));
                    let mut cmd = std::process::Command::new(exe);
                    for arg in std::env::args().skip(1) {
                        if arg != "--detach" {
                            cmd.arg(arg);
                        }
                    }
                    // The run needs a known id before it starts, or the person cannot find it again.
                    let id = run_id.clone().unwrap_or_else(|| format!("run-{}", cordata_orchestrator::machine::short_id()));
                    if run_id.is_none() {
                        cmd.args(["--run-id", &id]);
                    }
                    let home = cli::cordata_home(&std::env::vars().collect());
                    let log = detach::log_path(&home, &id);
                    let log_file = log.parent().and_then(|dir| std::fs::create_dir_all(dir).ok()).and_then(|()| std::fs::File::create(&log).ok());
                    unsafe {
                        cmd.pre_exec(|| {
                            libc::setsid();
                            Ok(())
                        });
                    }
                    cmd.env(detach::DETACHED_ENV, "1");
                    cmd.stdin(std::process::Stdio::null());
                    match log_file.and_then(|f| f.try_clone().ok().map(|g| (f, g))) {
                        Some((out, err)) => {
                            cmd.stdout(out);
                            cmd.stderr(err);
                        }
                        None => {
                            cmd.stdout(std::process::Stdio::null());
                            cmd.stderr(std::process::Stdio::null());
                        }
                    }
                    match cmd.spawn() {
                        Ok(child) => {
                            detach::keep_awake(child.id());
                            println!("Detached run {id} started with PID {}", child.id());
                            println!("  cordata attach {id}   watch it; a notification says when it finishes or needs you");
                            return ExitCode::from(0);
                        }
                        Err(e) => {
                            eprintln!("Failed to spawn detached run: {e}");
                            return ExitCode::from(1);
                        }
                    }
                }
            }
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            cli::run(RunArgs { goal, cwd, workers, dispatch, allow_direct, max_usd, max_rounds, max_parallel, no_harden, review_plan, no_research, research, no_conformance, no_tier2, no_step_check, no_screen, json, run_id, session_id, land, detach }).await
        }
        Command::Status { json } => cli::status_cmd(json),
        Command::Show { run_id, json, step, attempt, follow, timeline } => cli::show_cmd(&run_id, json, step.as_deref(), attempt, follow, timeline),
        Command::Attach { run_id } => detach::attach(&run_id),
        Command::Export { run_id, otlp } => cli::export_cmd(&run_id, otlp),
        Command::Resume { run_id, workers, allow_direct, max_usd, allow_weaker_guards, direction, redo_step, json } => cli::resume(&run_id, workers, allow_direct, max_usd, allow_weaker_guards, direction, redo_step, json).await,
        Command::Evolve { target, threshold, dry_run, mutate, propose, propose_usd, propose_workers, cwd, json } => cli::evolve_cmd(&target, threshold, dry_run, json, cwd, mutate, propose, propose_usd, propose_workers),
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// A run id names a directory under `~/.cordata/orchestrator`: letters, digits, `-` and `_` only, so it cannot
/// point anywhere else.
fn run_id_arg(id: &str) -> Result<String, String> {
    let ok = !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok { Ok(id.to_owned()) } else { Err("a run id uses only letters, digits, '-' and '_'".into()) }
}
