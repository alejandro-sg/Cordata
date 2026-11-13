//! `cordata-doctor [--json] [--cwd DIR]` — the native `cordata doctor`. Local checks only; spends nothing and
//! never reads a secret. Exit status is 1 when any check fails, like the Python command.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(about = "check the tools, services and settings Cordata depends on")]
struct Args {
    #[arg(long)]
    json: bool,
    #[arg(long)]
    cwd: Option<PathBuf>,
}

fn main() -> ExitCode {
    cordata_guard::home::enforce();
    let args = Args::parse();
    let cwd = args.cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let checks = cordata_doctor::run_checks(&cwd);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&checks).expect("checks serialize"));
    } else {
        use std::io::IsTerminal;
        println!("{}", cordata_doctor::render(&checks, std::io::stdout().is_terminal()));
    }
    if checks.iter().any(|c| c.level == cordata_doctor::Level::Fail) { ExitCode::from(1) } else { ExitCode::SUCCESS }
}
