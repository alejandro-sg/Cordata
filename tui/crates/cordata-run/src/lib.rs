//! `cordata-run`: the native `cordata run | status | show | resume`, the first thing that drives the Rust orchestrator
//! (`cordata-orchestrator`) from a terminal. Ported from `backend/orchestrator/{__main__,config,status,control}.py`.
//!
//! What a native run does not have yet (so `bin/cordata` keeps `run` and `resume` on Python unless asked): the research
//! phase and its evidence check, the synthesis check after parallel steps, per-step reward attribution, the Auto Mode
//! router, Laya Guard (a conservative pattern guard stands in), and the Ollama workers. `status` and `show` only read
//! `state.db` and are identical to Python's.

pub mod cli;
pub mod config;
pub mod controls;
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
