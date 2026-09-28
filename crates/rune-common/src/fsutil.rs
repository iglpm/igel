//! File-system helpers that keep concurrent Rune processes from breaking each
//! other: atomic replacement of build outputs and per-unit advisory locks.
//!
//! Two `rune` processes on one project (an editor's background check and a
//! terminal build, CI shards, two agents in one checkout) share the target
//! directory and the content-addressable store. Cargo serialises them with one
//! lock on the whole directory; Rune instead makes every step safe to overlap
//! and only makes processes wait when they would build the *same unit*.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A name next to `dest` that no other process or thread will pick.
fn temp_sibling(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Writes `path` whole or not at all: a temporary file renamed over it, so a
/// concurrent reader never sees half of it.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = temp_sibling(path);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

/// Makes `dest` a hard link to `src` (or a copy, where linking is impossible),
/// replacing whatever was there **atomically**: a reader that opens `dest` at
/// any moment sees either the old file or the new one, never a missing file.
/// (Removing `dest` first, as one might, leaves a window in which another
/// process's compiler finds nothing.)
pub fn replace_with_link(src: &Path, dest: &Path) -> io::Result<()> {
    let tmp = temp_sibling(dest);
    let staged = match fs::hard_link(src, &tmp) {
        Ok(()) => Ok(()),
        Err(_) => fs::copy(src, &tmp).map(|_| ()),
    };
    if let Err(e) = staged {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, dest).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// An exclusive advisory lock on a file, released when dropped (or when the
/// process dies — the kernel drops it, so a crash never leaves a stale lock).
#[derive(Debug)]
pub struct FileLock {
    _file: Option<File>,
}

/// How a lock was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquired {
    /// Nobody held it.
    Immediately,
    /// Someone else held it and has released it since: whatever they were
    /// producing may be ready now.
    AfterWaiting,
}

impl FileLock {
    /// Takes an exclusive lock on `path` (created if needed), blocking until it
    /// is available. Locking is best effort: if the lock file cannot be
    /// created or locked (read-only directory, unsupported file system), the
    /// caller proceeds unlocked rather than failing the build.
    pub fn exclusive(path: &Path) -> (Self, Acquired) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let Ok(file) = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
        else {
            return (Self { _file: None }, Acquired::Immediately);
        };
        #[cfg(unix)]
        {
            use rustix::fs::{FlockOperation, flock};
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => (Self { _file: Some(file) }, Acquired::Immediately),
                Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                    match flock(&file, FlockOperation::LockExclusive) {
                        Ok(()) => (Self { _file: Some(file) }, Acquired::AfterWaiting),
                        Err(_) => (Self { _file: None }, Acquired::Immediately),
                    }
                }
                Err(_) => (Self { _file: None }, Acquired::Immediately),
            }
        }
        #[cfg(not(unix))]
        {
            // The standard library's locks (`LockFileEx` on Windows).
            match file.try_lock() {
                Ok(()) => (Self { _file: Some(file) }, Acquired::Immediately),
                Err(std::fs::TryLockError::WouldBlock) => match file.lock() {
                    Ok(()) => (Self { _file: Some(file) }, Acquired::AfterWaiting),
                    Err(_) => (Self { _file: None }, Acquired::Immediately),
                },
                Err(_) => (Self { _file: None }, Acquired::Immediately),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn replacing_a_link_never_leaves_the_destination_missing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dest = dir.path().join("dest");
        fs::write(&src, "new").unwrap();
        fs::write(&dest, "old").unwrap();
        replace_with_link(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new");
        // No temporary files are left behind.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_reader_racing_a_replacement_always_finds_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dest = dir.path().join("dest");
        fs::write(&src, "payload").unwrap();
        fs::write(&dest, "payload").unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let missing = Arc::new(AtomicUsize::new(0));
        let reader = {
            let (dest, stop, missing) = (dest.clone(), stop.clone(), missing.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if fs::read(&dest).is_err() {
                        missing.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        };
        for _ in 0..2000 {
            replace_with_link(&src, &dest).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();
        assert_eq!(missing.load(Ordering::Relaxed), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_second_taker_waits_and_is_told_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locks").join("unit");
        let (first, how) = FileLock::exclusive(&path);
        assert_eq!(how, Acquired::Immediately);
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || FileLock::exclusive(&path).1)
        };
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(!waiter.is_finished(), "must block while the lock is held");
        drop(first);
        assert_eq!(waiter.join().unwrap(), Acquired::AfterWaiting);
    }
}
