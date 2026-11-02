//! Reporting to herdr (herdr.dev), the terminal multiplexer for coding agents, when Cordata runs in one of its panes:
//! the lifecycle state (`working`, `blocked`, `idle`), why it is blocked, and the command that resumes the session after
//! a herdr restart. herdr's own protocol: `$HERDR_BIN_PATH pane report-agent …` and `pane release-agent` on exit.
//!
//! Reports never slow the UI: they go through a watch channel, so only the latest is sent, each with a short timeout,
//! and failures are ignored. Outside herdr (`HERDR_ENV` unset) nothing runs. `CORDATA_HERDR=off` turns it off.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::watch;

const SOURCE: &str = "cordata";
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// `working`, `blocked` or `idle` (herdr has no `done`: it works that out from what the person has seen).
    pub state: &'static str,
    pub message: Option<String>,
    pub session: Option<String>,
}

pub struct Herdr {
    bin: PathBuf,
    pane: String,
    /// Whether `cordata` is on PATH: herdr reruns the resume command by name.
    resumable: bool,
    tx: watch::Sender<Option<Report>>,
}

/// Wall-clock milliseconds: herdr orders reports by `seq` per source, and this keeps rising across restarts.
fn seq() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

/// herdr reruns a resume command only when every argument is plain: no apostrophe, no control character.
fn plain_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn on_path(name: &str, path: Option<&str>) -> bool {
    path.unwrap_or_default().split(':').filter(|d| !d.is_empty()).any(|dir| Path::new(dir).join(name).is_file())
}

/// The arguments after `$HERDR_BIN_PATH` for one report.
pub fn report_args(pane: &str, seq: u128, report: &Report, resumable: bool) -> Vec<String> {
    let mut args: Vec<String> = ["pane", "report-agent", pane, "--source", SOURCE, "--agent", SOURCE, "--state", report.state]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    args.extend(["--seq".to_owned(), seq.to_string()]);
    if let Some(message) = &report.message {
        let message: String = message.chars().filter(|c| !c.is_control()).take(200).collect();
        args.extend(["--message".to_owned(), message]);
    }
    if let Some(id) = report.session.as_deref().filter(|id| plain_id(id)) {
        args.extend(["--agent-session-id".to_owned(), id.to_owned()]);
        if resumable {
            // `cordata --resume`, the flag: `cordata resume` is the orchestrator's command.
            args.extend(["--".to_owned(), "cordata".to_owned(), "--resume".to_owned(), id.to_owned()]);
        }
    }
    args
}

pub fn release_args(pane: &str, seq: u128) -> Vec<String> {
    ["pane", "release-agent", pane, "--source", SOURCE, "--agent", SOURCE, "--seq"]
        .iter()
        .map(|s| (*s).to_owned())
        .chain(std::iter::once(seq.to_string()))
        .collect()
}

async fn invoke(bin: &Path, args: &[String]) {
    let child = tokio::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    let _ = tokio::time::timeout(TIMEOUT, child).await;
}

impl Herdr {
    /// `Some` inside a herdr pane.
    pub fn connect(env: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if env("HERDR_ENV").as_deref() != Some("1") || env("CORDATA_HERDR").is_some_and(|v| v == "off") {
            return None;
        }
        let pane = env("HERDR_PANE_ID").filter(|p| !p.is_empty())?;
        let bin = PathBuf::from(env("HERDR_BIN_PATH").filter(|b| !b.is_empty())?);
        let resumable = on_path("cordata", env("PATH").as_deref());
        let (tx, _) = watch::channel(None);
        Some(Self { bin, pane, resumable, tx })
    }

    /// Starts the task that sends reports. Call once, inside the runtime.
    pub fn start(&self) {
        let mut rx = self.tx.subscribe();
        let (bin, pane, resumable) = (self.bin.clone(), self.pane.clone(), self.resumable);
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let Some(report) = rx.borrow_and_update().clone() else { continue };
                invoke(&bin, &report_args(&pane, seq(), &report, resumable)).await;
            }
        });
    }

    /// Queues `report` unless it is what was last queued.
    pub fn report(&self, report: Report) {
        self.tx.send_if_modified(|last| {
            let changed = last.as_ref() != Some(&report);
            if changed {
                *last = Some(report);
            }
            changed
        });
    }

    /// The person quit: clears Cordata's name, state and resume command from the pane.
    pub async fn release(&self) {
        invoke(&self.bin, &release_args(&self.pane, seq())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn only_inside_a_herdr_pane() {
        assert!(Herdr::connect(env(&[])).is_none());
        assert!(Herdr::connect(env(&[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w1:p2")])).is_none(), "no binary to call");
        let vars = [("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w1:p2"), ("HERDR_BIN_PATH", "/x/herdr")];
        assert!(Herdr::connect(env(&vars)).is_some());
        let off = [("HERDR_ENV", "1"), ("HERDR_PANE_ID", "p"), ("HERDR_BIN_PATH", "/x/herdr"), ("CORDATA_HERDR", "off")];
        assert!(Herdr::connect(env(&off)).is_none());
    }

    #[test]
    fn a_report_names_the_state_and_resumes_with_the_flag() {
        let report = Report { state: "blocked", message: Some("needs approval:\nRun run_command".into()), session: Some("20261102-ab12".into()) };
        let args = report_args("w1:p2", 7, &report, true);
        assert_eq!(
            args,
            [
                "pane", "report-agent", "w1:p2", "--source", "cordata", "--agent", "cordata", "--state", "blocked", "--seq", "7",
                "--message", "needs approval:Run run_command", "--agent-session-id", "20261102-ab12", "--", "cordata", "--resume",
                "20261102-ab12",
            ]
        );
        let not_on_path = report_args("p", 1, &report, false);
        assert!(!not_on_path.contains(&"--".to_owned()) && not_on_path.contains(&"--agent-session-id".to_owned()));
        let odd = Report { state: "idle", message: None, session: Some("a'b".into()) };
        assert_eq!(report_args("p", 1, &odd, true).len(), 11, "an id herdr would refuse is left out");
        assert_eq!(release_args("p", 3), ["pane", "release-agent", "p", "--source", "cordata", "--agent", "cordata", "--seq", "3"]);
    }

    #[test]
    fn path_lookup_finds_a_file_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cordata"), "").unwrap();
        let path = format!("/nonexistent:{}", dir.path().display());
        assert!(on_path("cordata", Some(&path)));
        assert!(!on_path("cordata", Some("/nonexistent")) && !on_path("cordata", None));
    }

    #[tokio::test]
    async fn only_the_latest_change_is_queued() {
        let vars = [("HERDR_ENV", "1"), ("HERDR_PANE_ID", "p"), ("HERDR_BIN_PATH", "/x/herdr")];
        let herdr = Herdr::connect(env(&vars)).unwrap();
        let mut rx = herdr.tx.subscribe();
        let idle = Report { state: "idle", message: None, session: None };
        herdr.report(idle.clone());
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();
        herdr.report(idle);
        assert!(!rx.has_changed().unwrap(), "the same report is not sent twice");
    }
}
