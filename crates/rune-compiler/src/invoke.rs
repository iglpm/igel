use crate::output::{ArtifactEvent, CompilationResult, parse_rustc_output};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(thiserror::Error, Debug)]
pub enum InvokeError {
    #[error("I/O error during invocation: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, InvokeError>;

pub struct RustcInvoker {
    rustc_path: PathBuf,
}

impl Default for RustcInvoker {
    fn default() -> Self {
        Self::new()
    }
}

impl RustcInvoker {
    pub fn new() -> Self {
        let rustc_path = std::env::var_os("RUSTC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("rustc"));
        Self { rustc_path }
    }

    pub fn rustc_path(&self) -> &PathBuf {
        &self.rustc_path
    }

    pub fn invoke(&self, mut cmd: Command) -> Result<CompilationResult> {
        let output = cmd.output()?;
        let stderr_raw = String::from_utf8_lossy(&output.stderr).into_owned();
        let (artifacts, diagnostics) = parse_rustc_output(&stderr_raw);

        Ok(CompilationResult {
            peak_memory: None,
            success: output.status.success(),
            exit_code: output.status.code().unwrap_or(1),
            artifacts,
            diagnostics,
            stderr_raw,
        })
    }

    pub fn invoke_pipelined(
        &self,
        cmd: Command,
        on_metadata: impl FnOnce(PathBuf),
    ) -> Result<CompilationResult> {
        let mut on_metadata = Some(on_metadata);
        self.invoke_streaming(cmd, &mut |artifact| {
            if artifact.emit == "metadata"
                && let Some(cb) = on_metadata.take()
            {
                cb(artifact.artifact.clone());
            }
        })
    }

    /// Calls `on_artifact` as rustc announces each artifact; dep-info comes before metadata.
    pub fn invoke_streaming(
        &self,
        mut cmd: Command,
        on_artifact: &mut dyn FnMut(&ArtifactEvent),
    ) -> Result<CompilationResult> {
        cmd.stderr(Stdio::piped());
        cmd.stdout(Stdio::null());

        let mut child = cmd.spawn()?;
        let stderr = child.stderr.take().unwrap();

        let mut stderr_raw = String::new();
        let mut artifacts = Vec::new();
        let mut diagnostics = Vec::new();
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();

        while reader.read_line(&mut line)? > 0 {
            stderr_raw.push_str(&line);

            if line.starts_with('{')
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
            {
                if value.get("$message_type").and_then(|v| v.as_str()) == Some("artifact") {
                    if let Ok(artifact) = serde_json::from_value::<ArtifactEvent>(value.clone()) {
                        on_artifact(&artifact);
                        artifacts.push(artifact);
                    }
                } else if value.get("message").is_some()
                    && value.get("level").is_some()
                    && let Ok(diag) = serde_json::from_value::<crate::output::RawDiagnostic>(value)
                {
                    diagnostics.push(crate::output::Diagnostic {
                        level: diag.level,
                        message: diag.message,
                        rendered: diag.rendered,
                    });
                }
            }
            line.clear();
        }

        let (status, peak_memory) = wait_with_peak_memory(&mut child)?;

        Ok(CompilationResult {
            success: status.success(),
            exit_code: status.code().unwrap_or(1),
            artifacts,
            diagnostics,
            stderr_raw,
            peak_memory,
        })
    }
}

#[cfg(unix)]
fn wait_with_peak_memory(
    child: &mut std::process::Child,
) -> std::io::Result<(std::process::ExitStatus, Option<u64>)> {
    use std::os::unix::process::ExitStatusExt;
    let pid = child.id() as libc::pid_t;
    let mut status: libc::c_int = 0;
    // SAFETY: an all-zero `rusage` is a valid value to be overwritten.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `pid` is our own unreaped child; the pointers are valid.
        let r = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
        if r == pid {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return child.wait().map(|s| (s, None));
        }
    }
    // Linux reports kilobytes, macOS bytes.
    let max = usage.ru_maxrss.max(0) as u64;
    let bytes = if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024
    };
    Ok((std::process::ExitStatus::from_raw(status), Some(bytes)))
}

#[cfg(windows)]
fn wait_with_peak_memory(
    child: &mut std::process::Child,
) -> std::io::Result<(std::process::ExitStatus, Option<u64>)> {
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut Counters,
            size: u32,
        ) -> i32;
    }

    let status = child.wait()?;
    let mut counters = Counters {
        cb: std::mem::size_of::<Counters>() as u32,
        ..Counters::default()
    };
    // SAFETY: the child's process handle (still open: `child` owns it) and
    // a `PROCESS_MEMORY_COUNTERS` of the size it says.
    let ok = unsafe {
        K32GetProcessMemoryInfo(child.as_raw_handle().cast(), &mut counters, counters.cb)
    };
    let peak = (ok != 0 && counters.peak_working_set_size > 0)
        .then_some(counters.peak_working_set_size as u64);
    Ok((status, peak))
}

#[cfg(not(any(unix, windows)))]
fn wait_with_peak_memory(
    child: &mut std::process::Child,
) -> std::io::Result<(std::process::ExitStatus, Option<u64>)> {
    child.wait().map(|s| (s, None))
}
