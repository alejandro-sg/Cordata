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
        r#"(deny file-read* file-write* (regex #"(^|/)\.env($|\.[^/]*$)"))"#.to_owned(),
        r#"(allow file-read* (regex #"(^|/)\.env\.(example|sample|template|dist)$"))"#.to_owned(),
    ];
    for relative in DENY_HOME {
        lines.push(format!("(deny file-read* file-write* (subpath {}))", quote(&format!("{home}/{relative}"))));
    }
    // Permit reading offloaded tool outputs under ~/.cordata/offloaded even while ~/.cordata is denied.
    lines.push(format!("(allow file-read* (subpath {}))", quote(&format!("{home}/.cordata/offloaded"))));
    if !confinement.network {
        // Loopback stays open for tests that bind a port. Names resolve through mDNSResponder's socket, so closing it
        // keeps a command from leaking data in a DNS query; `localhost` still resolves.
        lines.extend([
            "(deny network-outbound)".to_owned(),
            r#"(allow network-outbound (remote ip "localhost:*"))"#.to_owned(),
            "(allow network-outbound (remote unix-socket))".to_owned(),
            r#"(deny network-outbound (remote unix-socket (path-literal "/private/var/run/mDNSResponder")))"#.to_owned(),
        ]);
        let sockets = DENY_SOCKETS.iter().map(|s| (*s).to_owned()).chain(DENY_HOME_SOCKETS.iter().map(|s| format!("{home}/{s}")));
        lines.extend(sockets.map(|s| format!("(deny network-outbound (remote unix-socket (path-literal {})))", quote(&s))));
    }
    lines.join("\n") + "\n"
}

pub(crate) fn scrubbed_env() -> Vec<(String, String)> {
    ENV_ALLOW.iter().filter_map(|name| std::env::var(name).ok().map(|v| ((*name).to_owned(), v))).collect()
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
    let child = Command::new(SANDBOX_EXEC)
        .args(["-p", &profile(&home, confinement), "/bin/zsh", "-c", cmd])
        .env_clear()
        .envs(scrubbed_env())
        .current_dir(confinement.project)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, child).await {
        Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
        Ok(Err(e)) => Err(format!("could not start the command: {e}")),
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let code = out.status.code().map_or("signal".to_owned(), |c| c.to_string());
            if out.status.success() { Ok(text) } else { Err(format!("exit {code}\n{text}")) }
        }
    }
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
        assert!(!online.contains("network-outbound") && online.contains(r#"(subpath "/opt/out")"#));
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
