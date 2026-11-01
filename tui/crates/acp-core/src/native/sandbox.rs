//! Command confinement, ported from `backend/scrubbed_sandbox.py`: a Seatbelt profile that denies
//! `.env` files and credential directories, and a scrubbed environment. Fail closed: when
//! `sandbox-exec` cannot be applied, commands do not run.

use std::path::Path;
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

pub(crate) fn profile(home: &str) -> String {
    let mut lines = vec![
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        r#"(deny file-read* file-write* (regex #"(^|/)\.env($|\.[^/]*$)"))"#.to_owned(),
        r#"(allow file-read* (regex #"(^|/)\.env\.(example|sample|template|dist)$"))"#.to_owned(),
    ];
    for relative in DENY_HOME {
        lines.push(format!("(deny file-read* file-write* (subpath {}))", quote(&format!("{home}/{relative}"))));
    }
    // Permit reading offloaded tool outputs under ~/.cordata/offloaded even while ~/.cordata is denied.
    lines.push(format!("(allow file-read* (subpath {}))", quote(&format!("{home}/.cordata/offloaded"))));
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
pub(crate) async fn run(cmd: &str, cwd: &Path, timeout: Duration) -> Result<String, String> {
    if !available() {
        return Err("the Seatbelt sandbox (sandbox-exec) cannot be applied here, so commands are refused \
                    (a child process could read .env files)"
            .into());
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let child = Command::new(SANDBOX_EXEC)
        .args(["-p", &profile(&home), "/bin/zsh", "-c", cmd])
        .env_clear()
        .envs(scrubbed_env())
        .current_dir(cwd)
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

    #[test]
    fn profile_denies_secrets_and_reallows_templates() {
        let p = profile("/Users/x");
        assert!(p.starts_with("(version 1)\n(allow default)\n"));
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
        let out = run("echo hi; echo \"[$CORDATA_TEST_SECRET]\"", &std::env::temp_dir(), Duration::from_secs(10)).await.unwrap();
        assert_eq!(out, "hi\n[]\n");
    }

    #[tokio::test]
    async fn nonzero_exit_and_timeout_are_errors() {
        if !available() {
            return;
        }
        let err = run("echo no; exit 3", &std::env::temp_dir(), Duration::from_secs(10)).await.unwrap_err();
        assert!(err.starts_with("exit 3"));
        let err = run("sleep 5", &std::env::temp_dir(), Duration::from_millis(200)).await.unwrap_err();
        assert!(err.contains("timed out"));
    }
}
