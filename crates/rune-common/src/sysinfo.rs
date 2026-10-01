//! What the machine has to offer a build: cores and memory.

/// Physical memory, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    pub total: u64,
    /// What can be given to new processes without swapping (Linux's
    /// `MemAvailable`; on macOS, the share the kernel's memory-pressure level
    /// reports as available).
    pub available: u64,
}

impl Memory {
    /// Below this, starting another compiler risks swapping (or the
    /// out-of-memory killer): a tenth of the machine, at least 1 GiB and at
    /// most 4 GiB — a large crate's `rustc` can take a gigabyte or two.
    pub fn reserve(&self) -> u64 {
        const GIB: u64 = 1 << 30;
        (self.total / 10).clamp(GIB, 4 * GIB)
    }

    /// Whether memory is short enough to hold back new work.
    pub fn is_short(&self) -> bool {
        self.available < self.reserve()
    }
}

/// The machine's memory, where the platform says.
pub fn memory() -> Option<Memory> {
    imp::memory()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Memory;

    pub fn memory() -> Option<Memory> {
        parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
    }

    pub(super) fn parse_meminfo(text: &str) -> Option<Memory> {
        let field = |name: &str| -> Option<u64> {
            let line = text.lines().find(|l| l.starts_with(name))?;
            let kib: u64 = line[name.len()..]
                .trim_start_matches(':')
                .split_whitespace()
                .next()?
                .parse()
                .ok()?;
            Some(kib * 1024)
        };
        let total = field("MemTotal")?;
        let available = field("MemAvailable").or_else(|| field("MemFree"))?;
        Some(Memory { total, available })
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Memory;

    fn sysctl<T: Default + Copy>(name: &str) -> Option<T> {
        let name = std::ffi::CString::new(name).ok()?;
        let mut value = T::default();
        let mut len = std::mem::size_of::<T>();
        // SAFETY: `value` is a plain integer of `len` bytes; the kernel writes
        // at most `len` bytes and reports how many it wrote.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut value as *mut T).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && len == std::mem::size_of::<T>()).then_some(value)
    }

    pub fn memory() -> Option<Memory> {
        let total: u64 = sysctl("hw.memsize")?;
        // The percentage of memory the kernel considers available (what
        // Activity Monitor's "memory pressure" is derived from); unlike free
        // pages, it counts the file cache and compressible memory.
        let level: u32 = sysctl("kern.memorystatus_level")?;
        Some(Memory {
            total,
            available: total / 100 * u64::from(level.min(100)),
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use super::Memory;

    pub fn memory() -> Option<Memory> {
        None
    }
}

/// Lowers this process's scheduling priority (and so its children's): they
/// get the cores nothing else wants. Best effort.
pub fn lower_priority() {
    #[cfg(unix)]
    // SAFETY: plain system call on this process.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

/// The size in bytes of the filesystem holding `path`.
pub fn disk_size(path: &std::path::Path) -> Option<u64> {
    filesystem(path).map(|(total, _)| total)
}

/// The bytes an unprivileged process can still write on the filesystem
/// holding `path`.
pub fn disk_available(path: &std::path::Path) -> Option<u64> {
    filesystem(path).map(|(_, available)| available)
}

/// Total and available bytes of the filesystem holding `path` (or its
/// nearest existing ancestor).
fn filesystem(path: &std::path::Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut probe = path.to_path_buf();
        while !probe.exists() {
            probe = probe.parent()?.to_path_buf();
        }
        let c = std::ffi::CString::new(probe.as_os_str().as_bytes()).ok()?;
        // SAFETY: a zeroed `statvfs` is valid to be filled in; `c` is a valid path.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let unit = st.f_frsize as u64;
        Some((st.f_blocks as u64 * unit, st.f_bavail as u64 * unit))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn the_reserve_is_a_tenth_between_one_and_four_gib() {
        let m = |total| Memory {
            total,
            available: 0,
        };
        assert_eq!(m(8 * GIB).reserve(), GIB);
        assert_eq!(m(32 * GIB).reserve(), 32 * GIB / 10);
        assert_eq!(m(256 * GIB).reserve(), 4 * GIB);
        let fine = Memory {
            total: 32 * GIB,
            available: 20 * GIB,
        };
        assert!(!fine.is_short());
        let tight = Memory {
            total: 32 * GIB,
            available: 2 * GIB,
        };
        assert!(tight.is_short());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn meminfo_is_parsed() {
        let m = imp::parse_meminfo(
            "MemTotal:       16384000 kB\nMemFree:  100 kB\nMemAvailable:    8192000 kB\n",
        )
        .unwrap();
        assert_eq!(m.total, 16_384_000 * 1024);
        assert_eq!(m.available, 8_192_000 * 1024);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_machine_reports_its_memory() {
        let m = memory().expect("memory is known here");
        assert!(m.total > 0 && m.available <= m.total, "{m:?}");
    }
}
