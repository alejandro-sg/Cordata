//! `!command`: run a shell command yourself, from the prompt box, in the project folder.
//!
//! It is the person's own command with the person's own rights — the same as typing it in their shell — so it is not
//! an agent action and does not pass the agent's approval gate. What it prints is shown, and handed to the agent with
//! the next prompt ("I ran this, it said that"), which is the point: no copy and paste, and no tool call to ask for it.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How much of the output is kept (the end of it: that is where errors are).
const KEEP_BYTES: usize = 24 * 1024;
pub const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub output: String,
    pub code: Option<i32>,
    pub timed_out: bool,
    /// The output was longer than what is kept.
    pub cut: bool,
}

/// Run `command` through `sh -c` with stderr merged into stdout, no stdin, and a deadline.
pub fn run(command: &str, cwd: &Path, timeout: Duration) -> Outcome {
    run_keeping(command, cwd, timeout, KEEP_BYTES)
}

/// `run`, keeping the end of up to `keep` bytes of output (a diff review wants far more than a `!command` echo).
pub fn run_keeping(command: &str, cwd: &Path, timeout: Duration, keep: usize) -> Outcome {
    run_inner(command, cwd, timeout, keep, None)
}

fn augmented_path() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let common = [
        format!("{home}/.local/bin"),
        format!("{home}/.cargo/bin"),
        "/opt/homebrew/bin".to_string(),
        "/opt/homebrew/sbin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
        "/usr/sbin".to_string(),
        "/sbin".to_string(),
    ];
    let current = std::env::var("PATH").unwrap_or_default();
    let mut parts: Vec<String> = common
        .into_iter()
        .filter(|p| !p.is_empty() && Path::new(p).exists())
        .collect();
    for existing in current.split(':') {
        if !existing.is_empty() && !parts.iter().any(|p| p == existing) {
            parts.push(existing.to_string());
        }
    }
    parts.join(":")
}

fn run_inner(command: &str, cwd: &Path, timeout: Duration, keep: usize, cancel: Option<&AtomicBool>) -> Outcome {
    let spawned = Command::new("sh")
        .arg("-c")
        .arg(format!("exec 2>&1\n{command}"))
        .current_dir(cwd)
        .env("PATH", augmented_path())
        .env("TERM", "dumb")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return Outcome { output: format!("could not start sh: {error}"), code: None, timed_out: false, cut: false },
    };
    let mut pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let (mut kept, mut cut) = (Vec::<u8>::new(), false);
        let mut buffer = [0u8; 8192];
        while let Some(Ok(n)) = pipe.as_mut().map(|p| p.read(&mut buffer)) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&buffer[..n]);
            if kept.len() > keep * 2 {
                kept.drain(..kept.len() - keep);
                cut = true;
            }
        }
        (kept, cut)
    });

    let started = Instant::now();
    let (code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), false),
            Ok(None) if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) => {
                let _ = child.kill();
                let _ = child.wait();
                break (None, false);
            }
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break (None, false),
        }
    };
    let (mut bytes, mut cut) = reader.join().unwrap_or_default();
    if bytes.len() > keep {
        bytes.drain(..bytes.len() - keep);
        cut = true;
    }
    Outcome { output: String::from_utf8_lossy(&bytes).into_owned(), code, timed_out, cut }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_and_exit_code_come_back_with_stderr_merged() {
        let dir = tempfile::tempdir().unwrap();
        let out = run("echo out; echo err 1>&2; exit 3", dir.path(), Duration::from_secs(5));
        assert_eq!(out.code, Some(3));
        assert!(out.output.contains("out") && out.output.contains("err"), "{out:?}");
        assert!(!out.timed_out && !out.cut);
    }

    #[test]
    fn it_runs_in_the_project_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let out = run("ls", dir.path(), Duration::from_secs(5));
        assert!(out.output.contains("marker.txt"), "{out:?}");
    }

    #[test]
    fn a_runaway_command_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let out = run("sleep 30", dir.path(), Duration::from_millis(200));
        assert!(out.timed_out && out.code.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_flood_keeps_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let out = run("i=0; while [ $i -lt 20000 ]; do echo line$i; i=$((i+1)); done", dir.path(), Duration::from_secs(20));
        assert!(out.cut);
        assert!(out.output.trim_end().ends_with("line19999"), "{}", &out.output[out.output.len().saturating_sub(40)..]);
        assert!(out.output.len() <= KEEP_BYTES);
    }
}
