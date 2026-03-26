use anyhow::{Result, bail};
use std::fs::File;
use std::os::unix::io::AsRawFd;

/// Acquire an exclusive blocking advisory lock on the given file.
/// Blocks until the lock is available. Released when the file is dropped.
pub fn acquire_blocking(file: &File) -> Result<()> {
    // LOCK_EX = exclusive, blocking (waits if held by another process)
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        bail!("failed to acquire global lock: {}", std::io::Error::last_os_error());
    }
    Ok(())
}
