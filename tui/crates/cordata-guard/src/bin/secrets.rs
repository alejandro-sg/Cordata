//! `cordata-secrets [--staged | --history]`: every tracked and not-ignored file by default; only the lines being committed
//! with `--staged` (the pre-commit hook); every blob reachable from any ref with `--history`.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use cordata_guard::secrets::{credential_file, findings_in, skip_path};

fn git(args: &[&str]) -> Vec<u8> {
    Command::new("git").args(args).output().map(|o| o.stdout).unwrap_or_default()
}

fn names(bytes: Vec<u8>, sep: u8) -> Vec<String> {
    bytes.split(|b| *b == sep).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
}

fn text(data: &[u8]) -> Option<String> {
    (!data[..data.len().min(2000)].contains(&0)).then(|| String::from_utf8_lossy(data).into_owned())
}

fn scan_tree() -> Vec<String> {
    let mut bad = Vec::new();
    for path in names(git(&["ls-files", "-co", "--exclude-standard", "-z"]), 0) {
        if skip_path(&path) {
            continue;
        }
        let Some(body) = std::fs::read(&path).ok().and_then(|d| text(&d)) else { continue };
        bad.extend(findings_in(&body).into_iter().map(|(n, kind)| format!("{path}:{n}: {kind}")));
    }
    bad
}

fn scan_staged() -> Vec<String> {
    let mut bad = Vec::new();
    let staged = names(git(&["diff", "--cached", "--name-only", "--diff-filter=ACMR", "-z"]), 0);
    for path in &staged {
        if skip_path(path) {
            continue;
        }
        let diff = String::from_utf8_lossy(&git(&["diff", "--cached", "-U0", "--", path])).into_owned();
        let added: Vec<&str> = diff.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).map(|l| &l[1..]).collect();
        bad.extend(findings_in(&added.join("\n")).into_iter().map(|(_, kind)| format!("{path}: {kind} (in the staged change)")));
    }
    bad.extend(staged.iter().filter(|p| credential_file(p)).map(|p| format!("{p}: this kind of file holds credentials; it must not be committed")));
    bad
}

fn scan_history() -> Vec<String> {
    let listing = String::from_utf8_lossy(&git(&["rev-list", "--objects", "--all"])).into_owned();
    let mut bad = Vec::new();
    let Ok(mut child) = Command::new("git").args(["cat-file", "--batch"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn() else {
        return bad;
    };
    let mut stdin = child.stdin.take().expect("stdin");
    let mut out = BufReader::new(child.stdout.take().expect("stdout"));
    for line in listing.lines() {
        let (sha, name) = line.split_once(' ').unwrap_or((line, ""));
        if writeln!(stdin, "{sha}").is_err() || stdin.flush().is_err() {
            break;
        }
        let mut header = String::new();
        if out.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        let parts: Vec<&str> = header.split_whitespace().collect();
        let Some(size) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else { continue };
        let mut data = vec![0u8; size + 1];
        if out.read_exact(&mut data).is_err() {
            break;
        }
        data.truncate(size);
        if parts[1] != "blob" || size > 2_000_000 || skip_path(name) {
            continue;
        }
        let Some(body) = text(&data) else { continue };
        bad.extend(findings_in(&body).into_iter().map(|(n, kind)| format!("{}@{}:{n}: {kind}", if name.is_empty() { sha } else { name }, &sha[..8])));
    }
    drop(stdin);
    let _ = child.wait();
    bad
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = if args.iter().any(|a| a == "--staged") { "staged" } else if args.iter().any(|a| a == "--history") { "history" } else { "tree" };
    let bad = match mode { "staged" => scan_staged(), "history" => scan_history(), _ => scan_tree() };
    if bad.is_empty() {
        println!("no credentials found ({mode})");
        return;
    }
    eprintln!("possible credentials found (values not shown):");
    for item in bad {
        eprintln!("  {item}");
    }
    eprintln!("Move the value to the macOS Keychain (make key) or an environment variable. If it is a test fake, make it plainly fake (abcdefghij…, or say 'fake' in it).");
    std::process::exit(1);
}
