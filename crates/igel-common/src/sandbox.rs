//! `build.sandbox`: build scripts and the compiler run with no network and a read-only file system except
//! their output and temp directories. `igl __sandbox` sets this up (Linux namespaces / macOS
//! `sandbox-exec`) and then `exec`s the program.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const HELPER_ARG: &str = "__sandbox";

#[derive(Debug, Clone)]
pub struct Sandbox {
    helper: PathBuf,
    tmp: PathBuf,
}

impl Sandbox {
    pub fn new(helper: PathBuf, tmp: PathBuf) -> Self {
        Self { helper, tmp }
    }

    pub fn command(&self, program: &OsStr, writable: &[&Path], network: bool) -> Command {
        let mut cmd = Command::new(&self.helper);
        cmd.arg(HELPER_ARG);
        if network {
            cmd.arg("--network");
        }
        for dir in writable.iter().copied().chain([self.tmp.as_path()]) {
            cmd.arg("--rw").arg(dir);
        }
        cmd.arg("--").arg(program);
        for var in ["TMPDIR", "TMP", "TEMP"] {
            cmd.env(var, &self.tmp);
        }
        cmd
    }

    pub fn wrap(&self, inner: &Command, writable: &[&Path], network: bool) -> Command {
        let mut cmd = self.command(inner.get_program(), writable, network);
        cmd.args(inner.get_args());
        for (k, v) in inner.get_envs().filter(|(k, _)| *k != "TMPDIR") {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        if let Some(dir) = inner.get_current_dir() {
            cmd.current_dir(dir);
        }
        cmd
    }

    pub fn probe(&self) -> Result<(), String> {
        let out = self
            .command(OsStr::new("true"), &[], false)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("could not start the sandbox helper: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }
}

/// `igl __sandbox [--network] [--rw DIR]... -- PROGRAM ARGS...`. Exits 125 if setup fails.
pub fn helper_main(args: &[OsString]) -> ! {
    let mut network = false;
    let mut writable: Vec<PathBuf> = Vec::new();
    let mut it = args.iter();
    let mut program: Vec<OsString> = Vec::new();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--network") => network = true,
            Some("--rw") => {
                if let Some(d) = it.next() {
                    writable.push(PathBuf::from(d));
                }
            }
            Some("--") => {
                program = it.cloned().collect();
                break;
            }
            _ => fail(&format!("unexpected argument {a:?}")),
        }
    }
    if program.is_empty() {
        fail("no program to run");
    }
    let writable: Vec<PathBuf> = writable
        .into_iter()
        .filter_map(|d| {
            let _ = std::fs::create_dir_all(&d);
            d.canonicalize().ok()
        })
        .collect();
    if let Err(e) = confine(&writable, network) {
        fail(&e);
    }
    exec(&program)
}

fn fail(message: &str) -> ! {
    eprintln!("igl sandbox: {message}");
    std::process::exit(125);
}

#[cfg(target_os = "macos")]
fn exec(program: &[OsString]) -> ! {
    exec_confined(program)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn exec(program: &[OsString]) -> ! {
    use std::os::unix::process::CommandExt;
    let err = Command::new(&program[0]).args(&program[1..]).exec();
    fail(&format!("could not run {:?}: {err}", program[0]))
}

#[cfg(not(unix))]
fn exec(_program: &[OsString]) -> ! {
    fail("the sandbox is not supported on this platform")
}

#[cfg(target_os = "linux")]
fn confine(writable: &[PathBuf], network: bool) -> Result<(), String> {
    use std::ffi::CString;
    let err = |what: &str| format!("{what}: {}", std::io::Error::last_os_error());
    // SAFETY: plain syscalls on a single-threaded process.
    unsafe {
        let uid = libc::getuid();
        let gid = libc::getgid();
        let mut flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS;
        if !network {
            flags |= libc::CLONE_NEWNET;
        }
        if libc::unshare(flags) != 0 {
            let mut why = err("cannot create namespaces");
            let restricted =
                std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
                    .is_ok_and(|v| v.trim() == "1");
            if restricted {
                why.push_str(
                    " — AppArmor restricts unprivileged user namespaces here (Ubuntu 23.10+); \
                     allow them with `sysctl kernel.apparmor_restrict_unprivileged_userns=0` \
                     or an AppArmor profile for igl",
                );
            } else {
                why.push_str(" — unprivileged user namespaces may be disabled");
            }
            return Err(why);
        }
        let _ = std::fs::write("/proc/self/setgroups", "deny");
        std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1"))
            .map_err(|e| format!("uid_map: {e}"))?;
        std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1"))
            .map_err(|e| format!("gid_map: {e}"))?;

        let root = CString::new("/").unwrap();
        if libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ) != 0
        {
            return Err(err("making mounts private"));
        }
        for dir in writable {
            let d = CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
            if libc::mount(
                d.as_ptr(),
                d.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            ) != 0
            {
                return Err(err(&format!("binding {}", dir.display())));
            }
        }
        let mounts = std::fs::read_to_string("/proc/self/mountinfo")
            .map_err(|e| format!("mountinfo: {e}"))?;
        for line in mounts.lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            let (Some(point), Some(opts)) = (fields.get(4), fields.get(5)) else {
                continue;
            };
            let point = unescape_mount_path(point);
            let path = Path::new(&point);
            if writable.iter().any(|w| path.starts_with(w)) {
                continue;
            }
            let mut flags = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY;
            for o in opts.split(',') {
                flags |= match o {
                    "nosuid" => libc::MS_NOSUID,
                    "nodev" => libc::MS_NODEV,
                    "noexec" => libc::MS_NOEXEC,
                    "noatime" => libc::MS_NOATIME,
                    "nodiratime" => libc::MS_NODIRATIME,
                    "relatime" => libc::MS_RELATIME,
                    _ => 0,
                };
            }
            let p = CString::new(point.as_bytes()).map_err(|e| e.to_string())?;
            if libc::mount(
                std::ptr::null(),
                p.as_ptr(),
                std::ptr::null(),
                flags,
                std::ptr::null(),
            ) != 0
            {
                // Kernel file systems refuse remounting from a user namespace.
                let special = ["/proc", "/sys", "/dev"]
                    .iter()
                    .any(|s| path.starts_with(s));
                let hidden = std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL);
                if !special && !hidden {
                    return Err(err(&format!("making {point} read-only")));
                }
            }
        }
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unescape_mount_path(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && let Ok(n) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(n);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(target_os = "macos")]
fn confine(writable: &[PathBuf], network: bool) -> Result<(), String> {
    let quote = |p: &Path| format!("{:?}", p.to_string_lossy());
    let mut profile = String::from("(version 1)\n(allow default)\n");
    if !network {
        profile.push_str("(deny network*)\n(allow network* (local unix))\n");
    }
    profile.push_str("(deny file-write*)\n(allow file-write*\n");
    for dir in writable {
        profile.push_str(&format!("  (subpath {})\n", quote(dir)));
    }
    profile.push_str(
        "  (literal \"/dev/null\") (literal \"/dev/tty\") (literal \"/dev/dtracehelper\")\n  (regex #\"^/dev/fd/\"))\n",
    );
    MACOS_PROFILE.with(|p| *p.borrow_mut() = Some(profile));
    Ok(())
}

#[cfg(target_os = "macos")]
thread_local! {
    static MACOS_PROFILE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

#[cfg(target_os = "macos")]
fn exec_confined(program: &[OsString]) -> ! {
    use std::os::unix::process::CommandExt;
    let profile = MACOS_PROFILE
        .with(|p| p.borrow().clone())
        .unwrap_or_default();
    let err = Command::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg(profile)
        .args(program)
        .exec();
    fail(&format!("could not run sandbox-exec: {err}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn confine(_writable: &[PathBuf], _network: bool) -> Result<(), String> {
    Err("the sandbox is not supported on this platform".into())
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    #[test]
    fn mount_paths_are_unescaped() {
        assert_eq!(unescape_mount_path("/a\\040b"), "/a b");
        assert_eq!(unescape_mount_path("/plain"), "/plain");
    }
}
