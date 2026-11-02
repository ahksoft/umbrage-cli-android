//! Progress rendering for long-running operations.
//!
//! Lives apart from `ops` so both the device commands and the network layer
//! (`repo`) can use it without depending on each other. Everything drawn here
//! goes to **stderr** so stdout stays reserved for data.

use std::io::Write;

use crate::numbers::human_bytes;

/// Build a progress callback that renders a single updating line on stderr.
///
/// Redraws only when the percentage changes, so a slow 2 GiB read or a large
/// download does not flood a log file.
pub(crate) fn progress(label: &'static str) -> impl FnMut(usize, usize) + Send {
    let mut last_pct: i32 = -1;
    move |done, total| {
        let pct = if total > 0 {
            (done as u64 * 100 / total as u64) as i32
        } else {
            -1
        };
        if pct == last_pct {
            return;
        }
        last_pct = pct;
        if total > 0 {
            eprint!(
                "\r  {label}: {pct}% ({}/{})",
                human_bytes(done as u64),
                human_bytes(total as u64)
            );
        } else {
            eprint!("\r  {label}: {}", human_bytes(done as u64));
        }
        let _ = std::io::stderr().flush();
    }
}

/// Finish a progress line so the next output starts clean.
pub(crate) fn end_progress() {
    eprintln!();
}
