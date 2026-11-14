//! A run that outlives its terminal: where its output goes, keeping the Mac awake for it, telling you when it stops,
//! and `cordata attach` to watch it again.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use cordata_orchestrator::machine::{self, TERMINAL};

/// Set in the environment of a detached run so it announces how it ended.
pub const DETACHED_ENV: &str = "CORDATA_DETACHED";

/// Where a detached run's console output goes.
pub fn log_path(home: &Path, run_id: &str) -> PathBuf {
    machine::artifacts_dir(home, run_id).join("run.log")
}

/// States where the run has stopped and is waiting on a person or is over.
fn stopped(state: &str) -> bool {
    TERMINAL.contains(&state) || matches!(state, "paused" | "to_review" | "usage_low")
}

/// The notification for a run that stopped in `state`: (title, body). Only the run id and state are shown; the goal and
/// reasons can carry text from the project or a worker.
pub fn outcome_message(run_id: &str, state: &str) -> (String, String) {
    let title = match state {
        "done" => "Cordata run finished",
        "to_review" => "Cordata run is ready to review",
        "paused" | "usage_low" => "Cordata run needs you",
        "failed" | "budget_exhausted" => "Cordata run stopped",
        "cancelled" => "Cordata run cancelled",
        _ => "Cordata run stopped",
    };
    (title.to_owned(), format!("{run_id}: {state}"))
}

/// Post a macOS notification. The text goes in as arguments, never into the script, so it cannot be read as AppleScript.
/// Best effort: a failure here must not change how the run ended.
pub fn notify(title: &str, body: &str) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let script = "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run";
    let _ = std::process::Command::new("osascript")
        .args(["-e", script, "--", title, body])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Called when a detached run stops.
pub fn announce(run_id: &str, state: &str) {
    if std::env::var_os(DETACHED_ENV).is_some() && stopped(state) {
        let (title, body) = outcome_message(run_id, state);
        notify(&title, &body);
    }
}

/// Keep the Mac from idle-sleeping for as long as process `pid` lives (`caffeinate -i -w`). It exits with the process.
pub fn keep_awake(pid: u32) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let _ = std::process::Command::new("caffeinate")
        .args(["-i", "-w", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// `cordata attach RUN_ID`: print a detached run's output so far, follow it, and return when the run stops. Exit 0 when
/// it ended `done`.
pub fn attach(run_id: &str) -> i32 {
    let environ: std::collections::HashMap<String, String> = std::env::vars().collect();
    let home = crate::cli::cordata_home(&environ);
    let log = log_path(&home, run_id);
    let mut pos = 0u64;
    let mut idle_after_stop = 0;
    loop {
        let state = cordata_ledger::Ledger::open_default(Vec::new()).ok().and_then(|l| machine::load(&l, run_id)).map(|r| r.state);
        let Some(state) = state else {
            eprintln!("no run {run_id}");
            return 2;
        };
        let mut grew = false;
        if let Ok(mut file) = std::fs::File::open(&log)
            && file.seek(SeekFrom::Start(pos)).is_ok()
        {
            let mut buf = Vec::new();
            if file.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                pos += buf.len() as u64;
                print!("{}", String::from_utf8_lossy(&buf));
                grew = true;
            }
        } else if pos == 0 && !log.exists() && stopped(&state) {
            println!("{run_id}: {state} (no console log: it was not started with --detach)");
            return i32::from(state != "done");
        }
        if stopped(&state) && !grew {
            // One more quiet pass after the state changes, so the last lines the run printed are not cut off.
            idle_after_stop += 1;
            if idle_after_stop >= 2 {
                return i32::from(state != "done");
            }
        } else {
            idle_after_stop = 0;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lives_beside_the_run() {
        let p = log_path(Path::new("/h"), "run-1");
        assert_eq!(p, PathBuf::from("/h/orchestrator/run-1/run.log"));
    }

    #[test]
    fn messages_name_the_state_and_nothing_else() {
        assert_eq!(outcome_message("run-1", "done"), ("Cordata run finished".into(), "run-1: done".into()));
        assert_eq!(outcome_message("run-1", "paused").0, "Cordata run needs you");
        assert_eq!(outcome_message("run-1", "to_review").0, "Cordata run is ready to review");
        assert_eq!(outcome_message("run-1", "failed").0, "Cordata run stopped");
    }

    #[test]
    fn running_states_are_not_stops() {
        assert!(!stopped("execute"));
        assert!(!stopped("plan"));
        assert!(stopped("done") && stopped("paused") && stopped("cancelled"));
    }
}
