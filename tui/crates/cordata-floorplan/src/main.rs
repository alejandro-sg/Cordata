use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "cordata-floorplan",
    about = "Interactive architectural floorplan & active-work visualizer for Cordata workspaces"
)]
struct Cli {
    /// Workspace root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    cwd: PathBuf,

    /// Generate standalone interactive HTML blueprint and open it
    #[arg(long)]
    html: bool,

    /// Output path for the HTML blueprint (defaults to <cwd>/.cordata/floorplan.html)
    #[arg(long)]
    out: Option<PathBuf>,

    /// Open the generated HTML blueprint in default browser
    #[arg(long)]
    open: bool,

    /// Output full floorplan model as JSON to stdout
    #[arg(long)]
    json: bool,

    /// Comma-separated active wave scopes or files being worked on
    #[arg(long)]
    scope: Option<String>,

    /// Disable terminal ANSI colors
    #[arg(long)]
    no_color: bool,
}

fn main() -> ExitCode {
    let args = Cli::parse();

    let active_scope: Vec<String> = args
        .scope
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let floorplan = match cordata_floorplan::generate(&args.cwd, &active_scope) {
        Ok(fp) => fp,
        Err(e) => {
            eprintln!("Error generating floorplan: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    if args.json {
        match serde_json::to_string_pretty(&floorplan) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                eprintln!("Error serializing JSON: {e:#}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }

    if args.html || args.open || args.out.is_some() {
        let dest = args
            .out
            .unwrap_or_else(|| args.cwd.join(".cordata").join("floorplan.html"));

        if let Err(e) = cordata_floorplan::write_html_report(&floorplan, &dest) {
            eprintln!("Error writing HTML floorplan: {e:#}");
            return ExitCode::FAILURE;
        }

        println!("Interactive floorplan generated at: {}", dest.display());

        if (args.open || args.html)
            && let Err(e) = cordata_floorplan::open_in_browser(&dest) {
                eprintln!("Warning: failed opening browser: {e:#}");
            }
        return ExitCode::SUCCESS;
    }

    // Default: render beautiful terminal layout
    let term_str = cordata_floorplan::render_terminal(&floorplan, !args.no_color);
    println!("{term_str}");

    ExitCode::SUCCESS
}
