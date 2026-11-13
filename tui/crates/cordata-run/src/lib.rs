//! `cordata-run`: `cordata run | status | show | resume`, which drives the orchestrator (`cordata-orchestrator`) from a
//! terminal. `status` and `show` only read `state.db`.

pub mod cli;
pub mod config;
pub mod controls;
pub mod native_worker;
pub mod receipt;
pub mod roster;
pub mod status;

/// Local `HH:MM` / `HH:MM:SS` for a Unix timestamp, like Python's `time.strftime(..., time.localtime(ts))`.
pub fn local_time(ts: f64, format: &str) -> String {
    let secs = ts as libc::time_t;
    // SAFETY: `localtime_r` and `strftime` write only into the buffers handed to them, and the format string is NUL-terminated.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return String::new();
        }
        let format = std::ffi::CString::new(format).expect("no NUL in a format");
        let mut buf = [0u8; 64];
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), format.as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}
