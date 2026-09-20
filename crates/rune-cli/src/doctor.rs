//! `rune doctor`: looks at the machine, the toolchain and the project and says
//! what slows builds down or makes them fragile, with the exact fix.
//!
//! The judgement lives in [`diagnose`], a pure function of an [`Environment`],
//! so every recommendation is unit-tested; gathering the environment is the
//! thin, untested part.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Nothing to do.
    Ok,
    /// Worth knowing.
    Note,
    /// Costs time or risks a problem.
    Warn,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub title: String,
    pub detail: Option<String>,
    /// What to run or write to fix it.
    pub fix: Option<String>,
}

impl Finding {
    fn ok(title: impl Into<String>) -> Self {
        Self {
            level: Level::Ok,
            title: title.into(),
            detail: None,
            fix: None,
        }
    }

    fn note(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            level: Level::Note,
            title: title.into(),
            detail: Some(detail.into()),
            fix: None,
        }
    }

    fn warn(title: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            level: Level::Warn,
            title: title.into(),
            detail: Some(detail.into()),
            fix: Some(fix.into()),
        }
    }
}

/// Everything the diagnosis depends on.
pub struct Environment {
    /// `1.98.1`, from `rustc -vV`.
    pub rustc_release: Option<String>,
    pub host: Option<String>,
    /// Whether an executable of this name is on `PATH`.
    pub on_path: Box<dyn Fn(&str) -> bool>,
    /// Flags from `RUSTFLAGS` and the configuration files.
    pub rustflags: Vec<String>,
    /// `target.<host>.linker` from the configuration, if set.
    pub configured_linker: Option<String>,
    /// The manifest sets `[profile.dev] debug` to full explicitly.
    pub dev_debug_full_explicit: bool,
    /// `build.dependency-debuginfo = true`: dependencies get full debuginfo.
    pub full_dependency_debuginfo: bool,
    /// Where settings, the store and downloads live.
    pub home: String,
    pub home_fs: Option<String>,
    pub cas_bytes: u64,
    /// Bytes in the store not used for 30 days.
    pub cas_idle_bytes: u64,
    pub jobs: usize,
    pub cpus: usize,
    pub rustc_wrapper: Option<String>,
    /// The project has a `Rune.toml` manifest *and* a `Cargo.toml` manifest: Rune
    /// reads the first, plain `cargo` reads the second.
    pub shadowed_cargo_toml: bool,
}

/// The file-system type of the mount `path` lives on, from `/proc/mounts` text.
pub fn fs_type_of(path: &Path, mounts: &str) -> Option<String> {
    mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (_dev, mount, fs) = (f.next()?, f.next()?, f.next()?);
            let mount = mount.replace("\\040", " ");
            path.starts_with(&mount)
                .then(|| (mount.len(), fs.to_string()))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, fs)| fs)
}

/// `(major, minor)` of a `1.98.1`-style release.
fn parse_release(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.split(['.', '-']);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn human(bytes: u64) -> String {
    let gib = bytes as f64 / (1u64 << 30) as f64;
    if gib >= 1.0 {
        format!("{gib:.1} GiB")
    } else {
        format!("{:.0} MiB", bytes as f64 / (1u64 << 20) as f64)
    }
}

pub fn diagnose(env: &Environment) -> Vec<Finding> {
    let mut out = Vec::new();
    let has = |tool: &str| (env.on_path)(tool);

    // --- the tools Rune itself needs --------------------------------------
    match &env.rustc_release {
        Some(r) => out.push(Finding::ok(format!("rustc {r}"))),
        None => out.push(Finding::warn(
            "rustc was not found",
            "Nothing can be built without a compiler.",
            "install a toolchain: https://rustup.rs",
        )),
    }
    if !has("cc") && !has("gcc") && !has("clang") {
        out.push(Finding::warn(
            "no C compiler on PATH",
            "Linking needs one, and so do build scripts of crates with native code.",
            "install one (Debian/Ubuntu: `apt install build-essential`, Fedora: `dnf install gcc`, Arch: `pacman -S base-devel`)",
        ));
    }
    if !has("git") {
        out.push(Finding::warn(
            "git is not installed",
            "Git dependencies are fetched with the `git` binary.",
            "install git (only needed if you have `git = \"…\"` dependencies)",
        ));
    }
    if !has("pkg-config") {
        out.push(Finding::note(
            "pkg-config is not installed",
            "Crates that link system libraries (openssl-sys, libz-sys, …) look them up with it.",
        ));
    }

    // --- linking ------------------------------------------------------------
    let linux_x86 = env.host.as_deref() == Some("x86_64-unknown-linux-gnu");
    let already_lld = linux_x86
        && env
            .rustc_release
            .as_deref()
            .and_then(parse_release)
            .is_some_and(|v| v >= (1, 90));
    let linker_chosen = env.configured_linker.is_some()
        || env
            .rustflags
            .iter()
            .any(|f| f.contains("fuse-ld") || f.starts_with("linker=") || f.contains("-Clinker"));
    if linker_chosen {
        out.push(Finding::ok("linker configured explicitly"));
    } else if let Some(fast) = ["mold", "wild"].into_iter().find(|t| has(t)) {
        let host = env.host.as_deref().unwrap_or("<your target triple>");
        out.push(Finding::warn(
            format!("`{fast}` is installed but not used for linking"),
            "Linking is the one step that always starts from scratch, and `mold`/`wild` are several times faster than the default.",
            format!(
                "add to .cargo/config.toml:\n    [target.{host}]\n    rustflags = [\"-C\", \"link-arg=-fuse-ld={fast}\"]"
            ),
        ));
    } else if !already_lld && has("ld.lld") {
        let host = env.host.as_deref().unwrap_or("<your target triple>");
        out.push(Finding::warn(
            "`lld` is installed but not used for linking",
            "The default GNU linker is much slower.",
            format!(
                "add to .cargo/config.toml:\n    [target.{host}]\n    rustflags = [\"-C\", \"link-arg=-fuse-ld=lld\"]"
            ),
        ));
    } else if already_lld {
        out.push(Finding::note(
            "linking with rust-lld (the default since Rust 1.90)",
            "Installing `mold` or `wild` would make linking faster still.",
        ));
    } else {
        out.push(Finding::note(
            "linking with the system linker",
            "Installing `mold` or `lld` would make linking noticeably faster.",
        ));
    }

    // --- profile ---------------------------------------------------------
    if env.dev_debug_full_explicit {
        out.push(Finding::warn(
            "`[profile.dev] debug` is set to full",
            "Full debuginfo costs about 10–20 % of a cold build and is rarely needed beyond line numbers.",
            "use `debug = \"line-tables-only\"` (backtraces and breakpoints keep working), or `debug = false`",
        ));
    } else if env.full_dependency_debuginfo {
        out.push(Finding::note(
            "dependencies are built with full debuginfo (`build.dependency-debuginfo`)",
            "That makes cold builds about 10 % slower; it only helps if you step through dependency code.",
        ));
    } else {
        out.push(Finding::ok(
            "dev profile: dependencies use line-tables-only debuginfo",
        ));
    }

    // --- machine ------------------------------------------------------------
    if env.jobs < env.cpus {
        out.push(Finding::warn(
            format!("only {} parallel jobs on {} CPUs", env.jobs, env.cpus),
            "Some cores will sit idle during a build.",
            "remove `build.jobs` / `-j`, or raise it (`rune config set build.jobs <n>`)",
        ));
    }
    if matches!(env.home_fs.as_deref(), Some("tmpfs" | "ramfs")) {
        out.push(Finding::warn(
            format!("Rune's home ({}) is on a RAM-backed file system", env.home),
            "The store and downloads live in memory: they vanish on reboot and a big build can fill RAM.",
            "point RUNE_HOME at a disk-backed directory, e.g. `export RUNE_HOME=~/.cache/rune`",
        ));
    }
    if let Some(w) = &env.rustc_wrapper {
        out.push(Finding::note(
            format!("RUSTC_WRAPPER is set ({w})"),
            "It is part of every unit's identity, so switching it rebuilds everything once. \
             Rune's own store makes `sccache` unnecessary for local builds.",
        ));
    }

    // --- manifests ----------------------------------------------------------
    if env.shadowed_cargo_toml {
        out.push(Finding::warn(
            "both `Rune.toml` and `Cargo.toml` define this project",
            "Rune reads `Rune.toml`; plain `cargo` reads `Cargo.toml`, so the two can drift apart \
             (different dependencies or features, different builds).",
            "keep one of them as the manifest — a `Rune.toml` with only settings (no `[package]` \
             or `[workspace]`) is fine next to a `Cargo.toml`",
        ));
    }

    // --- the store ------------------------------------------------------------
    out.push(Finding::ok(format!("store holds {}", human(env.cas_bytes))));
    if env.cas_idle_bytes >= (1 << 30) && env.cas_idle_bytes * 4 >= env.cas_bytes {
        out.push(Finding::warn(
            format!(
                "{} of the store has not been used for 30 days",
                human(env.cas_idle_bytes)
            ),
            "Old entries are only ever needed if you go back to an old dependency version.",
            "rune cas gc --max-age 2592000     (see `rune cas du` for what takes the space)",
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Environment {
        Environment {
            rustc_release: Some("1.98.1".into()),
            host: Some("x86_64-unknown-linux-gnu".into()),
            on_path: Box::new(|t| matches!(t, "cc" | "git" | "pkg-config")),
            rustflags: vec![],
            configured_linker: None,
            dev_debug_full_explicit: false,
            full_dependency_debuginfo: false,
            home: "/home/me/.rune".into(),
            home_fs: Some("ext4".into()),
            cas_bytes: 2 << 30,
            cas_idle_bytes: 0,
            jobs: 16,
            cpus: 12,
            rustc_wrapper: None,
            shadowed_cargo_toml: false,
        }
    }

    fn warnings(f: &[Finding]) -> Vec<&str> {
        f.iter()
            .filter(|f| f.level == Level::Warn)
            .map(|f| f.title.as_str())
            .collect()
    }

    #[test]
    fn a_healthy_setup_has_no_warnings() {
        assert!(warnings(&diagnose(&env())).is_empty());
    }

    #[test]
    fn an_installed_but_unused_fast_linker_is_recommended_with_the_config_to_add() {
        let mut e = env();
        e.on_path = Box::new(|t| matches!(t, "cc" | "git" | "pkg-config" | "mold"));
        let f = diagnose(&e);
        let w = f.iter().find(|f| f.level == Level::Warn).unwrap();
        assert!(w.title.contains("mold"));
        let fix = w.fix.as_deref().unwrap();
        assert!(fix.contains("[target.x86_64-unknown-linux-gnu]"));
        assert!(fix.contains("-fuse-ld=mold"));
    }

    #[test]
    fn a_configured_linker_is_left_alone() {
        let mut e = env();
        e.on_path = Box::new(|t| matches!(t, "cc" | "git" | "pkg-config" | "mold"));
        e.rustflags = vec!["-Clink-arg=-fuse-ld=mold".into()];
        assert!(warnings(&diagnose(&e)).is_empty());
        e.rustflags.clear();
        e.configured_linker = Some("clang".into());
        assert!(warnings(&diagnose(&e)).is_empty());
    }

    #[test]
    fn lld_is_not_recommended_where_it_is_already_the_default() {
        let mut e = env();
        e.on_path = Box::new(|t| matches!(t, "cc" | "git" | "pkg-config" | "ld.lld"));
        assert!(
            warnings(&diagnose(&e)).is_empty(),
            "1.98 on x86_64 linux already uses lld"
        );
        e.rustc_release = Some("1.85.0".into());
        assert!(warnings(&diagnose(&e))[0].contains("lld"));
    }

    #[test]
    fn full_debuginfo_in_the_dev_profile_is_flagged() {
        let mut e = env();
        e.dev_debug_full_explicit = true;
        assert!(warnings(&diagnose(&e))[0].contains("debug"));
    }

    #[test]
    fn full_dependency_debuginfo_is_noted_not_nagged() {
        let mut e = env();
        e.full_dependency_debuginfo = true;
        let f = diagnose(&e);
        assert!(warnings(&f).is_empty());
        assert!(
            f.iter()
                .any(|f| f.level == Level::Note && f.title.contains("full debuginfo"))
        );
    }

    #[test]
    fn two_competing_manifests_are_flagged() {
        let mut e = env();
        e.shadowed_cargo_toml = true;
        let f = diagnose(&e);
        assert!(warnings(&f)[0].contains("both `Rune.toml` and `Cargo.toml`"));
    }

    #[test]
    fn a_store_in_ram_is_flagged() {
        let mut e = env();
        e.home = "/tmp/rune".into();
        e.home_fs = Some("tmpfs".into());
        assert!(warnings(&diagnose(&e))[0].contains("RAM-backed"));
    }

    #[test]
    fn a_mostly_idle_store_suggests_gc() {
        let mut e = env();
        e.cas_bytes = 10 << 30;
        e.cas_idle_bytes = 6 << 30;
        let f = diagnose(&e);
        let w = f.iter().find(|f| f.level == Level::Warn).unwrap();
        assert!(w.fix.as_deref().unwrap().contains("cas gc --max-age"));
        // A small idle remainder is not worth a warning.
        e.cas_idle_bytes = 100 << 20;
        assert!(warnings(&diagnose(&e)).is_empty());
    }

    #[test]
    fn missing_tools_are_reported() {
        let mut e = env();
        e.on_path = Box::new(|_| false);
        e.rustc_release = None;
        let titles = warnings(&diagnose(&e)).join("|");
        assert!(titles.contains("rustc was not found"));
        assert!(titles.contains("no C compiler"));
        assert!(titles.contains("git is not installed"));
    }

    #[test]
    fn too_few_jobs_are_flagged() {
        let mut e = env();
        e.jobs = 4;
        assert!(warnings(&diagnose(&e))[0].contains("only 4 parallel jobs"));
    }

    #[test]
    fn the_file_system_is_found_from_the_longest_matching_mount() {
        let mounts =
            "/dev/sda1 / ext4 rw 0 0\ntmpfs /tmp tmpfs rw 0 0\n/dev/sdb1 /tmp/disk ext4 rw 0 0\n";
        assert_eq!(
            fs_type_of(Path::new("/home/me"), mounts).as_deref(),
            Some("ext4")
        );
        assert_eq!(
            fs_type_of(Path::new("/tmp/x/y"), mounts).as_deref(),
            Some("tmpfs")
        );
        assert_eq!(
            fs_type_of(Path::new("/tmp/disk/x"), mounts).as_deref(),
            Some("ext4")
        );
    }
}
