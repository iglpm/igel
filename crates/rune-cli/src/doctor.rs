//! `rune doctor`. The judgement is the pure, tested [`diagnose`]; gathering the environment is not.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Note,
    Warn,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub title: String,
    pub detail: Option<String>,
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

pub struct Environment {
    pub rustc_release: Option<String>,
    pub host: Option<String>,
    pub on_path: Box<dyn Fn(&str) -> bool>,
    pub rustflags: Vec<String>,
    pub configured_linker: Option<String>,
    /// Rune's `build.linker` setting, set or default.
    pub rune_linker: String,
    /// rustc-runified's version, when it compiles in place of `rustc` (`build.compiler`).
    pub runified: Option<String>,
    pub dev_debug_full_explicit: bool,
    pub full_dependency_debuginfo: bool,
    pub home: String,
    pub home_fs: Option<String>,
    pub cas_bytes: u64,
    pub cas_idle_bytes: u64,
    pub jobs: usize,
    pub cpus: usize,
    pub rustc_wrapper: Option<String>,
    pub shadowed_cargo_toml: bool,
    pub disk_free: Vec<(String, u64)>,
    pub memory_total: Option<u64>,
}

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

    let linker_chosen = env.configured_linker.is_some()
        || env
            .rustflags
            .iter()
            .any(|f| f.contains("fuse-ld") || f.starts_with("linker=") || f.contains("-Clinker"));
    if let Some(driver) = &env.runified {
        out.push(Finding::ok(format!(
            "compiling with {driver}: a parallel front end, and executables that compile before \
             their dependencies are done"
        )));
    }
    let hyprlnk_host = env.host.as_deref().is_some_and(hyprlnk::supports_target);
    if hyprlnk_host && env.rune_linker == "hyprlnk" {
        out.push(Finding::ok(
            "linking with hyprlnk, the linker built into Rune",
        ));
    } else if hyprlnk_host && !linker_chosen {
        out.push(Finding::warn(
            "the linker built into Rune is turned off",
            "On this platform hyprlnk links faster than lld, mold or the system linker, with nothing to install.",
            "rune config set build.linker hyprlnk --global",
        ));
    } else if linker_chosen {
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
    } else if has("ld.lld") {
        let host = env.host.as_deref().unwrap_or("<your target triple>");
        out.push(Finding::warn(
            "`lld` is installed but not used for linking",
            "The default GNU linker is much slower.",
            format!(
                "add to .cargo/config.toml:\n    [target.{host}]\n    rustflags = [\"-C\", \"link-arg=-fuse-ld=lld\"]"
            ),
        ));
    } else {
        out.push(Finding::note(
            "linking with the system linker",
            "Installing `mold` or `lld` would make linking noticeably faster.",
        ));
    }

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

    if let Some(total) = env.memory_total {
        let per_job = total / env.jobs.max(1) as u64;
        if per_job < (768 << 20) {
            out.push(Finding::note(
                format!("{} of memory for {} parallel jobs", human(total), env.jobs),
                "Rune starts fewer compilers when memory runs short, so this costs time rather \
                 than failing; fewer jobs (`build.jobs`) can be steadier on this machine.",
            ));
        }
    }
    for (what, free) in &env.disk_free {
        if *free < (2 << 30) {
            out.push(Finding::warn(
                format!("only {} free for {what}", human(*free)),
                "Compilers and linkers that run out of space fail with confusing errors \
                 (a linker can crash with a bus error).",
                "rune cas gc --max-size 5GB     (shrinks the store; `rune clean` removes this \
                 project's build output)",
            ));
        }
    }

    if env.shadowed_cargo_toml {
        out.push(Finding::warn(
            "both `Rune.toml` and `Cargo.toml` define this project",
            "Rune reads `Rune.toml`; plain `cargo` reads `Cargo.toml`, so the two can drift apart \
             (different dependencies or features, different builds).",
            "keep one of them as the manifest — a `Rune.toml` with only settings (no `[package]` \
             or `[workspace]`) is fine next to a `Cargo.toml`",
        ));
    }

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
            rune_linker: "hyprlnk".into(),
            runified: None,
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
            disk_free: vec![("the store".into(), 50 << 30)],
            memory_total: Some(32 << 30),
        }
    }

    #[test]
    fn the_compiler_driver_is_reported_when_it_compiles() {
        assert!(
            !diagnose(&env())
                .iter()
                .any(|f| f.title.contains("rustc-runified"))
        );
        let mut e = env();
        e.runified = Some("rustc-runified 2027.0.0".into());
        let f = diagnose(&e);
        let found = f
            .iter()
            .find(|f| f.title.contains("rustc-runified 2027.0.0"))
            .unwrap();
        assert_eq!(found.level, Level::Ok);
    }

    #[test]
    fn a_nearly_full_disk_is_a_warning_with_the_fix() {
        let mut e = env();
        e.disk_free = vec![("the store".into(), 300 << 20)];
        let f = diagnose(&e);
        let w = f
            .iter()
            .find(|f| f.title.contains("free for the store"))
            .unwrap();
        assert_eq!(w.level, Level::Warn);
        assert!(w.fix.as_deref().unwrap().contains("rune cas gc"));
    }

    #[test]
    fn little_memory_per_job_is_noted() {
        let mut e = env();
        e.memory_total = Some(4 << 30);
        e.jobs = 16;
        assert!(
            diagnose(&e)
                .iter()
                .any(|f| f.title.contains("of memory for"))
        );
        e.jobs = 4;
        assert!(
            !diagnose(&e)
                .iter()
                .any(|f| f.title.contains("of memory for"))
        );
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
        // Where hyprlnk doesn't link.
        let mut e = env();
        e.host = Some("x86_64-unknown-freebsd".into());
        e.on_path = Box::new(|t| matches!(t, "cc" | "git" | "pkg-config" | "mold"));
        let f = diagnose(&e);
        let w = f.iter().find(|f| f.level == Level::Warn).unwrap();
        assert!(w.title.contains("mold"));
        let fix = w.fix.as_deref().unwrap();
        assert!(fix.contains("[target.x86_64-unknown-freebsd]"));
        assert!(fix.contains("-fuse-ld=mold"));
        e.on_path = Box::new(|t| matches!(t, "cc" | "git" | "pkg-config" | "ld.lld"));
        assert!(warnings(&diagnose(&e))[0].contains("lld"));
    }

    #[test]
    fn hyprlnk_links_by_default_and_is_recommended_when_turned_off() {
        for host in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
        ] {
            let mut e = env();
            e.host = Some(host.into());
            e.on_path = Box::new(|t| matches!(t, "cc" | "git" | "pkg-config" | "mold"));
            let findings = diagnose(&e);
            assert!(warnings(&findings).is_empty(), "{host}");
            assert!(
                findings
                    .iter()
                    .any(|f| f.title.contains("linking with hyprlnk")),
                "{host}"
            );
            e.rune_linker = "default".into();
            let w = warnings(&diagnose(&e)).join("\n");
            assert!(w.contains("built into Rune is turned off"), "{host}: {w}");
            e.configured_linker = Some("clang".into());
            assert!(warnings(&diagnose(&e)).is_empty(), "{host}");
        }
    }

    #[test]
    fn a_configured_linker_is_left_alone() {
        let mut e = env();
        e.rune_linker = "default".into();
        e.on_path = Box::new(|t| matches!(t, "cc" | "git" | "pkg-config" | "mold"));
        e.rustflags = vec!["-Clink-arg=-fuse-ld=mold".into()];
        assert!(warnings(&diagnose(&e)).is_empty());
        e.rustflags.clear();
        e.configured_linker = Some("clang".into());
        assert!(warnings(&diagnose(&e)).is_empty());
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
