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
    /// list the doc sections whose covered files changed since they were reviewed (exit 1 when any are)
    #[arg(long)]
    docs: bool,
    /// with --docs: mark every section as reviewed against the current HEAD
    #[arg(long, requires = "docs")]
    stamp: bool,
    /// with --docs: for sections without a marker, suggest `covers:` paths from the repo map
    #[arg(long, requires = "docs", conflicts_with = "stamp")]
    suggest: bool,
}

fn main() -> ExitCode {
    cordata_guard::home::enforce();
    let args = Args::parse();
    let cwd = args.cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    if args.docs {
        return docs(&cwd, args.stamp, args.suggest, args.json);
    }
    let checks = cordata_doctor::run_checks(&cwd);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&checks).expect("checks serialize"));
    } else {
        use std::io::IsTerminal;
        println!("{}", cordata_doctor::render(&checks, std::io::stdout().is_terminal()));
    }
    if checks.iter().any(|c| c.level == cordata_doctor::Level::Fail) { ExitCode::from(1) } else { ExitCode::SUCCESS }
}

fn docs(cwd: &std::path::Path, stamp: bool, suggest: bool, json: bool) -> ExitCode {
    let Some(repo) = cordata_doctor::repo_root(cwd) else {
        eprintln!("docs: no Cordata checkout found from {}", cwd.display());
        return ExitCode::from(2);
    };
    if suggest {
        let found = cordata_doctor::suggest::suggest(&repo, 3);
        if json {
            println!("{}", serde_json::to_string_pretty(&found).expect("serializes"));
        } else {
            println!("{}", cordata_doctor::suggest::render(&found));
        }
        return ExitCode::SUCCESS;
    }
    if stamp {
        match cordata_doctor::docs::stamp(&repo) {
            Ok(n) => println!("docs: {n} section(s) marked as reviewed against HEAD"),
            Err(e) => {
                eprintln!("docs: {e}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(stale) = cordata_doctor::docs::check(&repo) else {
        eprintln!("docs: {} is not a git work tree", repo.display());
        return ExitCode::from(2);
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&stale).expect("serializes"));
    } else {
        println!("{}", cordata_doctor::docs::render(&stale));
    }
    if stale.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) }
}
