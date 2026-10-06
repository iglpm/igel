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

pub use dunce::simplified;

/// The 8.3 short spelling of an existing path (`C:\Users\me\DOCUME~1\...`),
/// when it differs from `path`. Tools driven by build scripts (CMake, notably)
/// report paths this way, so a path recognized by its long spelling alone
/// can be missed. Always `None` off Windows.
pub fn short_path(path: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetShortPathNameW(long: *const u16, short: *mut u16, len: u32) -> u32;
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut buf = vec![0u16; 512];
        loop {
            // SAFETY: `wide` is NUL-terminated and `buf` holds `buf.len()` units.
            let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) }
                as usize;
            if n == 0 {
                return None;
            }
            if n > buf.len() {
                buf.resize(n, 0);
                continue;
            }
            let short = PathBuf::from(OsString::from_wide(&buf[..n]));
            return (short.as_os_str() != path.as_os_str()).then_some(short);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// [`dunce::canonicalize`], remembered for the rest of the process for
/// absolute paths: planning asks about the same few directories (the
/// workspace's packages) hundreds of times, and every answer costs a system
/// call per path component. (A relative path depends on the working
/// directory, so it is always asked again.)
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static KNOWN: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    let path = path.as_ref();
    if !path.is_absolute() {
        return dunce::canonicalize(path);
    }
    let known = KNOWN.get_or_init(Default::default);
    if let Some(hit) = known.lock().unwrap_or_else(|e| e.into_inner()).get(path) {
        return Ok(hit.clone());
    }
    let resolved = dunce::canonicalize(path)?;
    known
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf(), resolved.clone());
    Ok(resolved)
}

/// `path` (absolute) as a `file:` URL, spelled as Cargo spells it in package
/// ids: `file:///home/me/app`, `file:///C:/Users/me/app` — forward slashes,
/// three of them before a drive, and characters a URL path cannot hold
/// percent-encoded (`My%20Projects`). Tools read package ids as URLs; a
/// Windows path pasted after `file://` is not one.
pub fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let slashed = text.replace('\\', "/");
    let mut url = String::from("file://");
    if !slashed.starts_with('/') {
        url.push('/');
    }
    for byte in slashed.bytes() {
        match byte {
            b'%' | b' ' | b'"' | b'#' | b'<' | b'>' | b'?' | b'`' | b'{' | b'}' => {
                url.push_str(&format!("%{byte:02X}"))
            }
            0..0x20 | 0x7F.. => url.push_str(&format!("%{byte:02X}")),
            _ => url.push(byte as char),
        }
    }
    url
}

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

/// The name of a [cache directory tag](https://bford.info/cachedir/).
pub const CACHEDIR_TAG: &str = "CACHEDIR.TAG";

/// Marks `dir` as a cache directory, as Cargo marks its target directory:
/// backup tools skip it, and so does Rune when it hashes a package's files
/// (a target directory or store that lives inside a package, `--target-dir
/// out` or a CI's `RUNE_HOME=$PWD/.cache`, must not count as its source).
pub fn tag_cache_dir(dir: &Path) {
    let tag = dir.join(CACHEDIR_TAG);
    if !tag.exists() {
        let _ = fs::create_dir_all(dir);
        let _ = fs::write(
            tag,
            "Signature: 8a477f597d28d172789f06886806bc55\n\
             # This file is a cache directory tag created by rune.\n\
             # For information about cache directory tags see https://bford.info/cachedir/\n",
        );
    }
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
    rename_over(&tmp, dest).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// [`fs::rename`], also when `to` is a read-only file — as every output Rune
/// restores or stores is. On Unix a rename replaces it regardless. Windows
/// refuses ("Access is denied"), and every switch back to an earlier build of
/// a unit (another branch, an edit undone) failed to restore it and compiled
/// it instead; there the replacement is still atomic where the file system
/// allows, and otherwise moves the old file aside for the moment it takes.
pub fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        #[cfg(windows)]
        Err(e)
            if e.kind() == io::ErrorKind::PermissionDenied
                && fs::symlink_metadata(to).is_ok_and(|m| m.permissions().readonly()) =>
        {
            windows::rename_over_read_only(from, to).map_err(|_| e)
        }
        result => result,
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    /// `FILE_INFO_BY_HANDLE_CLASS::FileRenameInfoEx`.
    const FILE_RENAME_INFO_EX: i32 = 22;
    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;
    const FILE_RENAME_FLAG_IGNORE_READONLY_ATTRIBUTE: u32 = 0x40;

    /// `FILE_RENAME_INFO` (the `Flags` member of its union), followed in the
    /// same allocation by the rest of `FileName`.
    #[repr(C)]
    struct FileRenameInfo {
        flags: u32,
        root_directory: *mut c_void,
        file_name_length: u32,
        file_name: [u16; 1],
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetFileInformationByHandle(
            file: *mut c_void,
            class: i32,
            info: *const c_void,
            size: u32,
        ) -> i32;
    }

    /// Replaces the read-only `to` with `from`.
    pub(super) fn rename_over_read_only(from: &Path, to: &Path) -> io::Result<()> {
        rename_ignoring_read_only(from, to).or_else(|_| rename_moving_aside(from, to))
    }

    /// One atomic rename that ignores the read-only attribute (Windows 10
    /// 1809 and later, on NTFS).
    pub(super) fn rename_ignoring_read_only(from: &Path, to: &Path) -> io::Result<()> {
        let file = OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(from)?;
        let name: Vec<u16> = std::path::absolute(to)?.as_os_str().encode_wide().collect();
        let name_bytes = name.len() * 2;
        let size = std::mem::size_of::<FileRenameInfo>() + name_bytes;
        // `u64`s: aligned for the struct, which holds a pointer.
        let mut buffer = vec![0u64; size.div_ceil(8)];
        let info = buffer.as_mut_ptr().cast::<FileRenameInfo>();
        // SAFETY: `buffer` is zeroed, suitably aligned, and large enough for
        // the header plus `name` (`size` counts the name in full on top of
        // the one-element array the header ends with).
        unsafe {
            (*info).flags = FILE_RENAME_FLAG_REPLACE_IF_EXISTS
                | FILE_RENAME_FLAG_POSIX_SEMANTICS
                | FILE_RENAME_FLAG_IGNORE_READONLY_ATTRIBUTE;
            (*info).root_directory = std::ptr::null_mut();
            (*info).file_name_length = name_bytes as u32;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                std::ptr::addr_of_mut!((*info).file_name).cast::<u16>(),
                name.len(),
            );
        }
        // SAFETY: a valid handle opened with `DELETE` access, and a
        // `FILE_RENAME_INFO` of `size` bytes.
        let ok = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle().cast(),
                FILE_RENAME_INFO_EX,
                info.cast(),
                size as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Elsewhere: the old file steps aside (a rename may move a read-only
    /// file, only not replace one), the new one takes its name, and the old
    /// one is deleted.
    pub(super) fn rename_moving_aside(from: &Path, to: &Path) -> io::Result<()> {
        let aside = super::temp_sibling(to);
        fs::rename(to, &aside)?;
        if let Err(e) = fs::rename(from, to) {
            let _ = fs::rename(&aside, to);
            return Err(e);
        }
        let _ = fs::remove_file(&aside);
        Ok(())
    }
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

/// The outcome of [`FileLock::try_exclusive`].
#[derive(Debug)]
pub enum TryLock {
    /// The lock is ours (or locking is unavailable here, and we proceed
    /// unlocked, as [`FileLock::exclusive`] does).
    Locked(FileLock),
    /// Another process holds it.
    Busy,
}

impl FileLock {
    /// Takes an exclusive lock on `path` if nobody holds it, without waiting.
    /// A scheduler uses this so that a unit another process is building does
    /// not tie up one of its own job slots while it waits.
    pub fn try_exclusive(path: &Path) -> TryLock {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let Ok(file) = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
        else {
            return TryLock::Locked(Self { _file: None });
        };
        #[cfg(unix)]
        {
            use rustix::fs::{FlockOperation, flock};
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => TryLock::Locked(Self { _file: Some(file) }),
                Err(e) if e == rustix::io::Errno::WOULDBLOCK => TryLock::Busy,
                Err(_) => TryLock::Locked(Self { _file: None }),
            }
        }
        #[cfg(not(unix))]
        {
            match file.try_lock() {
                Ok(()) => TryLock::Locked(Self { _file: Some(file) }),
                Err(std::fs::TryLockError::WouldBlock) => TryLock::Busy,
                Err(_) => TryLock::Locked(Self { _file: None }),
            }
        }
    }

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
    fn file_urls_are_spelled_like_cargos() {
        assert_eq!(file_url(Path::new("/home/me/app")), "file:///home/me/app");
        assert_eq!(
            file_url(Path::new(r"C:\Users\me\My Projects\a#1")),
            "file:///C:/Users/me/My%20Projects/a%231"
        );
        assert_eq!(
            file_url(Path::new(r"\\?\C:\work\app")),
            "file:///C:/work/app"
        );
        assert_eq!(
            file_url(Path::new("/srv/münchen")),
            "file:///srv/m%C3%BCnchen"
        );
    }

    fn read_only(path: &Path) {
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn a_read_only_destination_is_replaced_too() {
        // Restored and stored outputs are read-only, and hard links to a
        // shared object: replacing one must neither fail nor touch the object.
        let dir = tempfile::tempdir().unwrap();
        let object = dir.path().join("object");
        let dest = dir.path().join("dest");
        let src = dir.path().join("src");
        fs::write(&object, "old").unwrap();
        read_only(&object);
        fs::hard_link(&object, &dest).unwrap();
        fs::write(&src, "new").unwrap();
        read_only(&src);
        replace_with_link(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new");
        assert_eq!(fs::read_to_string(&object).unwrap(), "old");
        assert!(fs::metadata(&object).unwrap().permissions().readonly());
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // And once more over the now read-only replacement.
        let newer = dir.path().join("newer");
        fs::write(&newer, "newer").unwrap();
        rename_over(&newer, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "newer");
    }

    #[cfg(windows)]
    #[test]
    fn ntfs_replaces_a_read_only_file_in_one_rename() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        let src = dir.path().join("src");
        fs::write(&dest, "old").unwrap();
        read_only(&dest);
        fs::write(&src, "new").unwrap();
        windows::rename_ignoring_read_only(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new");
        assert!(!src.exists());
    }

    #[cfg(windows)]
    #[test]
    fn moving_a_read_only_file_aside_replaces_it_too() {
        // Where the file system cannot rename over a read-only file at once.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        let src = dir.path().join("src");
        fs::write(&dest, "old").unwrap();
        read_only(&dest);
        fs::write(&src, "new").unwrap();
        windows::rename_moving_aside(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
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

    #[cfg(unix)]
    #[test]
    fn trying_a_held_lock_says_busy_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locks").join("unit");
        let first = match FileLock::try_exclusive(&path) {
            TryLock::Locked(l) => l,
            TryLock::Busy => panic!("nobody holds it yet"),
        };
        let other = std::thread::spawn({
            let path = path.clone();
            move || matches!(FileLock::try_exclusive(&path), TryLock::Busy)
        });
        assert!(
            other.join().unwrap(),
            "a second taker must be told it is busy"
        );
        drop(first);
        assert!(matches!(FileLock::try_exclusive(&path), TryLock::Locked(_)));
    }
}
