//! Running one command and measuring it: wall time, CPU time (user and
//! system, the command and every process it waited for — compilers,
//! linkers, test binaries) and the peak memory of its largest process.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::time::{Duration, Instant};

/// One measured run.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Sample {
    /// Seconds from start to exit.
    pub wall: f64,
    /// CPU seconds in user mode, the process tree included.
    pub user: f64,
    /// CPU seconds in the kernel, the process tree included.
    pub sys: f64,
    /// Peak resident memory of the largest single process, in KiB.
    pub max_rss_kib: u64,
    /// Exited successfully (and within the time limit).
    pub ok: bool,
}

impl Sample {
    pub fn cpu(&self) -> f64 {
        self.user + self.sys
    }
}

/// What a run printed, for when it failed.
pub struct Outcome {
    pub sample: Sample,
    pub output: String,
}

/// Runs `cmd` to completion (killing it after `timeout`) and measures it.
pub fn run(cmd: &mut Command, timeout: Duration) -> Result<Outcome> {
    use std::process::Stdio;
    let log = tempfile_path();
    let file = std::fs::File::create(&log).context("creating a log file")?;
    let err = file.try_clone()?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(err));
    let started = Instant::now();
    let child = cmd
        .spawn()
        .with_context(|| format!("starting {:?}", cmd.get_program()))?;
    let sample = wait(child, started, timeout)?;
    let output = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    Ok(Outcome { sample, output })
}

fn tempfile_path() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "rune-bench-{}-{}.log",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(unix)]
fn wait(child: std::process::Child, started: Instant, timeout: Duration) -> Result<Sample> {
    // `wait4` reports the child's resource use including every descendant it
    // waited for, which is how a build tool's compilers are counted.
    let pid = child.id() as libc::pid_t;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut status: libc::c_int = 0;
        // SAFETY: a zeroed rusage is a valid out-parameter.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `pid` is our child; nothing else waits for it.
        let r = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
        let _ = tx.send((r, status, usage, Instant::now()));
    });
    let (r, status, usage, ended, timed_out) = match rx.recv_timeout(timeout) {
        Ok((r, s, u, e)) => (r, s, u, e, false),
        Err(_) => {
            // SAFETY: plain kill of our own child.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            let (r, s, u, e) = rx.recv().context("waiting for a killed child")?;
            (r, s, u, e, true)
        }
    };
    // The `Child` must not try to wait again.
    std::mem::forget(child);
    anyhow::ensure!(
        r == pid,
        "wait4 failed: {}",
        std::io::Error::last_os_error()
    );
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    let exited_ok = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
    // Linux reports ru_maxrss in KiB, macOS in bytes.
    let max_rss_kib = if cfg!(target_os = "macos") {
        usage.ru_maxrss as u64 / 1024
    } else {
        usage.ru_maxrss as u64
    };
    Ok(Sample {
        wall: (ended - started).as_secs_f64(),
        user: secs(usage.ru_utime),
        sys: secs(usage.ru_stime),
        max_rss_kib,
        ok: exited_ok && !timed_out,
    })
}

#[cfg(not(unix))]
fn wait(mut child: std::process::Child, started: Instant, timeout: Duration) -> Result<Sample> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Sample {
                wall: started.elapsed().as_secs_f64(),
                ok: status.success(),
                ..Sample::default()
            });
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Sample {
                wall: started.elapsed().as_secs_f64(),
                ..Sample::default()
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Bytes of the files under `dir` (hard links to one file counted once).
pub fn dir_size(dir: &std::path::Path) -> u64 {
    dirs_size(&[dir])
}

/// Bytes of the files under all of `dirs`, a file hard-linked into several
/// of them counted once (Rune's build output links to its store).
pub fn dirs_size(dirs: &[&std::path::Path]) -> u64 {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    #[cfg(unix)]
    let mut seen: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    // (Windows: the volume and file index identify a file; std has no stable
    // accessor for them, so a handle is opened per file.)
    #[cfg(windows)]
    let mut seen: std::collections::HashSet<same_file::Handle> = std::collections::HashSet::new();
    let mut total = 0;
    for entry in dirs
        .iter()
        .flat_map(|d| walkdir::WalkDir::new(d).into_iter().flatten())
    {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        #[cfg(unix)]
        if !seen.insert((meta.dev(), meta.ino())) {
            continue;
        }
        #[cfg(windows)]
        if let Ok(handle) = same_file::Handle::from_path(entry.path())
            && !seen.insert(handle)
        {
            continue;
        }
        total += meta.len();
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_is_timed_and_its_exit_status_kept() {
        let ok = run(
            Command::new("sh").args(["-c", "echo hi"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(ok.sample.ok);
        assert!(ok.output.contains("hi"));
        let bad = run(
            Command::new("sh").args(["-c", "exit 3"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(!bad.sample.ok);
    }

    #[test]
    fn a_command_past_its_time_is_stopped() {
        let t = Instant::now();
        let out = run(
            Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(200),
        )
        .unwrap();
        assert!(!out.sample.ok);
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn hard_links_count_once() {
        let dir = std::env::temp_dir().join(format!("rune-bench-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), vec![0u8; 1000]).unwrap();
        std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
        std::fs::write(dir.join("c"), vec![0u8; 10]).unwrap();
        assert_eq!(dir_size(&dir), 1010);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
