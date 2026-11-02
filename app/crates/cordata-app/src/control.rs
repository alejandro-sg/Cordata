//! `cordata-app status`, `cordata-app ls` and `cordata-app do <command> [arg]`: drive the open window from a shell
//! (scripts, agents, tests) over a socket, one JSON line each way — herdr-app's `herdr-app do`, with two guards it
//! lacks. Only a process of the same user may connect (the peer's credentials are checked, not just the file's mode),
//! and the chat sandbox cannot reach the socket at all: commands reach only unix sockets under the folders they may
//! write (`acp-core`'s Seatbelt profile). `CORDATA_APP_CONTROL=off` keeps the window from listening.
//!
//! Commands: `new`, `close`, `next`, `prev`, `switch <n>`, `attention` (the thread that needs you), `prompt <text>`
//! (sent to the shown thread, which still asks before any edit or command), `focus`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

/// A request line is small; anything longer is refused rather than read without end.
const MAX_LINE: u64 = 64 * 1024;

pub struct Request {
    pub line: Value,
    pub reply: oneshot::Sender<Value>,
}

/// `~/Library/Application Support/Cordata/cordata-app.sock`, beside the prefs (`CORDATA_APP_PREFS_DIR` moves both).
pub fn socket_path() -> Option<PathBuf> {
    crate::prefs::path().and_then(|p| p.parent().map(|dir| dir.join("cordata-app.sock")))
}

pub fn enabled() -> bool {
    std::env::var_os("CORDATA_APP_CONTROL").is_none_or(|v| v != "off")
}

/// The peer's user is this process's user. A socket file's mode is advisory on macOS; the credentials are not.
fn same_user(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: getpeereid only writes the two out-parameters for a valid connected socket descriptor.
    let ok = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0;
    // SAFETY: getuid has no preconditions.
    ok && uid == unsafe { libc::getuid() }
}

/// Serves the socket for this window. A socket that answers means another window owns it: `cordata-app do` goes there.
pub fn listen(tx: mpsc::UnboundedSender<Request>) {
    let Some(path) = socket_path() else { return };
    if UnixStream::connect(&path).is_ok() {
        eprintln!("cordata: another window serves {}; `cordata-app do` goes there", path.display());
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => return eprintln!("cordata: cannot serve {}: {error}", path.display()),
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let _ = std::thread::Builder::new().name("cordata-control".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || serve(stream, &tx));
        }
    });
}

fn serve(mut stream: UnixStream, tx: &mpsc::UnboundedSender<Request>) {
    let answer = |stream: &mut UnixStream, value: Value| {
        let _ = writeln!(stream, "{value}");
    };
    if !same_user(&stream) {
        return answer(&mut stream, json!({"ok": false, "error": "another user's process"}));
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut line = String::new();
    let read = BufReader::new((&stream).take(MAX_LINE)).read_line(&mut line);
    let request = match read.ok().filter(|n| *n > 0).and_then(|_| serde_json::from_str::<Value>(line.trim()).ok()) {
        Some(value) => value,
        None => return answer(&mut stream, json!({"ok": false, "error": "send one JSON object on one line"})),
    };
    let (reply, rx) = oneshot::channel();
    if tx.send(Request { line: request, reply }).is_err() {
        return answer(&mut stream, json!({"ok": false, "error": "the window is closing"}));
    }
    let value = rx.blocking_recv().unwrap_or_else(|_| json!({"ok": false, "error": "no answer"}));
    answer(&mut stream, value);
}

/// The request for a command line: `status`, `ls`, `do <command> [arg…]`.
pub fn request_for(args: &[String]) -> Option<Value> {
    match args.first().map(String::as_str)? {
        "status" if args.len() == 1 => Some(json!({"cmd": "status"})),
        "read" if args.len() == 1 => Some(json!({"cmd": "read"})),
        "ls" if args.len() == 1 => Some(json!({"cmd": "ls"})),
        "do" if args.len() >= 2 => {
            let arg = (args.len() > 2).then(|| args[2..].join(" "));
            Some(json!({"cmd": "do", "action": args[1], "arg": arg}))
        }
        _ => None,
    }
}

/// `cordata-app status|ls|do …` as a client: prints the window's answer, exits 0 when it did what was asked.
pub fn client(args: &[String]) -> i32 {
    let Some(request) = request_for(args) else {
        eprintln!("usage: cordata-app status | ls | read | do <new|close|next|prev|switch N|attention|prompt TEXT|run COMMAND|focus>");
        return 2;
    };
    let Some(path) = socket_path() else {
        eprintln!("cordata-app: no home folder");
        return 1;
    };
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("cordata-app: no Cordata window at {}: {error}", path.display());
            return 1;
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    if writeln!(stream, "{request}").is_err() {
        return 1;
    }
    let mut line = String::new();
    let _ = BufReader::new(&stream).read_line(&mut line);
    let value: Value = serde_json::from_str(line.trim()).unwrap_or_else(|_| json!({"ok": false, "error": "no answer"}));
    println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
    if value.get("ok").and_then(Value::as_bool) == Some(true) { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn command_lines_become_requests() {
        assert_eq!(request_for(&args(&["status"])), Some(json!({"cmd": "status"})));
        assert_eq!(request_for(&args(&["ls"])), Some(json!({"cmd": "ls"})));
        assert_eq!(request_for(&args(&["do", "switch", "2"])), Some(json!({"cmd": "do", "action": "switch", "arg": "2"})));
        assert_eq!(request_for(&args(&["do", "prompt", "fix", "the", "test"])).unwrap()["arg"], "fix the test");
        assert_eq!(request_for(&args(&["do", "attention"])).unwrap()["arg"], Value::Null);
        assert_eq!(request_for(&args(&["do"])), None);
        assert_eq!(request_for(&args(&["status", "x"])), None);
        assert_eq!(request_for(&args(&[])), None);
    }

    #[test]
    fn a_connection_from_this_user_is_this_user() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert!(same_user(&a));
    }

    #[test]
    fn one_line_in_one_line_out_and_garbage_is_refused() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || serve(server, &tx));
        writeln!(client, r#"{{"cmd":"status"}}"#).unwrap();
        let request = rx.blocking_recv().unwrap();
        assert_eq!(request.line["cmd"], "status");
        request.reply.send(json!({"ok": true})).unwrap();
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).unwrap();
        assert_eq!(line.trim(), r#"{"ok":true}"#);
        worker.join().unwrap();

        let (tx, _rx) = mpsc::unbounded_channel();
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || serve(server, &tx));
        writeln!(client, "not json").unwrap();
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).unwrap();
        assert!(line.contains("\"ok\":false"));
        worker.join().unwrap();
    }
}
