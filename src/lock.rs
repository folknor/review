use anyhow::{Result, bail};
use std::fs::File;
use std::os::unix::io::AsRawFd;

/// Acquire an exclusive advisory lock on the given file.
/// If the lock is contested, prints a message and reports wait time.
/// Released when the file is dropped.
pub fn acquire_blocking(file: &File) -> Result<()> {
    // Try non-blocking first
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        return Ok(());
    }

    // Lock is held by another process — wait
    eprintln!("Waiting for another review to finish...");
    let start = std::time::Instant::now();

    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        bail!("failed to acquire global lock: {}", std::io::Error::last_os_error());
    }

    let elapsed = start.elapsed();
    eprintln!("Lock acquired after {:.1}s", elapsed.as_secs_f64());
    Ok(())
}
