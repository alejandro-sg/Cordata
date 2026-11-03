//! Who is on the other end of a unix socket: the same user (`getpeereid`), and which process (`LOCAL_PEERPID`), so
//! the bus can insist the caller descends from the worker its token was issued to.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream as StdStream;

/// The peer runs as this process's user. A socket's file mode is advisory on macOS; the credentials are not.
pub fn same_user(fd: &impl AsRawFd) -> bool {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: getpeereid only writes the two out-parameters for a valid connected socket descriptor.
    let ok = unsafe { libc::getpeereid(fd.as_raw_fd(), &mut uid, &mut gid) } == 0;
    // SAFETY: getuid has no preconditions.
    ok && uid == unsafe { libc::getuid() }
}

/// The connecting process's pid.
#[cfg(target_os = "macos")]
pub fn peer_pid(fd: &impl AsRawFd) -> Option<u32> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: LOCAL_PEERPID writes one pid_t into the buffer we own; len says how big it is.
    let rc = unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&mut pid as *mut libc::pid_t).cast(), &mut len) };
    (rc == 0 && pid > 0).then_some(pid as u32)
}

#[cfg(not(target_os = "macos"))]
pub fn peer_pid(_fd: &impl AsRawFd) -> Option<u32> {
    None
}

/// A process's parent.
#[cfg(target_os = "macos")]
fn parent(pid: u32) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo fills at most `size` bytes of the struct we own.
    let got = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
    (got == size && info.pbi_ppid > 0).then_some(info.pbi_ppid)
}

#[cfg(not(target_os = "macos"))]
fn parent(_pid: u32) -> Option<u32> {
    None
}

/// `pid` is `root` or one of its descendants (the MCP server a worker's CLI started, a tool it ran).
pub fn descends_from(pid: u32, root: u32) -> bool {
    let mut at = pid;
    for _ in 0..64 {
        if at == root {
            return true;
        }
        match parent(at) {
            Some(up) if up != at && up > 1 => at = up,
            _ => return false,
        }
    }
    false
}

/// For the client side's own checks in tests.
pub fn std_peer_pid(stream: &StdStream) -> Option<u32> {
    peer_pid(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_descends_from_itself_and_its_parent_but_not_a_stranger() {
        let me = std::process::id();
        assert!(descends_from(me, me));
        // SAFETY: getppid has no preconditions.
        let parent_pid = unsafe { libc::getppid() } as u32;
        assert!(descends_from(me, parent_pid));
        assert!(!descends_from(parent_pid, me));
    }

    #[test]
    fn a_socket_pair_reports_this_user_and_process() {
        let (a, _b) = StdStream::pair().unwrap();
        assert!(same_user(&a));
        if cfg!(target_os = "macos") {
            assert_eq!(std_peer_pid(&a), Some(std::process::id()));
        }
    }
}
