//! Helpers that let concurrent Rune processes overlap safely: atomic replacement and per-unit locks.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub use dunce::simplified;

/// The 8.3 short spelling of a path (CMake reports paths this way). `None` off Windows.
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

/// Memoized for absolute paths: planning asks about the same directories hundreds of times.
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

/// `file:` URL spelled as Cargo spells package ids (`file:///C:/...`, percent-encoded).
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

pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = temp_sibling(path);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

pub const CACHEDIR_TAG: &str = "CACHEDIR.TAG";

/// Marks `dir` as a cache directory so backup tools and package hashing skip it.
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

/// Hard link (or copy) replacing `dest` atomically: readers never see a missing file.
pub fn replace_with_link(src: &Path, dest: &Path) -> io::Result<()> {
    let tmp = temp_sibling(dest);
    let staged = match fs::hard_link(src, &tmp) {
        Ok(()) => Ok(()),
        // Across file systems, or mounts of the same one (a container's bind mounts): a copy,
        // dated like the original so the next restore can tell it is one (`is_copy_of`).
        Err(_) => fs::copy(src, &tmp).map(|_| {
            let _ = fs::metadata(src)
                .and_then(|m| m.modified())
                .and_then(|t| open_for_times(&tmp)?.set_modified(t));
        }),
    };
    if let Err(e) = staged {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    rename_over(&tmp, dest).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// A handle that may change `path`'s timestamps, even when the file is read-only.
fn open_for_times(path: &Path) -> io::Result<fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        fs::File::open(path)
    }
}

/// Whether `dest` is a copy [`replace_with_link`] made of `src`: the same length, and the same
/// modification time to the nanosecond. Times in whole seconds can't tell copies apart: no.
pub fn is_copy_of(src: &Path, dest: &Path) -> bool {
    let (Ok(s), Ok(d)) = (fs::metadata(src), fs::metadata(dest)) else {
        return false;
    };
    if s.len() != d.len() || !d.is_file() {
        return false;
    }
    match (s.modified(), d.modified()) {
        (Ok(a), Ok(b)) => {
            a == b
                && a.duration_since(std::time::UNIX_EPOCH)
                    .is_ok_and(|t| t.subsec_nanos() != 0)
        }
        _ => false,
    }
}

/// [`fs::rename`] that also replaces read-only files (Windows refuses by default).
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
    const FILE_RENAME_INFO_EX: i32 = 22;
    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;
    const FILE_RENAME_FLAG_IGNORE_READONLY_ATTRIBUTE: u32 = 0x40;

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

    pub(super) fn rename_over_read_only(from: &Path, to: &Path) -> io::Result<()> {
        rename_ignoring_read_only(from, to).or_else(|_| rename_moving_aside(from, to))
    }

    pub(super) fn rename_ignoring_read_only(from: &Path, to: &Path) -> io::Result<()> {
        let file = OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(from)?;
        let name: Vec<u16> = std::path::absolute(to)?.as_os_str().encode_wide().collect();
        let name_bytes = name.len() * 2;
        let size = std::mem::size_of::<FileRenameInfo>() + name_bytes;
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

/// Released when dropped or when the process dies, so a crash never leaves a stale lock.
#[derive(Debug)]
pub struct FileLock {
    _file: Option<File>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquired {
    Immediately,
    AfterWaiting,
}

#[derive(Debug)]
pub enum TryLock {
    Locked(FileLock),
    Busy,
}

impl FileLock {
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

    /// Best effort: if locking is impossible here, the caller proceeds unlocked.
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
    fn copies_are_recognised_by_length_and_exact_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&a, b"artifact").unwrap();
        fs::copy(&a, &b).unwrap();
        let mtime = fs::metadata(&a).unwrap().modified().unwrap();
        let fresh = fs::metadata(&b).unwrap().modified().unwrap();
        assert_eq!(
            is_copy_of(&a, &b),
            fresh == mtime,
            "a plain copy has its own mtime"
        );
        open_for_times(&b).unwrap().set_modified(mtime).unwrap();
        let precise = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            != 0;
        assert_eq!(is_copy_of(&a, &b), precise);
        fs::write(&b, b"other length").unwrap();
        open_for_times(&b).unwrap().set_modified(mtime).unwrap();
        assert!(!is_copy_of(&a, &b), "a different length never is a copy");
    }

    /// `/dev/shm` and the temporary directory are separate mounts on Linux: hard links between
    /// them fail like those between a container's bind mounts.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_copy_made_across_mounts_is_recognised_next_time() {
        use std::os::unix::fs::MetadataExt;
        let Ok(shm) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let dev = |p: &Path| fs::metadata(p).map(|m| m.dev()).ok();
        if dev(shm.path()) == dev(tmp.path()) {
            return;
        }
        let src = shm.path().join("lib.rlib");
        fs::write(&src, vec![7u8; 4096]).unwrap();
        let dest = tmp.path().join("lib.rlib");
        replace_with_link(&src, &dest).unwrap();
        let ino = |p: &Path| fs::metadata(p).unwrap().ino();
        assert_ne!(ino(&src), ino(&dest), "a copy, not a link");
        assert!(is_copy_of(&src, &dest), "dated like the original");
    }

    #[test]
    fn replacing_a_link_never_leaves_the_destination_missing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dest = dir.path().join("dest");
        fs::write(&src, "new").unwrap();
        fs::write(&dest, "old").unwrap();
        replace_with_link(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new");
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
