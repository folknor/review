use anyhow::{Result, bail};
use std::fs::File;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Open (or create) the shared lock file safely. The lock lives on a shared,
/// world-writable path (`/tmp`), so:
/// - `O_NOFOLLOW` refuses to open through a symlink, defeating a planted-symlink
///   clobber that `File::create` (which follows symlinks and truncates) allowed;
/// - mode `0666` lets a second user open the pre-existing lock for the flock
///   (subject to umask), instead of failing on a first user's `0644`;
/// - no truncation - the file's contents are irrelevant to advisory locking, and
///   truncating would needlessly rewrite whatever is there.
pub fn open_lock_file(path: &Path) -> Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o666)
        .open(path)
        .map_err(|e| anyhow::anyhow!("failed to open lock file {}: {e}", path.display()))
}

/// Acquire an exclusive advisory lock on the given file.
/// If the lock is contested, prints a message and reports wait time.
/// Released when the file is dropped.
pub fn acquire_blocking(file: &File) -> Result<()> {
    // Try non-blocking first
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        return Ok(());
    }

    // Check if it's actually contention
    let err = std::io::Error::last_os_error();
    if err.kind() != std::io::ErrorKind::WouldBlock {
        bail!("failed to acquire lock: {err}");
    }

    // Lock is held by another process - wait
    eprintln!("Waiting for another review to finish...");
    let start = std::time::Instant::now();

    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        bail!(
            "failed to acquire lock: {}",
            std::io::Error::last_os_error()
        );
    }

    let elapsed = start.elapsed();
    eprintln!("Lock acquired after {:.1}s", elapsed.as_secs_f64());
    Ok(())
}
