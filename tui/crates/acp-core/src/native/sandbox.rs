//! Command confinement: a Seatbelt profile and a scrubbed environment. A command writes only inside the project,
//! temporary folders, package caches and the folders the person named (`chat.writable`); it reaches no host but this
//! one unless the call asked for the network and the person approved it; it never reads `.env` files or credential
//! directories. Fail closed: when `sandbox-exec` cannot be applied, commands do not run.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::process::Command;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The only parent-environment variables a child inherits.
const ENV_ALLOW: &[&str] =
    &["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "TERM", "LANG", "TZ", "LC_ALL", "LC_CTYPE", "COLORTERM", "NO_COLOR"];

/// Home-relative paths the child never needs and must not read.
const DENY_HOME: &[&str] = &[
    ".cordata",
    ".ssh",
    ".gnupg",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".gemini",
    ".anthropic",
    ".config/op",
    ".aws",
    ".netrc",
    ".config/gh",
    ".docker/config.json",
    ".kube/config",
];

const TEMPLATES: &[&str] = &["example", "sample", "template", "dist"];

/// `.env`, `.env.local` … but not the committed templates (`.env.example`).
pub(crate) fn is_secret_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    if name == ".env" {
        return true;
    }
    match name.strip_prefix(".env.") {
        Some(suffix) => !TEMPLATES.contains(&suffix),
        None => false,
    }
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Home-relative caches a build or a package manager writes on its own. Nothing here is executed from: `~/.cargo/bin` and
/// the like stay read-only, so a command cannot plant a binary the next shell runs.
const WRITABLE_HOME: &[&str] = &["Library/Caches", ".cache", ".cargo/registry", ".cargo/git", ".npm", ".pnpm-store", ".yarn/cache", "go/pkg/mod", ".gradle/caches", ".m2/repository"];

/// Where one command may write and whether it may reach the network.
pub(crate) struct Confinement<'a> {
    pub(crate) project: &'a Path,
    pub(crate) network: bool,
    pub(crate) writable: &'a [PathBuf],
}

/// Seatbelt matches resolved paths: `/tmp` is `/private/tmp`, `$TMPDIR` is under `/private/var/folders`.
fn real(path: &Path) -> String {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned()
}

/// The repository the project sits in, when it is a subfolder of one: `git commit` there writes the parent's `.git`.
/// Never `$HOME` (a dotfiles repository) or `/`: that would make shell start-up files and launch agents writable.
fn repository_root(project: &Path, home: &str) -> Option<PathBuf> {
    let project = project.canonicalize().ok()?;
    let home = Path::new(home).canonicalize().unwrap_or_else(|_| PathBuf::from(home));
    project
        .ancestors()
        .skip(1)
        .take_while(|dir| *dir != home && dir.parent().is_some())
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Inside the writable roots, still read-only: npx runs what it caches in `_npx`.
const READ_ONLY_HOME: &[&str] = &[".npm/_npx"];

/// What git runs later, outside any sandbox: hooks, and the config that names programs (`core.fsmonitor`,
/// `core.hooksPath`, filter drivers, aliases) or attributes that select them. Commits still write objects, refs and the
/// index.
pub(crate) const GIT_EXEC_DENY: &str =
    r#"(deny file-write* (regex #"/\.git/(config|hooks(/|$)|info/attributes$|modules/.+/(config|hooks(/|$)))"))"#;

/// Local daemons that would carry a request out on a command's behalf.
const DENY_SOCKETS: &[&str] = &["/private/var/run/docker.sock"];
const DENY_HOME_SOCKETS: &[&str] = &[".docker/run/docker.sock", ".colima/default/docker.sock", ".orbstack/run/docker.sock"];

pub(crate) fn profile(home: &str, confinement: &Confinement<'_>) -> String {
    let mut roots: Vec<String> = vec![real(confinement.project), "/private/tmp".into(), "/private/var/folders".into(), "/dev".into()];
    roots.extend(WRITABLE_HOME.iter().map(|relative| format!("{home}/{relative}")));
    roots.extend(repository_root(confinement.project, home).map(|p| p.to_string_lossy().into_owned()));
    roots.retain(|root| root != home && root != "/" && !root.is_empty());
    roots.extend(confinement.writable.iter().map(|p| real(p)));
    let mut lines = vec![
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        // Later rules win: deny every write, allow the roots, then deny the secrets inside them.
        "(deny file-write*)".to_owned(),
        format!("(allow file-write* {})", roots.iter().map(|r| format!("(subpath {})", quote(r))).collect::<Vec<_>>().join(" ")),
        format!("(deny file-write* {})", READ_ONLY_HOME.iter().map(|r| format!("(subpath {})", quote(&format!("{home}/{r}")))).collect::<Vec<_>>().join(" ")),
        GIT_EXEC_DENY.to_owned(),
        r#"(deny file-read* file-write* (regex #"(^|/)\.env($|\.[^/]*$)"))"#.to_owned(),
        r#"(allow file-read* (regex #"(^|/)\.env\.(example|sample|template|dist)$"))"#.to_owned(),
    ];
    for relative in DENY_HOME {
        lines.push(format!("(deny file-read* file-write* (subpath {}))", quote(&format!("{home}/{relative}"))));
    }
    // Permit reading offloaded tool outputs under ~/.cordata/offloaded even while ~/.cordata is denied.
    lines.push(format!("(allow file-read* (subpath {}))", quote(&format!("{home}/.cordata/offloaded"))));
    if !confinement.network {
        // Loopback stays open for tests that bind a port.
        lines.extend(["(deny network-outbound)".to_owned(), r#"(allow network-outbound (remote ip "localhost:*"))"#.to_owned()]);
    }
    // Unix sockets lead to local daemons that act outside this sandbox (herdr and other multiplexers start programs,
    // Docker runs containers, ssh-agent signs): a command may reach only the sockets under the roots it may write,
    // which is where a test's own server listens, online or not. The rules follow the network rules so they win.
    // One rule per root: Seatbelt does not OR several filters inside one `(remote unix-socket …)`, so a combined
    // rule matches nothing and shuts every socket.
    lines.push("(deny network-outbound (remote unix-socket))".to_owned());
    lines.extend(roots.iter().filter(|r| *r != "/dev").map(|r| format!("(allow network-outbound (remote unix-socket (subpath {})))", quote(r))));
    lines.push(LAUNCHD_SOCKETS_DENY.to_owned());
    let sockets = DENY_SOCKETS.iter().map(|s| (*s).to_owned()).chain(DENY_HOME_SOCKETS.iter().map(|s| format!("{home}/{s}")));
    lines.extend(sockets.map(|s| format!("(deny network-outbound (remote unix-socket (path-literal {})))", quote(&s))));
    if confinement.network {
        // Names resolve through mDNSResponder's socket. Offline it stays shut, so a DNS query cannot carry data out;
        // `localhost` still resolves.
        lines.push(format!("(allow network-outbound (remote unix-socket (path-literal {})))", quote(MDNS_SOCKET)));
    }
    lines.join("\n") + "\n"
}

const MDNS_SOCKET: &str = "/private/var/run/mDNSResponder";
/// launchd's per-login sockets (ssh-agent's among them) live under `/private/tmp`, a writable root.
const LAUNCHD_SOCKETS_DENY: &str = r#"(deny network-outbound (remote unix-socket (path-regex #"^/private/tmp/com\.apple\.launchd\.[^/]+/")))"#;

pub(crate) fn scrubbed_env() -> Vec<(String, String)> {
    ENV_ALLOW.iter().filter_map(|name| std::env::var(name).ok().map(|v| ((*name).to_owned(), v))).collect()
}

/// A project hook's shell, confined like `run_command`: offline, writing only the project and temp folders, with the
/// scrubbed environment. `None` when the sandbox cannot be applied here (the caller then refuses to run it).
pub(crate) fn hook_command(cmd: &str, project: &Path) -> Option<std::process::Command> {
    if !available() {
        return None;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let confinement = Confinement { project, network: false, writable: &[] };
    let mut command = std::process::Command::new(SANDBOX_EXEC);
    command.args(["-p", &profile(&home, &confinement), "/bin/sh", "-c", cmd]).env_clear().envs(scrubbed_env());
    Some(command)
}

/// Can `sandbox-exec` apply a profile here? It fails (exit 71) when already inside a sandbox.
fn available() -> bool {
    static PROBE: OnceLock<bool> = OnceLock::new();
    *PROBE.get_or_init(|| {
        std::process::Command::new(SANDBOX_EXEC)
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Runs `cmd` under `/bin/zsh -c`, confined. Returns combined stdout+stderr with the exit status.
pub(crate) async fn run(cmd: &str, confinement: &Confinement<'_>, timeout: Duration) -> Result<String, String> {
    if !available() {
        return Err("the Seatbelt sandbox (sandbox-exec) cannot be applied here, so commands are refused \
                    (a child process could read .env files)"
            .into());
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut child = Command::new(SANDBOX_EXEC)
        .args(["-p", &profile(&home, confinement), "/bin/zsh", "-c", cmd])
        .env_clear()
        .envs(scrubbed_env())
        .current_dir(confinement.project)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so what the shell started in the background dies with it.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start the command: {e}"))?;
    // A cancelled turn drops this future: the guard kills the whole group then too.
    let group = KillGroup(child.id());
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let capture = tokio::spawn(async move { tokio::join!(read_capped(stdout), read_capped(stderr)) });
    let status = tokio::time::timeout(timeout, child.wait()).await;
    // The shell is done (or out of time): background jobs go too, so the pipes close and the capture ends.
    kill_group(group.0);
    let ((out, out_dropped), (err, err_dropped)) = match tokio::time::timeout(PIPE_GRACE, capture).await {
        Ok(Ok(both)) => both,
        // Something left the group (`setsid`) and holds the pipes open; give up on the output rather than hang.
        _ => ((Vec::new(), 0), (b"(output lost: a process outside the command's group kept it open)\n".to_vec(), 0)),
    };
    let mut text = String::from_utf8_lossy(&out).into_owned();
    text.push_str(&String::from_utf8_lossy(&err));
    let dropped = out_dropped + err_dropped;
    if dropped > 0 {
        text.push_str(&format!("\n…[{dropped} more bytes of output discarded]"));
    }
    match status {
        Err(_) => Err(format!("timed out after {}s\n{text}", timeout.as_secs())),
        Ok(Err(e)) => Err(format!("could not wait for the command: {e}")),
        Ok(Ok(status)) if status.success() => Ok(text),
        Ok(Ok(status)) => Err(format!("exit {}\n{text}", status.code().map_or("signal".to_owned(), |c| c.to_string()))),
    }
}

/// Bytes kept per stream; the rest is read and counted so the command never blocks on a full pipe.
const MAX_CAPTURE: usize = 4 << 20;
/// How long the output may stay open after the command's group is gone.
const PIPE_GRACE: Duration = Duration::from_secs(2);

struct KillGroup(Option<u32>);

impl Drop for KillGroup {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid.filter(|p| *p > 0) {
        // SAFETY: killpg only sends a signal; a group that is already gone fails with ESRCH, which is ignored.
        unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
    }
}

async fn read_capped(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> (Vec<u8>, u64) {
    use tokio::io::AsyncReadExt as _;
    let (mut kept, mut dropped) = (Vec::new(), 0u64);
    let Some(mut pipe) = pipe else { return (kept, dropped) };
    let mut buf = [0u8; 8192];
    while let Ok(n @ 1..) = pipe.read(&mut buf).await {
        let take = n.min(MAX_CAPTURE.saturating_sub(kept.len()));
        kept.extend_from_slice(&buf[..take]);
        dropped += (n - take) as u64;
    }
    (kept, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_rule_matches_python() {
        for yes in [".env", "a/b/.env", ".env.local", "x/.env.production"] {
            assert!(is_secret_path(Path::new(yes)), "{yes}");
        }
        for no in [".env.example", ".env.sample", "env", "dotenv.rs", "a/.envrc"] {
            assert!(!is_secret_path(Path::new(no)), "{no}");
        }
    }

    fn offline(project: &Path) -> Confinement<'_> {
        Confinement { project, network: false, writable: &[] }
    }

    #[test]
    fn profile_denies_secrets_and_reallows_templates() {
        let p = profile("/Users/x", &offline(Path::new("/nonexistent/p")));
        assert!(p.starts_with("(version 1)\n(allow default)\n(deny file-write*)\n"));
        assert!(p.contains(r#"(subpath "/nonexistent/p") (subpath "/private/tmp")"#));
        assert!(p.contains(r#"(subpath "/Users/x/.cargo/registry")"#) && !p.contains(r#""/Users/x/.cargo")"#));
        assert!(p.contains("(deny network-outbound)"));
        let online = profile("/Users/x", &Confinement { project: Path::new("/p"), network: true, writable: &[PathBuf::from("/opt/out")] });
        assert!(!online.contains("(deny network-outbound)\n") && online.contains(r#"(subpath "/opt/out")"#));
        // Unix sockets: only under the writable roots, online or not; DNS's socket only online.
        for prof in [&p, &online] {
            let deny = prof.find("(deny network-outbound (remote unix-socket))").expect("every unix socket is refused first");
            let allow = prof.find("(allow network-outbound (remote unix-socket (subpath").unwrap();
            assert!(deny < allow && prof.contains(LAUNCHD_SOCKETS_DENY));
            assert!(!prof.lines().any(|l| l.starts_with("(allow network-outbound (remote unix-socket") && l.contains("\"/dev\"")));
            assert!(!prof.lines().any(|l| l.contains("(remote unix-socket (subpath") && l.matches("(subpath").count() > 1), "one place per rule");
        }
        assert!(online.contains(MDNS_SOCKET) && !p.contains(MDNS_SOCKET));
        assert!(p.find("(deny network-outbound)\n").unwrap() < p.find("(deny network-outbound (remote unix-socket))").unwrap(), "the socket rules come last, so they win");
        assert!(p.contains(r#"(deny file-read* file-write* (subpath "/Users/x/.aws"))"#));
        assert!(p.contains(r#"(deny file-read* file-write* (subpath "/Users/x/.ssh"))"#));
        assert!(p.contains(r#"(deny file-read* file-write* (subpath "/Users/x/.cordata"))"#));
        assert!(p.contains(r#"(allow file-read* (subpath "/Users/x/.cordata/offloaded"))"#));
        assert!(p.contains("example|sample|template|dist"));
    }

    #[test]
    fn env_is_scrubbed_to_the_allowlist() {
        unsafe { std::env::set_var("CORDATA_TEST_SECRET", "x") };
        assert!(scrubbed_env().iter().all(|(k, _)| ENV_ALLOW.contains(&k.as_str())));
    }

    #[tokio::test]
    async fn runs_a_command_and_hides_the_environment() {
        if !available() {
            return; // nested sandbox: nothing to assert
        }
        unsafe { std::env::set_var("CORDATA_TEST_SECRET", "leak") };
        let tmp = std::env::temp_dir();
        let out = run("echo hi; echo \"[$CORDATA_TEST_SECRET]\"", &offline(&tmp), Duration::from_secs(10)).await.unwrap();
        assert_eq!(out, "hi\n[]\n");
    }

    #[tokio::test]
    async fn nonzero_exit_and_timeout_are_errors() {
        if !available() {
            return;
        }
        let tmp = std::env::temp_dir();
        let err = run("echo no; exit 3", &offline(&tmp), Duration::from_secs(10)).await.unwrap_err();
        assert!(err.starts_with("exit 3"));
        let err = run("sleep 5", &offline(&tmp), Duration::from_millis(200)).await.unwrap_err();
        assert!(err.contains("timed out"));
    }

    #[test]
    fn a_subfolder_of_a_repository_may_write_the_repository() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        std::fs::create_dir_all(repo.path().join("sub/dir")).unwrap();
        let p = profile("/Users/x", &offline(&repo.path().join("sub/dir")));
        assert!(p.contains(&format!("(subpath {})", quote(&real(repo.path())))), "{p}");
        let alone = tempfile::tempdir().unwrap();
        assert_eq!(repository_root(alone.path(), "/Users/x"), None);
        // A dotfiles repository at $HOME is not a repository root to write into.
        let home = real(repo.path());
        let p = profile(&home, &offline(&repo.path().join("sub/dir")));
        assert!(!p.contains(&format!("(subpath {})", quote(&home))), "{p}");
        assert!(p.contains(&format!("(deny file-write* (subpath {}))", quote(&format!("{home}/.npm/_npx")))));
        assert!(p.contains("docker.sock"));
    }

    #[tokio::test]
    async fn git_config_and_hooks_are_read_only_but_commits_work() {
        if !available() || std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let project = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git").args(["init", "-q"]).current_dir(project.path()).status().unwrap();
        assert!(init.success());
        let c = offline(project.path());
        let err = run("git config core.fsmonitor 'echo pwned'", &c, Duration::from_secs(10)).await.unwrap_err();
        assert!(err.starts_with("exit "), "{err}");
        assert!(run("echo x > .git/hooks/pre-commit", &c, Duration::from_secs(10)).await.is_err());
        assert!(!project.path().join(".git/hooks/pre-commit").exists());
        let commit = "echo a > a.txt && git add a.txt && git -c user.name=t -c user.email=t@t commit -qm a && git log --oneline";
        assert!(run(commit, &c, Duration::from_secs(10)).await.unwrap().contains(" a"));
    }

    #[tokio::test]
    async fn only_sockets_under_the_writable_roots_can_be_reached() {
        if !available() || !Path::new("/usr/bin/nc").exists() {
            return;
        }
        // Socket paths are capped at 104 bytes, so both live in short folders under /private/tmp.
        let project = tempfile::Builder::new().prefix("cdp").tempdir_in("/private/tmp").unwrap();
        let outside = tempfile::Builder::new().prefix("cdo").tempdir_in(std::env::var("HOME").unwrap()).unwrap();
        let listen = |path: PathBuf| std::process::Command::new("/usr/bin/nc").arg("-lU").arg(path).stdout(Stdio::null()).spawn().unwrap();
        let (inside, away) = (project.path().join("s"), outside.path().join("s"));
        let mut servers = [listen(inside.clone()), listen(away.clone())];
        std::thread::sleep(Duration::from_millis(400));
        let c = offline(project.path());
        let reach = |path: &Path| format!("echo hi | /usr/bin/nc -U -w1 '{}'", path.display());
        assert!(run(&reach(&inside), &c, Duration::from_secs(10)).await.is_ok(), "a test's own server in the project");
        let online = Confinement { project: project.path(), network: true, writable: &[] };
        assert!(run(&reach(Path::new(MDNS_SOCKET)), &online, Duration::from_secs(10)).await.is_ok(), "address lookup reachable online");
        assert!(run(&reach(Path::new(MDNS_SOCKET)), &c, Duration::from_secs(10)).await.is_err(), "address lookup blocked offline");
        assert!(run(&reach(&away), &c, Duration::from_secs(10)).await.is_err(), "a daemon's socket elsewhere, offline");
        assert!(run(&reach(&away), &online, Duration::from_secs(10)).await.is_err(), "…and online");
        for server in &mut servers {
            let _ = server.kill();
        }
    }

    #[tokio::test]
    async fn a_background_job_neither_holds_the_result_nor_outlives_it() {
        if !available() {
            return;
        }
        let project = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let out = run("echo started; (sleep 3; touch late) &", &offline(project.path()), Duration::from_secs(20)).await.unwrap();
        assert_eq!(out, "started\n");
        assert!(started.elapsed() < Duration::from_secs(3), "returned when the shell did, not when the job did");
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert!(!project.path().join("late").exists(), "the background job was killed with the command");
    }

    #[tokio::test]
    async fn endless_output_is_capped_and_ends_at_the_timeout() {
        if !available() {
            return;
        }
        let tmp = std::env::temp_dir();
        let err = run("yes", &offline(&tmp), Duration::from_secs(2)).await.unwrap_err();
        assert!(err.starts_with("timed out") && err.contains("bytes of output discarded"));
        assert!(err.len() < MAX_CAPTURE + 4096);
    }

    #[test]
    fn the_profile_keeps_git_config_and_hooks_read_only() {
        let p = profile("/Users/x", &offline(Path::new("/p")));
        assert!(p.contains(GIT_EXEC_DENY));
        let allow = p.find("(allow file-write* (subpath").unwrap();
        assert!(p.find(GIT_EXEC_DENY).unwrap() > allow, "the deny follows the allow, so it wins");
    }

    #[tokio::test]
    async fn writes_stay_in_the_project_and_the_network_stays_shut() {
        if !available() {
            return;
        }
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
        let target = elsewhere.path().join("planted");
        let c = offline(project.path());
        run("echo ok > inside.txt", &c, Duration::from_secs(10)).await.unwrap();
        assert!(project.path().join("inside.txt").exists());
        assert!(run(&format!("echo x > '{}'", target.display()), &c, Duration::from_secs(10)).await.is_err());
        assert!(!target.exists(), "a write outside the project and the caches is denied");
        let err = run("/usr/bin/curl -sS -m 3 -o /dev/null http://1.1.1.1/", &c, Duration::from_secs(10)).await.unwrap_err();
        assert!(err.starts_with("exit "), "{err}");
        let extra = [elsewhere.path().to_path_buf()];
        let named = Confinement { project: project.path(), network: false, writable: &extra };
        run(&format!("echo x > '{}'", target.display()), &named, Duration::from_secs(10)).await.unwrap();
        assert!(target.exists(), "a folder from chat.writable may be written");
    }
}
