// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-user singleton lock for tray-mode launches.
//!
//! v0.3.0/v0.3.1 didn't guard against the user launching `clipboardwire`
//! twice (autostart entry + manual Start Menu / shell launch, an old
//! version still alive after upgrade, etc.). With `[hub] enabled = true`
//! the second process tried to bind the same port, got "address in
//! use", logged a warning, and then carried on with its supervisor
//! pointed at the other process's hub — producing the
//! "client connects/disconnects every few seconds" symptom that
//! looked like a hub bug.
//!
//! The fix here is to fail loudly *before* any of that happens: at the
//! top of tray-mode startup, take an exclusive lock on a well-known
//! file. If we can't get it, another tray is already running for this
//! user → log and exit cleanly. The lock fd is leaked into a static
//! so the OS holds the flock until the process actually exits.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
// `File::try_lock` is the stdlib's portable advisory file lock (stable
// since Rust 1.89 / std::fs::TryLockError). On Unix it's flock; on
// Windows it's LockFileEx. We don't need any extra dep for this.
use std::fs::TryLockError;

/// Holds the lock file open for the lifetime of the process. Kept in a
/// `OnceLock` so it can be acquired at the top of `main` and survive
/// across `run_with_tray` returning.
static LOCK_HOLDER: OnceLock<File> = OnceLock::new();

/// Acquire the per-user singleton lock. Returns Ok(()) on success
/// (the lock file is owned for the lifetime of the process) and an Err
/// describing the contention on failure.
pub fn acquire_or_fail(lock_dir: &Path) -> Result<()> {
    fs::create_dir_all(lock_dir)
        .with_context(|| format!("creating lock dir {}", lock_dir.display()))?;
    let path = lock_path(lock_dir);

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;

    match file.try_lock() {
        Ok(()) => {
            // Hold the file open for the rest of the process. The OS
            // releases the flock when the fd is closed (or the process
            // dies); never dropping the File keeps the lock held.
            LOCK_HOLDER
                .set(file)
                .map_err(|_| anyhow::anyhow!("singleton lock acquired twice"))?;
            Ok(())
        }
        Err(TryLockError::WouldBlock) => bail!(
            "another instance of clipboardwire is already running (lock held on {})",
            path.display()
        ),
        Err(TryLockError::Error(e)) => bail!(
            "could not test the singleton lock at {}: {e}",
            path.display()
        ),
    }
}

fn lock_path(dir: &Path) -> PathBuf {
    dir.join("clipboardwire.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "cw-instance-{label}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Re-locking from the *same* process after dropping the file
    /// handle works: flock is per-fd, not per-process.
    #[test]
    fn same_process_can_relock_after_drop() {
        let dir = unique_dir("relock");
        fs::create_dir_all(&dir).unwrap();
        let path = lock_path(&dir);

        // Acquire, drop, re-acquire — should succeed both times.
        {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            file.try_lock().expect("first acquire");
            // file dropped here → fd closed → lock released
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.try_lock().expect("relock after drop");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Two concurrent file handles to the same path conflict — this is
    /// the production case (two clipboardwire processes both opening
    /// the same lock file).
    #[test]
    fn two_handles_to_same_file_conflict() {
        let dir = unique_dir("conflict");
        fs::create_dir_all(&dir).unwrap();
        let path = lock_path(&dir);

        let a = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let b = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        a.try_lock().expect("a should acquire");
        assert!(
            matches!(b.try_lock(), Err(TryLockError::WouldBlock)),
            "second handle should not be able to acquire the exclusive lock"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
