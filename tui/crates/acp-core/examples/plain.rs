//! Headless client for verifying `acp-core` without ratatui: streams text to
//! stdout, prompts y/n on stdin for approvals (or `--yes` / `--no`).
//!
//!     cargo run -p acp-core --example plain -- [--mode plan|build|auto] [--yes|--no] "prompt"

use std::io::{BufRead, Write};

use acp_core::{AppEvent, ApprovalDecision, SessionHandle, SessionOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut mode = None;
    let mut policy: Option<ApprovalDecision> = None;
    let mut prompt = None;
    let mut debug_log: Option<std::path::PathBuf> = None;
    let mut linger = false; // keep listening briefly after TurnComplete (diagnostics)
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => mode = args.next(),
            "--yes" => policy = Some(ApprovalDecision::Allow),
            "--no" => policy = Some(ApprovalDecision::Reject),
            "--debug-log" => debug_log = args.next().map(Into::into),
            "--linger" => linger = true,
            _ => prompt = Some(arg),
        }
    }
    let prompt = prompt.ok_or("usage: plain [--mode M] [--yes|--no] <prompt>")?;

    let (session, mut events) = SessionHandle::spawn(SessionOptions {
        cwd: std::env::current_dir()?,
        transport: acp_core::BackendTransport::Native,
        resume: None,
        debug_log,
        continue_latest: false,
    });

    let mut permission_cards = 0;
    let mut exit_code = 0;
    while let Some(event) = events.recv().await {
        match event {
            AppEvent::Connected(info) => {
                eprintln!(
                    "[connected] session={} mode={} modes={:?}",
                    info.session_id,
                    info.current_mode,
                    info.modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>()
                );
                for option in &info.config_options {
                    eprintln!(
                        "[config] {}={} ({} choices)",
                        option.id,
                        option.current_value,
                        option.choices.len()
                    );
                }
                if let Some(mode) = mode.take() {
                    session.set_mode(mode)?;
                }
                session.send_prompt(prompt.clone())?;
            }
            AppEvent::ModeChanged(mode) => eprintln!("[mode] {mode}"),
            AppEvent::AgentText(text) => {
                print!("{text}");
                std::io::stdout().flush()?;
            }
            AppEvent::AgentThought(text) => eprint!("\x1b[2m{text}\x1b[0m"),
            AppEvent::ToolCallStarted(call) => eprintln!(
                "\n[tool] {} ({}) {}",
                call.name,
                call.kind.label(),
                call.status.label()
            ),
            AppEvent::ToolCallUpdated(patch) => {
                eprintln!(
                    "[tool:{}] {:?} untrusted={} {}",
                    patch.id,
                    patch.status,
                    patch.untrusted,
                    patch
                        .content
                        .as_deref()
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                );
            }
            AppEvent::PermissionRequested { info, decision } => {
                permission_cards += 1;
                eprintln!(
                    "\n[permission] {} input={}",
                    info.title,
                    info.raw_input
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default()
                );
                let answer = match policy.clone() {
                    Some(answer) => answer,
                    None => {
                        eprint!("Allow? [y/N] ");
                        let mut line = String::new();
                        std::io::stdin().lock().read_line(&mut line)?;
                        if line.trim().eq_ignore_ascii_case("y") {
                            ApprovalDecision::Allow
                        } else {
                            ApprovalDecision::Reject
                        }
                    }
                };
                let _ = decision.send(answer);
            }
            AppEvent::TurnComplete(reason) => {
                println!();
                eprintln!(
                    "[done] stop_reason={} permission_cards={permission_cards}",
                    reason.as_str()
                );
                if linger {
                    eprintln!("[linger] listening 3s for events after TurnComplete…");
                    let end = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
                    while let Ok(Some(late)) = tokio::time::timeout_at(end, events.recv()).await {
                        match late {
                            AppEvent::AgentText(text) => {
                                eprintln!("[LATE agent text after TurnComplete] {text:?}")
                            }
                            other => eprintln!("[late event] {other:?}"),
                        }
                    }
                }
                break;
            }
            AppEvent::TurnFailed(error) => {
                eprintln!("\n[turn failed] {error}");
                exit_code = 1;
                break;
            }
            AppEvent::ConnectionError(error) => {
                eprintln!("\n[connection error] {error}");
                exit_code = 1;
                break;
            }
            AppEvent::Notice(note) => eprintln!("[notice] {note}"),
            AppEvent::Closed => break,
            _ => {}
        }
    }
    session.shutdown().await;
    std::process::exit(exit_code);
}
