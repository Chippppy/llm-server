//! Process-level lock that serializes the CLI's mutating operations.
//!
//! `activate` and `stop --all` perform multi-step systemd sequences
//! (stop peers → start → wait for health), so a double-`activate` or a
//! concurrent `stop --all` can clobber an in-progress swap. A single
//! non-blocking exclusive `flock(2)` on a well-known lock file prevents
//! that: a second command fails fast instead of interleaving.
//!
//! Notes:
//! - The lock is per-user and anchored to the user's XDG state/config dir,
//!   so two invocations running under different environments (different
//!   `HOME`) are NOT mutually exclusive. When neither dir is available we
//!   fall back to the temp dir and warn, since a temp-dir lock may not be
//!   shared across invocations.
//! - `flock(2)` requires a local filesystem; it is not reliably honored on
//!   network filesystems such as NFS.
//! - On non-Unix targets the lock is a no-op (this tool is Linux/systemd-only).

use std::path::{Path, PathBuf};

/// RAII guard holding the exclusive process lock.
///
/// The lock is released when the guard is dropped: the fd is unlocked and
/// then closed (closing the fd releases the lock even without unlock).
#[derive(Debug)]
pub struct ProcessLock {
    file: std::fs::File,
}

impl ProcessLock {
    /// Acquires the lock at [`lock_path()`].
    ///
    /// Fails with a friendly message if another `llm-server-ctl` process
    /// already holds it (non-blocking `flock`).
    pub fn acquire() -> Result<Self, String> {
        Self::acquire_at(&lock_path())
    }

    /// Acquires the lock at an explicit `path`.
    ///
    /// Creates the lock file's parent directory if needed. Used directly by
    /// the unit tests so they never touch the real lock location.
    pub(crate) fn acquire_at(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "Failed to create lock directory {}: {}",
                    parent.display(),
                    e
                )
            })?;
        }
        #[cfg(unix)]
        let file = open_lock_file(path)?;
        #[cfg(not(unix))]
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .map_err(|e| format!("Failed to open lock file {}: {}", path.display(), e))?;
        lock_fd(&file, path)?;
        Ok(Self { file })
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        // Unlock explicitly, then `file` drops and closes the fd.
        unlock_fd(&self.file);
    }
}

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

/// Open the lock file for read/write, creating it if needed.
///
/// Hardened against symlink races (the file is opened with `O_NOFOLLOW`,
/// and a regular-file check follows) and created with `0o600` regardless of
/// umask.
#[cfg(unix)]
fn open_lock_file(path: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;

    // O_NOFOLLOW: refuse symlinks; 0o600: owner-only, independent of umask.
    let flags = libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW;
    let fd = unsafe {
        libc::open(
            path.as_os_str().as_bytes().as_ptr() as *const libc::c_char,
            flags,
            0o600,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!(
            "Failed to open lock file {}: {}",
            path.display(),
            err
        ));
    }
    let mut sb: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut sb) } != 0 {
        unsafe { libc::close(fd) };
        let err = std::io::Error::last_os_error();
        return Err(format!(
            "Failed to stat lock file {}: {}",
            path.display(),
            err
        ));
    }
    if (sb.st_mode & libc::S_IFMT) != libc::S_IFREG {
        unsafe { libc::close(fd) };
        return Err(format!(
            "Lock file {} is not a regular file; refusing to use it",
            path.display()
        ));
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Take a non-blocking exclusive `flock(2)` on the file's descriptor.
///
/// Returns the "already running" message on `EWOULDBLOCK` so the caller
/// can fail fast with an actionable hint.
#[cfg(unix)]
fn lock_fd(file: &std::fs::File, path: &Path) -> Result<(), String> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Err(format!(
                "another llm-server-ctl command is already running (lock held at {}). It may be waiting for a model to load (up to its --timeout). Wait for it to finish, then retry.",
                path.display()
            ));
        }
        return Err(format!(
            "Failed to acquire lock at {}: {}",
            path.display(),
            err
        ));
    }
    Ok(())
}

/// Release the exclusive `flock(2)` on the file's descriptor.
#[cfg(unix)]
fn unlock_fd(file: &std::fs::File) {
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

/// Non-Unix fallback: no locking available, so nothing to acquire.
#[cfg(not(unix))]
fn lock_fd(_file: &std::fs::File, _path: &Path) -> Result<(), String> {
    Ok(())
}

/// Non-Unix fallback: no locking available, so nothing to release.
#[cfg(not(unix))]
fn unlock_fd(_file: &std::fs::File) {}

/// Default lock file location.
///
/// `LLM_SERVER_CTL_LOCK_PATH` overrides the path (for relocatable setups);
/// otherwise it lives under the user's XDG
/// state dir, falling back to the config dir, and finally the temp dir
/// (with a warning, since a temp-dir lock may not be shared).
fn lock_path() -> PathBuf {
    // The CLI wiring tests override the path via `LLM_SERVER_CTL_LOCK_PATH`
    // (serialized with a mutex, since the process environment is shared
    // across test threads).
    if let Ok(value) = std::env::var("LLM_SERVER_CTL_LOCK_PATH") {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    match (dirs::state_dir(), dirs::config_dir()) {
        (Some(base), _) | (_, Some(base)) => base
            .join("llm-server-indicator")
            .join("llm-server-ctl.lock"),
        (None, None) => {
            let base = std::env::temp_dir();
            eprintln!(
                "warning: no XDG state/config dir available; process lock uses temp dir ({}) and may not be shared across invocations",
                base.display()
            );
            base.join("llm-server-indicator")
                .join("llm-server-ctl.lock")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_acquire_and_release() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.lock");
        let lock = ProcessLock::acquire_at(&path).expect("first acquire succeeds");
        drop(lock);
        assert!(
            ProcessLock::acquire_at(&path).is_ok(),
            "re-acquire after release should succeed"
        );
    }

    #[test]
    fn test_lock_fails_when_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.lock");
        let held = ProcessLock::acquire_at(&path).expect("first acquire succeeds");
        let err =
            ProcessLock::acquire_at(&path).expect_err("second acquire on the same path must fail");
        assert!(
            err.contains("already running"),
            "unexpected error message: {}",
            err
        );
        drop(held);
        assert!(
            ProcessLock::acquire_at(&path).is_ok(),
            "re-acquire after release should succeed"
        );
    }

    #[test]
    fn test_lock_creates_parent_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("x").join("y").join("z.lock");
        assert!(
            ProcessLock::acquire_at(&path).is_ok(),
            "acquire at a nested path should create parent dirs"
        );
    }
}
