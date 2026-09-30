//! End-to-end tests: run the real `rune` binary against throwaway projects.
//!
//! Every project gets its own `RUNE_HOME`, so tests never touch the developer's
//! CAS, registry cache, or global settings, and use only path dependencies, so
//! they need no network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Project {
    dir: tempfile::TempDir,
    /// Outside the project: files in a package directory are build inputs.
    home_dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            home_dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn home(&self) -> PathBuf {
        self.home_dir.path().to_path_buf()
    }

    fn write(&self, rel: &str, content: &str) -> &Self {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
        self
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path().join(rel)).unwrap()
    }

    fn rune_in(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(args)
            .current_dir(cwd)
            .env("RUNE_HOME", self.home())
            .env("NO_COLOR", "1")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTC")
            .env_remove("RUNE_BUILD_RELEASE_REBUILD_ALL")
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .unwrap()
    }

    fn rune(&self, args: &[&str]) -> Output {
        self.rune_in(self.path(), args)
    }

    /// Runs and asserts success, returning combined stdout+stderr.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.rune(args);
        let text = text(&out);
        assert!(
            out.status.success(),
            "`rune {}` failed:\n{text}",
            args.join(" ")
        );
        text
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const LIB_MANIFEST: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

fn lib_project() -> Project {
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    p.write(
        "src/lib.rs",
        "pub fn add(a: u32, b: u32) -> u32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() { assert_eq!(super::add(1, 2), 3); }\n}\n",
    );
    p
}

#[test]
fn toml_1_1_manifests_with_multiline_inline_tables_and_trailing_commas_build() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        r#"
[package]
name = "demo"
version = "0.1.0"
edition = "2021"
authors = [
    "Someone <s@example.com>",
]

[features]
default = [
    "extra",
]
extra = []

[dependencies]
helper = {
    path = "helper",
    # comment inside a multi-line inline table
    features = [
        "x",
    ],
}
"#,
    );
    p.write("helper/Cargo.toml", "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nx = []\n");
    p.write(
        "helper/src/lib.rs",
        "pub fn h() -> u32 { if cfg!(feature = \"x\") { 7 } else { 0 } }\n",
    );
    p.write("src/lib.rs", "pub fn v() -> u32 { helper::h() }\n#[cfg(test)]\nmod t { #[test] fn feature_flows_to_the_dependency() { assert_eq!(super::v(), 7); } }\n");
    let out = p.ok(&["build"]);
    assert!(out.contains("compiled"), "{out}");
    p.ok(&["test"]);
}

#[test]
fn second_build_is_fully_cached_and_edits_recompile_only_what_changed() {
    let p = lib_project();
    assert!(p.ok(&["build"]).contains("1 compiled"));
    assert!(p.ok(&["build"]).contains("0 compiled · 1 cached"));

    p.write(
        "src/lib.rs",
        "pub fn add(a: u32, b: u32) -> u32 { a + b + 0 }\n",
    );
    assert!(p.ok(&["build"]).contains("1 compiled"));

    // Reverting restores the earlier artifact from the CAS instead of compiling.
    p.write("src/lib.rs", "pub fn add(a: u32, b: u32) -> u32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() { assert_eq!(super::add(1, 2), 3); }\n}\n");
    assert!(p.ok(&["build"]).contains("0 compiled · 1 cached"));
}

#[test]
fn binaries_compile_in_deps_and_only_their_names_appear_in_the_profile_dir() {
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    p.write("src/main.rs", "fn main() { println!(\"hi\"); }\n");
    p.ok(&["build"]);
    let debug = p.path().join(".rune/target/debug");
    let run = Command::new(debug.join("demo")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "hi\n");
    // The hash-named binary and anything the compiler leaves beside it
    // (object files under `split-debuginfo=unpacked` on macOS) stay in `deps/`.
    let files: Vec<String> = std::fs::read_dir(&debug)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files, ["demo"], "{files:?}");
    // Tools reading `--message-format json` get that name too, as from Cargo.
    let json = p.ok(&["build", "--message-format", "json"]);
    let exe = json
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| v["executable"].as_str().map(str::to_string))
        .expect("an executable in the JSON output");
    assert!(exe.ends_with(".rune/target/debug/demo"), "{exe}");
    // Restored from the store, the name comes back too.
    std::fs::remove_dir_all(p.path().join(".rune/target")).unwrap();
    assert!(p.ok(&["build"]).contains("0 compiled"));
    assert!(debug.join("demo").is_file());
}

/// A `-sys`-style crate whose build script builds a *dynamic* C library with
/// the system compiler, as `pkg-config`/`cmake`-driven crates end up doing.
#[cfg(unix)]
fn native_dylib_workspace() -> Project {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"natsys\", \"mid\", \"app\"]\nresolver = \"2\"\n",
    );
    p.write(
        "natsys/Cargo.toml",
        "[package]\nname = \"natsys\"\nversion = \"0.1.0\"\nedition = \"2021\"\nlinks = \"nat\"\n",
    );
    p.write("natsys/nat.c", "int nat_answer(void) { return 42; }\n");
    p.write(
        "natsys/build.rs",
        r#"fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let mac = std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "macos";
    let lib = format!("{out}/libnat.{}", if mac { "dylib" } else { "so" });
    let mut cc = std::process::Command::new(std::env::var("CC").unwrap_or("cc".into()));
    cc.args(["-shared", "-fPIC", "nat.c", "-o", &lib]);
    if mac {
        // Found through the loader's search path at run time, not by path.
        cc.args(["-install_name", "libnat.dylib"]);
    }
    assert!(cc.status().unwrap().success());
    println!("cargo::rerun-if-changed=nat.c");
    println!("cargo::rustc-link-search=native={out}");
    println!("cargo::rustc-link-lib=dylib=nat");
}
"#,
    );
    p.write(
        "natsys/src/lib.rs",
        "unsafe extern \"C\" { fn nat_answer() -> i32; }\npub fn answer() -> i32 { unsafe { nat_answer() } }\n",
    );
    p.write(
        "mid/Cargo.toml",
        "[package]\nname = \"mid\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nnatsys = { path = \"../natsys\" }\n",
    );
    p.write(
        "mid/src/lib.rs",
        "pub fn doubled() -> i32 { natsys::answer() * 2 }\n",
    );
    p.write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nmid = { path = \"../mid\" }\n",
    );
    p.write(
        "app/src/main.rs",
        "fn main() { println!(\"native says {}\", mid::doubled()); }\n#[test]\nfn links() { assert_eq!(mid::doubled(), 84); }\n",
    );
    p
}

#[cfg(unix)]
#[test]
fn native_libraries_of_dependencies_link_and_load_two_levels_up() {
    // The library directory is known only to `natsys`'s build script; the
    // link that needs it happens in `app`, and so does loading it.
    let p = native_dylib_workspace();
    let out = p.ok(&["run", "-p", "app"]);
    assert!(out.contains("native says 84"), "{out}");
    let out = p.ok(&["test", "-p", "app"]);
    assert!(out.contains("1 passed"), "{out}");
    // Restored from the store, the directories are known again.
    std::fs::remove_dir_all(p.path().join(".rune/target")).unwrap();
    let out = p.ok(&["run", "-p", "app"]);
    assert!(
        out.contains("0 compiled") && out.contains("native says 84"),
        "{out}"
    );
}

#[test]
fn library_outputs_get_cargos_names_in_the_profile_directory() {
    let p = lib_project();
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}[lib]\ncrate-type = [\"cdylib\", \"staticlib\", \"rlib\"]\n"),
    );
    p.ok(&["build"]);
    let debug = p.path().join(".rune/target/debug");
    let dylib = if cfg!(target_os = "macos") {
        "libdemo.dylib"
    } else if cfg!(windows) {
        "demo.dll"
    } else {
        "libdemo.so"
    };
    let staticlib = if cfg!(windows) {
        "demo.lib"
    } else {
        "libdemo.a"
    };
    for name in [dylib, staticlib, "libdemo.rlib"] {
        assert!(debug.join(name).is_file(), "missing {name}");
    }
}

#[test]
fn doctests_see_the_build_scripts_out_dir_and_cfgs() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"gen\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "build.rs",
        r#"fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/gen.rs"), "pub const GEN: u32 = 5;").unwrap();
    println!("cargo::rustc-check-cfg=cfg(gen_ok)");
    println!("cargo::rustc-cfg=gen_ok");
}
"#,
    );
    p.write(
        "src/lib.rs",
        r#"include!(concat!(env!("OUT_DIR"), "/gen.rs"));

/// ```
/// assert_eq!(gen::GEN, 5);
/// ```
pub fn uses_generated() {}

/// Only documented, and tested, when the build script's cfg is on.
/// ```
/// assert!(gen::only_with_cfg());
/// ```
#[cfg(gen_ok)]
pub fn only_with_cfg() -> bool {
    true
}
"#,
    );
    let out = p.ok(&["test", "--doc"]);
    assert!(out.contains("2 passed"), "{out}");
}

#[cfg(unix)]
#[test]
fn config_aliases_and_external_subcommands_work_like_cargos() {
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    p.write(
        "src/main.rs",
        "fn main() { println!(\"args: {:?}\", std::env::args().skip(1).collect::<Vec<_>>()); }\n",
    );
    p.write(
        ".cargo/config.toml",
        "[alias]\ngreet = \"run -q --\"\nloud = [\"greet\", \"LOUD\"]\n",
    );
    // An alias, and an alias of an alias, with arguments passed through.
    assert!(p.ok(&["greet", "hi"]).contains("args: [\"hi\"]"));
    assert!(p.ok(&["loud", "x"]).contains("args: [\"LOUD\", \"x\"]"));

    // A `cargo-<name>` program on PATH is a subcommand, told where cargo is.
    let bin = tempfile::tempdir().unwrap();
    let plugin = bin.path().join("cargo-hello");
    std::fs::write(
        &plugin,
        "#!/bin/sh\necho \"plugin got: $*\"\n[ -n \"$CARGO\" ] && echo \"CARGO is set\"\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["hello", "--flag", "x"])
        .current_dir(p.path())
        .env("RUNE_HOME", p.home())
        .env("PATH", path)
        .output()
        .unwrap();
    let out = text(&out);
    assert!(out.contains("plugin got: hello --flag x"), "{out}");
    assert!(out.contains("CARGO is set"), "{out}");

    // Anything else is still an error.
    let out = p.rune(&["definitely-not-a-command"]);
    assert!(!out.status.success());
}

#[test]
fn cleaning_the_target_dir_restores_from_the_cas() {
    let p = lib_project();
    p.ok(&["build"]);
    p.ok(&["clean"]);
    assert!(!p.path().join(".rune/target").exists());
    assert!(p.ok(&["build"]).contains("0 compiled · 1 cached"));
}

#[test]
fn release_rebuild_all_is_off_by_default_and_controlled_by_config() {
    let p = lib_project();
    p.ok(&["build", "--release"]);

    // Default: a release build restores from the CAS like any other.
    p.ok(&["clean"]);
    let out = p.ok(&["build", "--release"]);
    assert!(out.contains("0 compiled · 1 cached"), "{out}");
    assert!(!out.contains("rebuilt all"), "{out}");

    // Enable for this project: everything recompiles, even though it is cached.
    p.ok(&["config", "set", "build.release-rebuild-all", "true"]);
    assert!(p.read("Rune.toml").contains("release-rebuild-all = true"));
    p.ok(&["clean"]);
    let out = p.ok(&["build", "--release"]);
    assert!(out.contains("1 compiled · 0 cached"), "{out}");
    assert!(out.contains("rebuilt all"), "{out}");

    // Dev builds are unaffected by the setting.
    p.ok(&["build"]);
    p.ok(&["clean"]);
    assert!(p.ok(&["build"]).contains("0 compiled · 1 cached"));

    // A flag overrides the setting for one run, both ways.
    p.ok(&["clean"]);
    assert!(
        p.ok(&["build", "--release", "--rebuild-all=false"])
            .contains("0 compiled · 1 cached")
    );
    p.ok(&["config", "unset", "build.release-rebuild-all"]);
    p.ok(&["clean"]);
    assert!(
        p.ok(&["build", "--release", "--rebuild-all"])
            .contains("1 compiled")
    );
}

#[test]
fn global_config_applies_to_every_project_and_project_overrides_it() {
    let p = lib_project();
    p.ok(&[
        "config",
        "set",
        "build.release-rebuild-all",
        "true",
        "--global",
    ]);
    assert!(
        p.ok(&["config", "get", "build.release-rebuild-all"])
            .trim()
            .ends_with("true")
    );
    p.ok(&["build", "--release"]);
    p.ok(&["clean"]);
    assert!(p.ok(&["build", "--release"]).contains("rebuilt all"));

    // The project file wins over the global one.
    p.ok(&[
        "config",
        "set",
        "build.release-rebuild-all",
        "false",
        "--project",
    ]);
    p.ok(&["clean"]);
    assert!(!p.ok(&["build", "--release"]).contains("rebuilt all"));

    let listing = p.ok(&["config", "list", "--show-origin"]);
    assert!(
        listing.contains("build.release-rebuild-all") && listing.contains("project"),
        "{listing}"
    );
}

#[test]
fn config_rejects_unknown_keys_and_bad_values() {
    let p = lib_project();
    let out = p.rune(&["config", "set", "build.nope", "1"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("unknown setting"), "{}", text(&out));

    let out = p.rune(&["config", "set", "build.release-rebuild-all", "maybe"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("boolean"), "{}", text(&out));
    assert!(
        !p.path().join("Rune.toml").exists(),
        "a rejected value must not create the file"
    );
}

#[test]
fn rune_toml_never_invalidates_the_build_cache() {
    let p = lib_project();
    p.ok(&["build"]);
    p.ok(&["config", "set", "build.jobs", "2"]);
    assert!(
        p.ok(&["build"]).contains("0 compiled"),
        "settings edits must not rebuild"
    );
}

#[test]
fn default_features_apply_to_local_crates_and_flags_change_them() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\ndefault = [\"on\"]\non = []\nopt = []\n",
    );
    p.write(
        "src/lib.rs",
        "pub fn f() -> (bool, bool) { (cfg!(feature = \"on\"), cfg!(feature = \"opt\")) }\n#[cfg(test)]\nmod t {\n#[test] fn flags() { let (on, opt) = super::f(); assert!(on); assert!(!opt); }\n}\n",
    );
    p.ok(&["test"]);

    // `--no-default-features` turns `on` off, so the same test must now fail.
    let out = p.rune(&["test", "--no-default-features"]);
    assert!(!out.status.success(), "{}", text(&out));

    p.write(
        "src/lib.rs",
        "pub fn f() -> (bool, bool) { (cfg!(feature = \"on\"), cfg!(feature = \"opt\")) }\n#[cfg(test)]\nmod t {\n#[test] fn flags() { let (on, opt) = super::f(); assert!(on && opt); }\n}\n",
    );
    p.ok(&["test", "--features", "opt"]);
    let bad = p.rune(&["build", "--features", "nonexistent"]);
    assert!(!bad.status.success());
    assert!(
        text(&bad).contains("does not contain the feature"),
        "{}",
        text(&bad)
    );
}

fn workspace() -> Project {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
    );
    p.write(
        "crates/good/Cargo.toml",
        "[package]\nname = \"good\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("crates/good/src/lib.rs", "pub fn g() -> u8 { 1 }\n");
    p.write(
        "crates/bad/Cargo.toml",
        "[package]\nname = \"bad\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("crates/bad/src/lib.rs", "this does not compile\n");
    p
}

#[test]
fn check_after_build_reuses_the_build_and_still_checks_edits() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"base\", \"mid\", \"app\"]\nresolver = \"2\"\n",
    );
    let pkg = |name: &str, deps: &str| {
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\n{deps}"
        )
    };
    p.write("base/Cargo.toml", &pkg("base", ""));
    p.write(
        "base/build.rs",
        "fn main() { let out = std::env::var(\"OUT_DIR\").unwrap(); std::fs::write(format!(\"{out}/n.rs\"), \"pub const N: u32 = 7;\").unwrap(); }\n",
    );
    p.write(
        "base/src/lib.rs",
        "include!(concat!(env!(\"OUT_DIR\"), \"/n.rs\"));\n",
    );
    p.write(
        "mid/Cargo.toml",
        &pkg("mid", "base = { path = \"../base\" }\n"),
    );
    p.write("mid/src/lib.rs", "pub fn m() -> u32 { base::N * 2 }\n");
    p.write(
        "app/Cargo.toml",
        &pkg(
            "app",
            "base = { path = \"../base\" }\nmid = { path = \"../mid\" }\n",
        ),
    );
    let main = "fn main() { let unused = 1; println!(\"{}\", mid::m() + base::N); }\n";
    p.write("app/src/main.rs", main);

    p.ok(&["build"]);
    // Nothing to do: every unit's fully built result stands in for its check,
    // and the build's warnings are replayed.
    let out = p.ok(&["check"]);
    assert!(out.contains("0 compiled"), "{out}");
    assert!(out.contains("unused"), "{out}");

    // An edit is checked for real, against the reused `.rmeta`s below it.
    p.write("mid/src/lib.rs", "pub fn m() -> u32 { base::N * 3 }\n");
    let out = p.ok(&["check"]);
    assert!(out.contains("2 compiled"), "{out}");
    p.write("app/src/main.rs", "fn main() { let x: u32 = \"no\"; }\n");
    let out = p.rune(&["check"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("mismatched types"), "{}", text(&out));

    // And back: build, then check once more.
    p.write("app/src/main.rs", main);
    p.ok(&["build"]);
    let out = p.ok(&["check"]);
    assert!(out.contains("0 compiled"), "{out}");
}

#[test]
fn package_selection_builds_only_what_was_asked_for() {
    let p = workspace();
    p.ok(&["build", "-p", "good"]);

    // The whole workspace includes the broken member.
    assert!(!p.rune(&["build"]).status.success());
    assert!(!p.rune(&["build", "--workspace"]).status.success());
    p.ok(&["build", "--workspace", "--exclude", "bad"]);

    // Running from inside a member selects just that member, like Cargo.
    let out = p.rune_in(&p.path().join("crates/good"), &["build"]);
    assert!(out.status.success(), "{}", text(&out));

    let unknown = p.rune(&["build", "-p", "nope"]);
    assert!(!unknown.status.success());
    assert!(
        text(&unknown).contains("did not match any packages"),
        "{}",
        text(&unknown)
    );
}

#[test]
fn failing_tests_exit_101_and_show_the_failure() {
    let p = lib_project();
    p.write(
        "src/lib.rs",
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn passes() {}\n    #[test]\n    fn breaks() { assert_eq!(1, 2, \"custom-message-xyz\"); }\n}\n",
    );
    let out = p.rune(&["test"]);
    assert_eq!(out.status.code(), Some(101), "{}", text(&out));
    let t = text(&out);
    assert!(
        t.contains("breaks") && t.contains("custom-message-xyz"),
        "{t}"
    );
    assert!(t.contains("FAILED"), "{t}");

    // A filter selects tests by name and can select none of the failures.
    let out = p.rune(&["test", "passes"]);
    assert!(out.status.success(), "{}", text(&out));

    // Arguments after `--` reach the test binary.
    let out = p.rune(&["test", "--", "--list"]);
    assert!(text(&out).contains("passes: test"), "{}", text(&out));
}

#[test]
fn compile_errors_in_tests_are_reported_with_a_nonzero_exit() {
    let p = lib_project();
    p.write(
        "src/lib.rs",
        "#[cfg(test)]\nmod tests { #[test] fn t() { let x: u32 = \"no\"; } }\n",
    );
    let out = p.rune(&["test"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("mismatched types"), "{}", text(&out));
}

#[test]
fn integration_tests_and_binaries_are_built_and_run() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    p.write(
        "src/main.rs",
        "fn main() { println!(\"answer={}\", demo::answer()); }\n",
    );
    p.write(
        "tests/it.rs",
        "#[test]\nfn lib_is_linked() { assert_eq!(demo::answer(), 42); }\n\n#[test]\nfn binary_is_built_first() {\n    let exe = env!(\"CARGO_BIN_EXE_demo\");\n    let out = std::process::Command::new(exe).output().unwrap();\n    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), \"answer=42\");\n}\n",
    );
    let out = p.ok(&["test"]);
    assert!(
        out.contains("test it"),
        "the integration test should be listed: {out}"
    );

    let run = p.ok(&["run"]);
    assert!(run.contains("answer=42"), "{run}");
    let with_args = {
        p.write("src/main.rs", "fn main() { println!(\"args={:?}\", std::env::args().skip(1).collect::<Vec<_>>()); }\n");
        p.ok(&["run", "--", "a", "--flag"])
    };
    assert!(with_args.contains("[\"a\", \"--flag\"]"), "{with_args}");
}

#[test]
fn doctests_run_and_failures_are_reported() {
    let p = lib_project();
    p.write("src/lib.rs", "/// ```\n/// assert_eq!(demo::add(1, 1), 2);\n/// ```\npub fn add(a: u32, b: u32) -> u32 { a + b }\n");
    let out = p.ok(&["test"]);
    assert!(
        out.contains("doc-tests") && out.contains("1 passed"),
        "{out}"
    );

    p.write("src/lib.rs", "/// ```\n/// assert_eq!(demo::add(1, 1), 3);\n/// ```\npub fn add(a: u32, b: u32) -> u32 { a + b }\n");
    assert_eq!(p.rune(&["test"]).status.code(), Some(101));
    // `--no-doc` skips them, and the setting can turn them off for good.
    p.ok(&["test", "--no-doc"]);
    p.ok(&["config", "set", "test.doctests", "false"]);
    p.ok(&["test"]);
}

#[test]
fn dev_dependencies_are_used_by_tests_but_not_built_by_build() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dev-dependencies]\nhelper = { path = \"helper\" }\n",
    );
    p.write(
        "helper/Cargo.toml",
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("helper/src/lib.rs", "pub fn h() -> u32 { 5 }\n");
    p.write("src/lib.rs", "pub fn f() -> u32 { 1 }\n#[cfg(test)]\nmod t { #[test] fn uses_dev_dep() { assert_eq!(helper::h(), 5); } }\n");
    assert!(
        p.ok(&["build"]).contains("1 compiled"),
        "dev-dependencies stay out of `build`"
    );
    p.ok(&["test"]);
}

#[test]
fn lints_tables_are_enforced() {
    let p = lib_project();
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}\n[lints.rust]\nunsafe_code = \"forbid\"\n"),
    );
    p.write(
        "src/lib.rs",
        "pub fn f() -> u8 { unsafe { std::mem::zeroed() } }\n",
    );
    let out = p.rune(&["build"]);
    assert!(
        !out.status.success(),
        "unsafe code must be rejected: {}",
        text(&out)
    );
    assert!(text(&out).contains("unsafe"), "{}", text(&out));
}

#[test]
fn compiler_warnings_are_shown_and_replayed_from_the_cache() {
    let p = lib_project();
    p.write("src/lib.rs", "pub fn f() { let unused = 1; }\n");
    assert!(p.ok(&["build"]).contains("unused variable"));
    let again = p.ok(&["build"]);
    assert!(
        again.contains("unused variable") && again.contains("0 compiled"),
        "{again}"
    );
    assert!(!p.ok(&["build", "--quiet"]).contains("compiled"));
}

#[test]
fn renamed_and_custom_named_libraries_link() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nfoo-pkg = { path = \"foo\" }\nrenamed = { path = \"bar\", package = \"bar-pkg\" }\n",
    );
    p.write("foo/Cargo.toml", "[package]\nname = \"foo-pkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\nname = \"custom_foo\"\n");
    p.write("foo/src/lib.rs", "pub fn f() -> u8 { 1 }\n");
    p.write(
        "bar/Cargo.toml",
        "[package]\nname = \"bar-pkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("bar/src/lib.rs", "pub fn b() -> u8 { 2 }\n");
    p.write(
        "src/main.rs",
        "fn main() { println!(\"{}\", custom_foo::f() + renamed::b()); }\n",
    );
    assert!(p.ok(&["run"]).contains('3'));
}

#[test]
fn custom_profiles_get_their_own_output_directory() {
    let p = lib_project();
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}\n[profile.release]\nlto = true\n[profile.fast]\ninherits = \"release\"\nopt-level = 1\n"),
    );
    p.ok(&["build", "--profile", "fast"]);
    assert!(p.path().join(".rune/target/fast/deps").is_dir());
    assert!(!p.path().join(".rune/target/release").exists());
    p.ok(&["build", "--release"]);
    assert!(p.path().join(".rune/target/release/deps").is_dir());

    let bad = p.rune(&["build", "--profile", "nonexistent"]);
    assert!(!bad.status.success());
    assert!(text(&bad).contains("inherits"), "{}", text(&bad));
}

#[test]
fn new_and_init_scaffold_projects_that_build_and_test() {
    let p = Project::new();
    p.ok(&["new", "mylib", "--lib"]);
    let lib = p.path().join("mylib");
    assert!(lib.join("Cargo.toml").is_file() && lib.join("src/lib.rs").is_file());
    let out = p.rune_in(&lib, &["test"]);
    assert!(out.status.success(), "{}", text(&out));

    p.ok(&["new", "myapp"]);
    let run = p.rune_in(&p.path().join("myapp"), &["run"]);
    assert!(text(&run).contains("Hello, world!"), "{}", text(&run));

    // Refuses to clobber, and validates names.
    assert!(!p.rune(&["new", "myapp"]).status.success());
    assert!(!p.rune(&["new", "fn"]).status.success());
    std::fs::create_dir_all(p.path().join("existing")).unwrap();
    p.ok(&["init", "existing", "--lib"]);
    assert!(p.path().join("existing/Cargo.toml").is_file());
    assert!(!p.rune(&["init", "existing"]).status.success());
}

#[test]
fn examples_can_be_built_and_run() {
    let p = lib_project();
    p.write(
        "examples/hello.rs",
        "fn main() { println!(\"from example: {}\", demo::add(2, 2)); }\n",
    );
    let out = p.ok(&["run", "--example", "hello"]);
    assert!(out.contains("from example: 4"), "{out}");
    p.ok(&["build", "--examples"]);
    assert!(!p.rune(&["run", "--example", "missing"]).status.success());
}

#[test]
fn locate_project_and_metadata_describe_the_workspace() {
    let p = workspace();
    let loc = p.ok(&["locate-project"]);
    assert!(loc.contains("Cargo.toml"), "{loc}");
    let meta = p.ok(&["metadata", "--no-deps", "--offline"]);
    let v: serde_json::Value = serde_json::from_str(meta.trim()).unwrap();
    let names: Vec<_> = v["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["bad", "good"]);
    assert_eq!(v["version"], 1);
}

#[test]
fn locked_fails_when_the_lockfile_would_change_and_offline_needs_a_cache() {
    let p = lib_project();
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}\n[dependencies]\nserde = \"1\"\n"),
    );
    let out = p.rune(&["build", "--offline"]);
    assert!(!out.status.success());
    assert!(
        text(&out).to_lowercase().contains("offline"),
        "{}",
        text(&out)
    );
}

#[test]
fn an_unreachable_git_dependency_fails_loudly_instead_of_resolving_to_the_wrong_crate() {
    let p = lib_project();
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}\n[dependencies]\nserde = {{ git = \"https://example.invalid/serde\" }}\n"),
    );
    let out = p.rune(&["build"]);
    assert!(!out.status.success());
    // The error names the dependency and its URL; it never quietly falls
    // back to whatever crates.io has under the same name.
    let t = text(&out);
    assert!(
        t.contains("git dependency `serde`") && t.contains("example.invalid"),
        "{t}"
    );
}

#[test]
fn editing_manifests_preserves_multiline_tables_and_comments() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        r#"# top comment
[package]
name = "demo"
version = "0.1.0"
edition = "2021"

[dependencies]
keep = {
    path = "keep",
    features = [
        "x",
    ],   # trailing comma and comment
}
drop-me = { path = "drop" }
"#,
    );
    for name in ["keep", "drop"] {
        p.write(
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nx = []\n", if name == "drop" { "drop-me" } else { name }),
        );
        p.write(&format!("{name}/src/lib.rs"), "");
    }
    p.write("src/lib.rs", "");

    p.ok(&["remove", "drop-me", "--offline"]);
    let manifest = p.read("Cargo.toml");
    assert!(!manifest.contains("drop-me"), "{manifest}");
    assert!(manifest.contains("# top comment"), "{manifest}");
    assert!(
        manifest.contains("features = [\n        \"x\",\n    ],   # trailing comma and comment"),
        "untouched entries must keep their exact formatting:\n{manifest}"
    );
    p.ok(&["build", "--offline"]);

    // Removing something that isn't there is a warning, not a failure.
    let out = p.ok(&["remove", "nope", "--offline"]);
    assert!(out.contains("not found"), "{out}");
}

// ---------------------------------------------------------------------------
// Build scripts, host/target roles, features per selection, configuration
// ---------------------------------------------------------------------------

fn host_triple() -> String {
    let out = Command::new("rustc").arg("-vV").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .unwrap()
        .to_string()
}

/// A package with a build script, a library and a binary that all use the
/// script's `OUT_DIR`.
fn build_script_project() -> Project {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        r#"[package]
name = "demo"
version = "1.2.3-beta.1"
edition = "2021"
authors = ["Ada <ada@example.com>", "Bob"]
description = "a demo"
build = "build.rs"
"#,
    );
    p.write(
        "build.rs",
        r#"use std::io::Write;
fn main() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut f = std::fs::File::create(out.join("generated.rs")).unwrap();
    writeln!(
        f,
        "pub const FACTS: &str = {:?};",
        format!(
            "os={} authors={} desc={} pre={} debug={} profile={} target={}",
            std::env::var("CARGO_CFG_TARGET_OS").unwrap(),
            std::env::var("CARGO_PKG_AUTHORS").unwrap(),
            std::env::var("CARGO_PKG_DESCRIPTION").unwrap(),
            std::env::var("CARGO_PKG_VERSION_PRE").unwrap(),
            std::env::var("DEBUG").unwrap(),
            std::env::var("PROFILE").unwrap(),
            std::env::var("TARGET").unwrap(),
        )
    )
    .unwrap();
    println!("cargo::rustc-cfg=from_script");
    println!("cargo::rustc-check-cfg=cfg(from_script)");
    println!("cargo::rustc-env=SCRIPT_VAR=ok");
    println!("cargo::rerun-if-changed=build.rs");
}
"#,
    );
    p.write(
        "src/lib.rs",
        r#"include!(concat!(env!("OUT_DIR"), "/generated.rs"));
pub fn script_cfg() -> bool { cfg!(from_script) }
"#,
    );
    p.write(
        "src/main.rs",
        r#"// The binary shares the library's build script run: same OUT_DIR, same cfgs.
mod generated { include!(concat!(env!("OUT_DIR"), "/generated.rs")); }
fn main() {
    println!("lib: {}", demo::FACTS);
    println!("bin: {}", generated::FACTS);
    println!("cfg={} env={}", cfg!(from_script), env!("SCRIPT_VAR"));
}
"#,
    );
    p
}

#[test]
fn build_scripts_see_the_target_and_package_metadata_and_share_out_dir() {
    let p = build_script_project();
    let out = p.ok(&["run"]);
    let host = host_triple();
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else {
        std::env::consts::OS
    };
    let expected = format!(
        "os={os} authors=Ada <ada@example.com>:Bob desc=a demo pre=beta.1 debug=true profile=debug target={host}"
    );
    assert!(out.contains(&format!("lib: {expected}")), "{out}");
    // The binary compiled against the *same* generated file and cfgs.
    assert!(out.contains(&format!("bin: {expected}")), "{out}");
    assert!(out.contains("cfg=true env=ok"), "{out}");
}

#[test]
fn projects_with_build_scripts_are_fully_cached_after_the_first_build() {
    let p = build_script_project();
    p.ok(&["build"]);
    let again = p.ok(&["build"]);
    assert!(again.contains("0 compiled"), "{again}");
    // Even with the local build directory gone, everything — build script
    // output included — comes back from the store.
    std::fs::remove_dir_all(p.path().join(".rune/target")).unwrap();
    let restored = p.ok(&["run"]);
    assert!(restored.contains("0 compiled"), "{restored}");
    assert!(restored.contains("cfg=true env=ok"), "{restored}");
}

#[test]
fn cross_builds_use_a_target_directory_and_host_scripts() {
    let p = build_script_project();
    let host = host_triple();
    let out = p.ok(&["build", "--target", &host]);
    assert!(out.contains("compiled"), "{out}");
    let bin = p.path().join(".rune/target/debug").join(&host).join("demo");
    assert!(bin.is_file(), "expected {}", bin.display());
    let run = Command::new(&bin).output().unwrap();
    let stdout = String::from_utf8_lossy(&run.stdout);
    // `TARGET` is the requested target even though the script ran on the host.
    assert!(stdout.contains(&format!("target={host}")), "{stdout}");
}

/// `shared` is a normal dependency (no features) *and* a build-dependency
/// (with `extra`): resolver 2 keeps the two feature sets apart.
fn split_feature_project() -> Project {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
shared = { path = "shared" }

[build-dependencies]
shared = { path = "shared", features = ["extra"] }
"#,
    );
    p.write(
        "shared/Cargo.toml",
        "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nextra = []\n",
    );
    p.write(
        "shared/src/lib.rs",
        "pub fn has_extra() -> bool { cfg!(feature = \"extra\") }\n",
    );
    p.write(
        "build.rs",
        "fn main() { println!(\"cargo::rustc-env=BUILD_SAW_EXTRA={}\", shared::has_extra()); }\n",
    );
    p.write(
        "src/main.rs",
        "fn main() { println!(\"build={} runtime={}\", env!(\"BUILD_SAW_EXTRA\"), shared::has_extra()); }\n",
    );
    p
}

#[test]
fn build_dependencies_get_their_own_features_and_are_not_linked_into_the_crate() {
    let p = split_feature_project();
    let out = p.ok(&["run"]);
    assert!(out.contains("build=true runtime=false"), "{out}");
    // Also as a cross build: the build script's `shared` is a host unit.
    let host = host_triple();
    let out = p.ok(&["build", "--target", &host]);
    assert!(out.contains("compiled"), "{out}");
    let run = Command::new(p.path().join(".rune/target/debug").join(&host).join("app"))
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("build=true runtime=false"),
        "{}",
        String::from_utf8_lossy(&run.stdout)
    );

    // `shared` is not `--extern`ed into the build script's dependents beyond
    // what was declared: a crate that only build-depends on something cannot
    // use it at runtime.
    let q = Project::new();
    q.write(
        "Cargo.toml",
        "[package]\nname = \"q\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n[build-dependencies]\nshared = { path = \"shared\" }\n",
    );
    q.write(
        "shared/Cargo.toml",
        "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    q.write("shared/src/lib.rs", "pub fn f() {}\n");
    q.write("build.rs", "fn main() { shared::f(); }\n");
    q.write("src/main.rs", "fn main() { shared::f(); }\n");
    let out = q.rune(&["build"]);
    assert!(
        !out.status.success(),
        "build-dependencies leaked into the crate"
    );
}

#[test]
fn proc_macros_build_for_the_host_in_native_and_cross_builds() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nanswer = { path = \"answer\" }\n",
    );
    p.write(
        "answer/Cargo.toml",
        "[package]\nname = \"answer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\nproc-macro = true\n",
    );
    p.write(
        "answer/src/lib.rs",
        "extern crate proc_macro;\n#[proc_macro]\npub fn answer(_: proc_macro::TokenStream) -> proc_macro::TokenStream { \"42u32\".parse().unwrap() }\n",
    );
    p.write(
        "src/main.rs",
        "fn main() { println!(\"answer={}\", answer::answer!()); }\n",
    );
    assert!(p.ok(&["run"]).contains("answer=42"));
    let host = host_triple();
    p.ok(&["build", "--target", &host]);
    let run = Command::new(p.path().join(".rune/target/debug").join(&host).join("app"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&run.stdout).contains("answer=42"));
}

#[test]
fn features_only_count_for_the_selected_packages() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\", \"shared\"]\nresolver = \"2\"\n",
    );
    p.write(
        "shared/Cargo.toml",
        "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nx = []\n",
    );
    p.write(
        "shared/src/lib.rs",
        "pub fn has_x() -> bool { cfg!(feature = \"x\") }\n",
    );
    // `a` turns `x` on; `b` does not.
    p.write(
        "a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nshared = { path = \"../shared\", features = [\"x\"] }\n",
    );
    p.write(
        "a/src/main.rs",
        "fn main() { println!(\"a sees x={}\", shared::has_x()); }\n",
    );
    p.write(
        "b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nshared = { path = \"../shared\" }\n",
    );
    p.write(
        "b/src/main.rs",
        "fn main() { println!(\"b sees x={}\", shared::has_x()); }\n",
    );

    // Selecting only `b` must not inherit `a`'s request for `x`…
    assert!(p.ok(&["run", "-p", "b"]).contains("b sees x=false"));
    // …while building both together unifies it, as Cargo does.
    p.ok(&["build", "-p", "a", "-p", "b"]);
    let b = Command::new(p.path().join(".rune/target/debug/b"))
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&b.stdout).contains("b sees x=true"),
        "{}",
        String::from_utf8_lossy(&b.stdout)
    );
}

#[test]
fn library_crate_types_produce_every_requested_artifact() {
    let p = lib_project();
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}[lib]\ncrate-type = [\"cdylib\", \"staticlib\", \"rlib\"]\n"),
    );
    p.ok(&["build"]);
    let deps = p.path().join(".rune/target/debug/deps");
    let names: Vec<String> = std::fs::read_dir(&deps)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let has = |suffix: &str| {
        names
            .iter()
            .any(|n| n.starts_with("libdemo") && n.ends_with(suffix))
    };
    assert!(has(".rlib"), "{names:?}");
    assert!(has(".a"), "{names:?}");
    assert!(
        has(if cfg!(target_os = "macos") {
            ".dylib"
        } else {
            ".so"
        }),
        "{names:?}"
    );
}

#[test]
fn package_profile_overrides_change_how_a_dependency_is_compiled() {
    let make = |override_table: &str| {
        let p = Project::new();
        p.write(
            "Cargo.toml",
            &format!(
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nhelper = {{ path = \"helper\" }}\n{override_table}"
            ),
        );
        p.write(
            "helper/Cargo.toml",
            "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        p.write(
            "helper/src/lib.rs",
            "pub fn add(a: u8, b: u8) -> u8 { a + b }\n",
        );
        p.write(
            "src/main.rs",
            "fn main() { let n = std::env::args().count() as u8; println!(\"sum={}\", helper::add(254 + n, 1)); }\n",
        );
        p
    };
    // Debug builds check overflow: 255 + 1 panics…
    let strict = make("");
    let out = strict.rune(&["run"]);
    assert!(!out.status.success(), "{}", text(&out));
    // …unless the override switches the check off for that package only.
    let loose = make("[profile.dev.package.helper]\noverflow-checks = false\n");
    assert!(loose.ok(&["run"]).contains("sum=0"));
}

#[cfg(unix)]
#[test]
fn rustc_wrappers_are_used_and_never_share_artifacts_with_unwrapped_builds() {
    use std::os::unix::fs::PermissionsExt;
    let p = lib_project();
    // Outside the project: files in a package directory are build inputs, so
    // a log growing inside it would (correctly) invalidate every build.
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("wrapper.log");
    let script = scratch.path().join("wrap.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho called >> {}\nexec \"$@\"\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let plain = p.ok(&["build"]);
    assert!(plain.contains("1 compiled"), "{plain}");
    assert!(!log.exists());

    let run = |p: &Project| {
        let out = Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(["build"])
            .current_dir(p.path())
            .env("RUNE_HOME", p.home())
            .env("NO_COLOR", "1")
            .env("RUSTC_WRAPPER", &script)
            .env_remove("RUSTFLAGS")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        text(&out)
    };
    // The wrapper changes the compilation, so the plain build's artifact is
    // not reused: it compiles again, through the wrapper.
    let wrapped = run(&p);
    assert!(
        wrapped.contains("1 compiled"),
        "wrapper must not reuse the plain build: {wrapped}"
    );
    assert!(log.exists(), "the wrapper was never invoked");
    let calls = std::fs::read_to_string(&log).unwrap().lines().count();
    // Same wrapper again: now it *is* cached, and the wrapper is left alone.
    let again = run(&p);
    assert!(
        again.contains("0 compiled") && again.contains("1 cached"),
        "{again}"
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap().lines().count(),
        calls
    );
}

#[test]
fn cargo_config_env_rustflags_and_target_tables_apply() {
    let p = Project::new();
    let host = host_triple();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        ".cargo/config.toml",
        &format!(
            "[env]\nGREETING = \"hello from config\"\n\n[build]\nrustflags = [\"--cfg\", \"from_build_flags\"]\n\n[target.{host}]\nrustflags = [\"--cfg\", \"from_target_flags\"]\n"
        ),
    );
    p.write(
        "src/main.rs",
        "fn main() {\n    println!(\"{} build={} target={}\", env!(\"GREETING\"), cfg!(from_build_flags), cfg!(from_target_flags));\n}\n",
    );
    let out = p.ok(&["run"]);
    // `[target.<triple>].rustflags` wins over `[build].rustflags`, as in Cargo.
    assert!(
        out.contains("hello from config build=false target=true"),
        "{out}"
    );
}

#[test]
fn rust_version_is_enforced_unless_ignored() {
    let p = lib_project();
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}rust-version = \"1.999\"\n"),
    );
    let out = p.rune(&["build"]);
    assert!(!out.status.success());
    let t = text(&out);
    assert!(t.contains("requires rustc 1.999"), "{t}");
    assert!(
        p.ok(&["build", "--ignore-rust-version"])
            .contains("compiled")
    );
}

#[test]
fn packages_linking_the_same_native_library_conflict() {
    let p = Project::new();
    let member = |name: &str| {
        (
            format!("{name}/Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\nlinks = \"samelib\"\nbuild = \"build.rs\"\n"
            ),
        )
    };
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\none = { path = \"one\" }\ntwo = { path = \"two\" }\n",
    );
    for name in ["one", "two"] {
        let (path, manifest) = member(name);
        p.write(&path, &manifest);
        p.write(&format!("{name}/build.rs"), "fn main() {}\n");
        p.write(&format!("{name}/src/lib.rs"), "");
    }
    p.write("src/main.rs", "fn main() {}\n");
    let out = p.rune(&["build"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("native library `samelib`"),
        "{}",
        text(&out)
    );
}

#[test]
fn a_failing_build_script_shows_its_stderr_and_error_directive() {
    let p = lib_project();
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}build = \"build.rs\"\n"),
    );
    p.write(
        "build.rs",
        "fn main() { eprintln!(\"custom diagnostic text\"); println!(\"cargo::error=configuration is wrong\"); }\n",
    );
    let out = p.rune(&["build"]);
    assert!(!out.status.success());
    let t = text(&out);
    assert!(t.contains("configuration is wrong"), "{t}");
}

// ---------------------------------------------------------------------------
// Git dependencies (against local repositories: no network)
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "protocol.file.allow")
        .env("GIT_CONFIG_VALUE_0", "always")
        .output()
        .expect("git is required for these tests");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository whose one package prints its version constant.
fn upstream(version_text: &str) -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let d = repo.path();
    git(d, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(
        d.join("Cargo.toml"),
        "[package]\nname = \"gitdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        d.join("src/lib.rs"),
        format!("pub fn version() -> &'static str {{ {version_text:?} }}\n"),
    )
    .unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "--quiet", "-m", "first"]);
    repo
}

fn commit_version(repo: &Path, text: &str, message: &str) -> String {
    std::fs::write(
        repo.join("src/lib.rs"),
        format!("pub fn version() -> &'static str {{ {text:?} }}\n"),
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
}

fn git_app(dep: &str) -> Project {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\ngitdep = {dep}\n"
        ),
    );
    p.write(
        "src/main.rs",
        "fn main() { println!(\"gitdep says {}\", gitdep::version()); }\n",
    );
    p
}

fn url(repo: &Path) -> String {
    format!("file://{}", repo.display())
}

#[test]
fn git_dependencies_build_lock_the_exact_commit_and_move_only_on_update() {
    let repo = upstream("v1");
    let first = git(repo.path(), &["rev-parse", "HEAD"]);
    let p = git_app(&format!("{{ git = {:?} }}", url(repo.path())));

    assert!(p.ok(&["run"]).contains("gitdep says v1"));

    // The lockfile records the resolved commit, not just a branch.
    let lock = p.read("Cargo.lock");
    assert!(
        lock.contains(&format!("git+{}#{first}", url(repo.path()))),
        "{lock}"
    );

    // A second build is fully cached: nothing is recompiled or refetched.
    let again = p.ok(&["build"]);
    assert!(again.contains("0 compiled"), "{again}");

    // The upstream moves on; a plain build stays on the locked commit…
    let second = commit_version(repo.path(), "v2", "second");
    assert_ne!(first, second);
    assert!(p.ok(&["run"]).contains("gitdep says v1"));

    // …until the user asks to update.
    p.ok(&["update"]);
    let run = p.ok(&["run"]);
    assert!(run.contains("gitdep says v2"), "{run}");
    assert!(
        p.read("Cargo.lock").contains(&format!("#{second}")),
        "{}",
        p.read("Cargo.lock")
    );
}

#[test]
fn git_dependencies_can_be_pinned_by_tag_branch_and_rev_and_the_key_follows_the_commit() {
    let repo = upstream("v1");
    git(repo.path(), &["tag", "release-1"]);
    let first = git(repo.path(), &["rev-parse", "HEAD"]);
    git(repo.path(), &["checkout", "--quiet", "-b", "next"]);
    commit_version(repo.path(), "next-work", "on next");
    git(repo.path(), &["checkout", "--quiet", "main"]);
    let u = url(repo.path());

    let by_tag = git_app(&format!("{{ git = {u:?}, tag = \"release-1\" }}"));
    assert!(by_tag.ok(&["run"]).contains("gitdep says v1"));
    assert!(by_tag.read("Cargo.lock").contains("?tag=release-1#"));

    let by_branch = git_app(&format!("{{ git = {u:?}, branch = \"next\" }}"));
    assert!(by_branch.ok(&["run"]).contains("gitdep says next-work"));
    assert!(by_branch.read("Cargo.lock").contains("?branch=next#"));

    let by_rev = git_app(&format!("{{ git = {u:?}, rev = {first:?} }}"));
    assert!(by_rev.ok(&["run"]).contains("gitdep says v1"));

    // A missing reference is a clear error, not a fallback to something else.
    let missing = git_app(&format!("{{ git = {u:?}, branch = \"nope\" }}"));
    let out = missing.rune(&["build"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("branch `nope`"), "{}", text(&out));
}

#[test]
fn git_repositories_with_their_own_workspace_inherit_from_it() {
    // A repo that is a workspace: members use `version.workspace = true`,
    // `[workspace.dependencies]` and depend on each other by path.
    let repo = tempfile::tempdir().unwrap();
    let d = repo.path();
    git(d, &["init", "--quiet", "--initial-branch=main"]);
    let w = |rel: &str, text: &str| {
        let p = d.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    w(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n[workspace.package]\nversion = \"2.5.0\"\nedition = \"2021\"\n[workspace.dependencies]\ncore-part = { path = \"crates/core-part\" }\n",
    );
    w(
        "crates/core-part/Cargo.toml",
        "[package]\nname = \"core-part\"\nversion.workspace = true\nedition.workspace = true\n",
    );
    w(
        "crates/core-part/src/lib.rs",
        "pub fn base() -> u32 { 40 }\n",
    );
    w(
        "crates/wrapper/Cargo.toml",
        "[package]\nname = \"wrapper\"\nversion.workspace = true\nedition.workspace = true\n[dependencies]\ncore-part.workspace = true\n",
    );
    w(
        "crates/wrapper/src/lib.rs",
        "pub fn answer() -> u32 { core_part::base() + 2 }\n",
    );
    git(d, &["add", "-A"]);
    git(d, &["commit", "--quiet", "-m", "workspace"]);

    let p = Project::new();
    p.write(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nwrapper = {{ git = {:?} }}\n",
            url(d)
        ),
    );
    p.write(
        "src/main.rs",
        "fn main() { println!(\"answer={}\", wrapper::answer()); }\n",
    );
    let out = p.ok(&["run"]);
    assert!(out.contains("answer=42"), "{out}");
    // The version came from the repository's `[workspace.package]`.
    assert!(
        p.read("Cargo.lock")
            .contains("name = \"wrapper\"\nversion = \"2.5.0\"")
    );
}

#[test]
fn git_submodules_are_checked_out_and_part_of_the_source() {
    let sub = tempfile::tempdir().unwrap();
    git(sub.path(), &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(sub.path().join("data.txt"), "from the submodule").unwrap();
    git(sub.path(), &["add", "-A"]);
    git(sub.path(), &["commit", "--quiet", "-m", "sub"]);

    let repo = upstream("v1");
    git(
        repo.path(),
        &[
            "submodule",
            "add",
            "--quiet",
            &url(sub.path()),
            "vendor-data",
        ],
    );
    std::fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn version() -> &'static str { include_str!(\"../vendor-data/data.txt\") }\n",
    )
    .unwrap();
    git(repo.path(), &["add", "-A"]);
    git(
        repo.path(),
        &["commit", "--quiet", "-m", "use the submodule"],
    );

    let p = git_app(&format!("{{ git = {:?} }}", url(repo.path())));
    let out = Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["run"])
        .current_dir(p.path())
        .env("RUNE_HOME", p.home())
        .env("NO_COLOR", "1")
        // Let git's submodule clone read a local (file://) repository.
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "protocol.file.allow")
        .env("GIT_CONFIG_VALUE_0", "always")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("gitdep says from the submodule"),
        "{}",
        text(&out)
    );
}

// ---------------------------------------------------------------------------
// [patch], [replace]
// ---------------------------------------------------------------------------

/// `app` depends on `foo` directly and through `mid`; `foo` is a registry
/// requirement that a patch redirects to a local fork.
fn patch_project(patch_section: &str) -> Project {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nfoo = \"1.0\"\nmid = {{ path = \"mid\" }}\n{patch_section}"
        ),
    );
    p.write(
        "mid/Cargo.toml",
        "[package]\nname = \"mid\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nfoo = \"1\"\n",
    );
    p.write(
        "mid/src/lib.rs",
        "pub fn via_mid() -> &'static str { foo::says() }\n",
    );
    p.write(
        "foo-fork/Cargo.toml",
        "[package]\nname = \"foo\"\nversion = \"1.5.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "foo-fork/src/lib.rs",
        "pub fn says() -> &'static str { \"the fork\" }\n",
    );
    p.write(
        "src/main.rs",
        "fn main() { println!(\"direct={} via_mid={}\", foo::says(), mid::via_mid()); }\n",
    );
    p
}

#[test]
fn patches_replace_a_dependency_for_every_dependent() {
    let p = patch_project(
        "[patch.crates-io]\nfoo = { path = \"foo-fork\" }\nnobody-uses-this = { path = \"foo-fork\" }\n",
    );
    let out = p.ok(&["run"]);
    assert!(out.contains("direct=the fork via_mid=the fork"), "{out}");
    // A patch that matches nothing is reported; the one in use is not.
    assert!(
        out.contains("patch `nobody-uses-this`") && out.contains("was not used"),
        "{out}"
    );
    assert!(!out.contains("patch `foo`"), "{out}");
    // One `foo`, the fork: a local package in the lockfile, at its own version.
    let lock = p.read("Cargo.lock");
    assert!(
        lock.contains("name = \"foo\"\nversion = \"1.5.0\"\n"),
        "{lock}"
    );
    assert!(!lock.contains("crates.io-index"), "{lock}");

    // Editing the fork rebuilds what depends on it (its key changed).
    p.write(
        "foo-fork/src/lib.rs",
        "pub fn says() -> &'static str { \"the edited fork\" }\n",
    );
    let out = p.ok(&["run"]);
    assert!(
        out.contains("direct=the edited fork via_mid=the edited fork"),
        "{out}"
    );
}

#[test]
fn replace_and_configuration_patches_work_too() {
    // The deprecated `[replace]`, for one exact version.
    let p = patch_project("[replace]\n\"foo:1.5.0\" = { path = \"foo-fork\" }\n");
    p.write(
        "Cargo.toml",
        &p.read("Cargo.toml")
            .replace("foo = \"1.0\"", "foo = \"=1.5.0\""),
    );
    p.write(
        "mid/Cargo.toml",
        &p.read("mid/Cargo.toml")
            .replace("foo = \"1\"", "foo = \"=1.5.0\""),
    );
    assert!(p.ok(&["run"]).contains("direct=the fork"));

    // `[patch]` from `.cargo/config.toml`, without touching the manifest.
    let q = patch_project("");
    q.write(
        ".cargo/config.toml",
        "[patch.crates-io]\nfoo = { path = \"foo-fork\" }\n",
    );
    assert!(q.ok(&["run"]).contains("via_mid=the fork"));
}

// ---------------------------------------------------------------------------
// Alternative registries and source replacement (local, no network)
// ---------------------------------------------------------------------------

/// Packs `name-version/` into a `.crate` and returns its bytes and SHA-256.
#[cfg(unix)]
fn make_crate(name: &str, version: &str, lib_rs: &str) -> (Vec<u8>, String) {
    make_crate_with(name, version, &[("src/lib.rs", lib_rs)])
}

/// Like [`make_crate`], with any files (paths relative to the package).
#[cfg(unix)]
fn make_crate_with(name: &str, version: &str, files: &[(&str, &str)]) -> (Vec<u8>, String) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(format!("{name}-{version}"));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    for (path, content) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let out = tmp.path().join("out.crate");
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(&out)
        .args(["-C"])
        .arg(tmp.path())
        .arg(format!("{name}-{version}"))
        .status()
        .expect("tar is required for these tests");
    assert!(status.success());
    let sum = Command::new("sha256sum")
        .arg(&out)
        .output()
        .expect("sha256sum is required");
    let sum = String::from_utf8_lossy(&sum.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    (std::fs::read(&out).unwrap(), sum)
}

fn registry_app(dep: &str) -> Project {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nfoo = {dep}\n"
        ),
    );
    p.write(
        "src/main.rs",
        "fn main() { println!(\"foo says {}\", foo::says()); }\n",
    );
    p
}

#[cfg(unix)]
const FOO_LIB: &str = "pub fn says() -> &'static str { \"hello from the registry\" }\n";

#[cfg(unix)]
#[test]
fn a_local_registry_replaces_crates_io_but_the_lockfile_keeps_the_identity() {
    let reg = tempfile::tempdir().unwrap();
    let (bytes, sum) = make_crate("foo", "1.0.0", FOO_LIB);
    std::fs::write(reg.path().join("foo-1.0.0.crate"), bytes).unwrap();
    std::fs::create_dir_all(reg.path().join("index/3/f")).unwrap();
    std::fs::write(
        reg.path().join("index/3/f/foo"),
        format!("{{\"name\":\"foo\",\"vers\":\"1.0.0\",\"deps\":[],\"cksum\":\"{sum}\",\"features\":{{}},\"yanked\":false}}\n"),
    )
    .unwrap();

    let p = registry_app("\"1\"");
    p.write(
        ".cargo/config.toml",
        &format!(
            "[source.crates-io]\nreplace-with = \"local\"\n[source.local]\nlocal-registry = {:?}\n",
            reg.path()
        ),
    );
    let out = p.ok(&["run"]);
    assert!(out.contains("foo says hello from the registry"), "{out}");
    let lock = p.read("Cargo.lock");
    // The package is still a crates.io package, verified by its checksum.
    assert!(
        lock.contains("registry+https://github.com/rust-lang/crates.io-index"),
        "{lock}"
    );
    assert!(lock.contains(&format!("checksum = \"{sum}\"")), "{lock}");
}

/// `(name, version, [(dependency, requirement)])`.
#[cfg(unix)]
type RegistryCrate<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

/// A local registry holding these crates.
#[cfg(unix)]
fn local_registry(crates: &[RegistryCrate]) -> tempfile::TempDir {
    let reg = tempfile::tempdir().unwrap();
    let mut index: std::collections::BTreeMap<String, String> = Default::default();
    for (name, version, deps) in crates {
        let (bytes, sum) = make_crate(name, version, "pub fn f() {}\n");
        std::fs::write(reg.path().join(format!("{name}-{version}.crate")), bytes).unwrap();
        let deps: Vec<String> = deps
            .iter()
            .map(|(d, req)| format!(
                "{{\"name\":\"{d}\",\"req\":\"{req}\",\"features\":[],\"optional\":false,\"default_features\":true,\"target\":null,\"kind\":\"normal\"}}"
            ))
            .collect();
        index.entry(name.to_string()).or_default().push_str(&format!(
            "{{\"name\":\"{name}\",\"vers\":\"{version}\",\"deps\":[{}],\"cksum\":\"{sum}\",\"features\":{{}},\"yanked\":false}}\n",
            deps.join(",")
        ));
    }
    for (name, lines) in index {
        let dir = match name.len() {
            1 => reg.path().join("index/1"),
            2 => reg.path().join("index/2"),
            3 => reg.path().join("index/3").join(&name[..1]),
            _ => reg.path().join("index").join(&name[..2]).join(&name[2..4]),
        };
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&name), lines).unwrap();
    }
    reg
}

/// An app depending on `deps` from `reg` (replacing crates.io).
#[cfg(unix)]
fn app_with(reg: &tempfile::TempDir, deps: &str) -> Project {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        &format!("[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\n{deps}"),
    );
    p.write("src/main.rs", "fn main() {}\n");
    p.write(
        ".cargo/config.toml",
        &format!(
            "[source.crates-io]\nreplace-with = \"local\"\n[source.local]\nlocal-registry = {:?}\n",
            reg.path()
        ),
    );
    p
}

/// `name version` of every package in `Cargo.lock`.
#[cfg(unix)]
fn locked(p: &Project) -> Vec<String> {
    let lock: toml::Table = p.read("Cargo.lock").parse().unwrap();
    let mut out: Vec<String> = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pkg| {
            format!(
                "{} {}",
                pkg["name"].as_str().unwrap(),
                pkg["version"].as_str().unwrap()
            )
        })
        .collect();
    out.sort();
    out
}

#[cfg(unix)]
#[test]
fn one_version_per_compatible_range_like_cargo() {
    // The first requirement alone would take `a 1.1.0`; `b` needs exactly
    // `1.0.0`, and one `1.x` is all Cargo allows.
    let reg = local_registry(&[
        ("a", "1.0.0", &[]),
        ("a", "1.1.0", &[]),
        ("b", "1.0.0", &[("a", "=1.0.0")]),
    ]);
    let p = app_with(&reg, "a = \"1.0\"\nb = \"1\"\n");
    p.ok(&["build"]);
    assert_eq!(locked(&p), ["a 1.0.0", "app 0.1.0", "b 1.0.0"]);
}

#[cfg(unix)]
#[test]
fn resolution_backtracks_to_an_older_release_that_fits() {
    // The newest `b` needs an `a` the app's pin rules out; an older `b` fits.
    let reg = local_registry(&[
        ("a", "1.0.0", &[]),
        ("a", "1.1.0", &[]),
        ("b", "1.0.0", &[("a", "^1.0")]),
        ("b", "2.0.0", &[("a", "^1.1")]),
    ]);
    let p = app_with(&reg, "a = \"=1.0.0\"\nb = \"*\"\n");
    p.ok(&["build"]);
    assert_eq!(locked(&p), ["a 1.0.0", "app 0.1.0", "b 1.0.0"]);
}

#[cfg(unix)]
#[test]
fn resolution_backtracks_several_crates_when_one_is_not_enough() {
    // Both newest `b` and newest `c` need an `a` the app's pin rules out:
    // no single requirement is the odd one out, both must move back.
    let reg = local_registry(&[
        ("a", "1.0.0", &[]),
        ("a", "1.1.0", &[]),
        ("b", "1.0.0", &[("a", "^1.0")]),
        ("b", "2.0.0", &[("a", "^1.1")]),
        ("c", "1.0.0", &[("a", "^1.0")]),
        ("c", "2.0.0", &[("a", "^1.1")]),
    ]);
    let p = app_with(&reg, "a = \"=1.0.0\"\nb = \"*\"\nc = \"*\"\n");
    p.ok(&["build"]);
    assert_eq!(locked(&p), ["a 1.0.0", "app 0.1.0", "b 1.0.0", "c 1.0.0"]);
}

#[cfg(unix)]
#[test]
fn update_precise_and_dry_run_move_one_package_like_cargo() {
    let reg = local_registry(&[
        ("foo", "1.0.0", &[]),
        ("foo", "1.1.0", &[]),
        ("foo", "1.2.0", &[]),
    ]);
    let p = app_with(&reg, "foo = \"1\"\n");
    p.ok(&["build"]);
    assert_eq!(locked(&p), ["app 0.1.0", "foo 1.2.0"]);

    // A dry run reports and changes nothing.
    let out = p.ok(&["update", "-p", "foo", "--precise", "1.0.0", "--dry-run"]);
    assert!(out.contains("foo v1.2.0 -> v1.0.0"), "{out}");
    assert_eq!(locked(&p), ["app 0.1.0", "foo 1.2.0"]);

    p.ok(&["update", "-p", "foo", "--precise", "1.0.0"]);
    assert_eq!(locked(&p), ["app 0.1.0", "foo 1.0.0"]);
    // …and the build uses it without resolving again.
    assert!(p.ok(&["build"]).contains("compiled"));
    assert_eq!(locked(&p), ["app 0.1.0", "foo 1.0.0"]);

    // A version the requirements rule out is an error, and changes nothing.
    let out = p.rune(&["update", "foo", "--precise", "2.0.0"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("cannot update `foo` to 2.0.0"),
        "{}",
        text(&out)
    );
    assert_eq!(locked(&p), ["app 0.1.0", "foo 1.0.0"]);

    // Updating it normally goes back to the newest.
    p.ok(&["update", "foo"]);
    assert_eq!(locked(&p), ["app 0.1.0", "foo 1.2.0"]);
}

#[cfg(unix)]
#[test]
fn add_lists_the_features_it_turns_on_like_cargo() {
    let reg = local_registry(&[("foo", "1.0.0", &[])]);
    // Features, two optional dependencies (one hidden behind `dep:`), and
    // what the index says about the release.
    let optional = |name: &str| {
        format!(
            "{{\"name\":\"{name}\",\"req\":\"1\",\"features\":[],\"optional\":true,\"default_features\":true,\"target\":null,\"kind\":\"normal\"}}"
        )
    };
    let file = reg.path().join("index/3/f/foo");
    let record = std::fs::read_to_string(&file)
        .unwrap()
        .replace(
            "\"deps\":[]",
            &format!("\"deps\":[{},{}]", optional("foo_derive"), optional("bar")),
        )
        .replace(
            "\"features\":{}",
            "\"features\":{\"default\":[\"std\"],\"std\":[],\"derive\":[\"dep:foo_derive\"],\"extra\":[\"bar\"]},\"rust_version\":\"1.70\",\"pubtime\":\"2026-01-02T03:04:05Z\"",
        );
    std::fs::write(&file, record).unwrap();
    let p = app_with(&reg, "");
    let out = p.ok(&["add", "foo", "-F", "extra", "--dry-run"]);
    let listed: Vec<&str> = out
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("+ ") || l.starts_with("- "))
        .collect();
    assert_eq!(listed, ["+ bar", "+ extra", "+ std", "- derive"], "{out}");
    assert!(
        out.contains("needs Rust 1.70, published 2026-01-02"),
        "{out}"
    );
    let quiet = p.ok(&["add", "foo", "--no-default-features", "--dry-run"]);
    assert!(quiet.contains("- std"), "{quiet}");
}

#[cfg(unix)]
#[test]
fn add_supports_renames_platforms_workspace_deps_and_replaced_sources() {
    let reg = local_registry(&[("foo", "1.0.0", &[]), ("foo", "1.4.2", &[])]);
    let p = app_with(&reg, "");
    // The index comes from the replaced source, not crates.io.
    p.ok(&["add", "foo", "--rename", "bar", "--target", "cfg(unix)"]);
    let manifest = p.read("Cargo.toml");
    assert!(
        manifest.contains("[target.'cfg(unix)'.dependencies]")
            && manifest.contains("bar = { version = \"1.4.2\", package = \"foo\" }"),
        "{manifest}"
    );
    // `--dry-run` changes nothing.
    let out = p.ok(&["add", "foo@1.0", "--dev", "--dry-run"]);
    assert!(out.contains("would add"), "{out}");
    assert_eq!(p.read("Cargo.toml"), manifest);

    // In a workspace that declares it, a member inherits it.
    let w = Project::new();
    w.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"m\"]\n\n[workspace.dependencies]\nfoo = \"1.0.0\"\n",
    );
    w.write(
        "m/Cargo.toml",
        "[package]\nname = \"m\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    w.write("m/src/lib.rs", "");
    w.write(".cargo/config.toml", &p.read(".cargo/config.toml"));
    w.ok(&["add", "foo", "-p", "m"]);
    assert!(
        w.read("m/Cargo.toml")
            .contains("foo = { workspace = true }")
    );
}

#[test]
fn clippy_lints_members_while_build_scripts_see_the_real_rustc() {
    if Command::new("clippy-driver")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("clippy-driver not installed; skipping");
        return;
    }
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    // What libc's and serde's build scripts do.
    p.write(
        "build.rs",
        r#"fn main() {
    let rustc = std::env::var("RUSTC").unwrap();
    let out = std::process::Command::new(rustc).arg("--version").output().unwrap();
    let v = String::from_utf8(out.stdout).unwrap();
    assert!(v.starts_with("rustc "), "RUSTC is not rustc: {v}");
}
"#,
    );
    p.write(
        "src/main.rs",
        "fn five() -> i32 { return 5; }\nfn main() { println!(\"{}\", five()); }\n",
    );
    // A warn-level lint: reported, but not fatal…
    let out = p.ok(&["clippy"]);
    assert!(
        out.contains("needless_return") || out.contains("unneeded `return`"),
        "{out}"
    );
    // …until `-D warnings` says so.
    let out = p.rune(&["clippy", "--", "-D", "warnings"]);
    assert!(!out.status.success(), "{}", text(&out));
}

#[test]
fn trim_paths_keeps_this_machines_paths_out_of_the_binary() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}\n[profile.release]\ntrim-paths = true\n"),
    );
    p.write("src/main.rs", "fn main() { panic!(\"where am I\"); }\n");
    p.ok(&["build", "--release"]);
    let exe = p.path().join(".rune/target/release/demo");
    let run = Command::new(&exe).output().unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("src/main.rs:1"), "{stderr}");
    let root = p.path().canonicalize().unwrap();
    assert!(!stderr.contains(root.to_str().unwrap()), "{stderr}");
    let bytes = std::fs::read(&exe).unwrap();
    for path in [root.to_str().unwrap(), p.home().to_str().unwrap()] {
        assert!(
            !bytes.windows(path.len()).any(|w| w == path.as_bytes()),
            "the binary still names {path}"
        );
    }

    // Without it, the workspace's own paths are still relative to its root,
    // as with Cargo (it compiles them from there), so panics name
    // `src/main.rs` either way.
    p.write("Cargo.toml", LIB_MANIFEST);
    p.ok(&["build", "--release"]);
    let run = Command::new(&exe).output().unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("src/main.rs:1"), "{stderr}");
    assert!(!stderr.contains(root.to_str().unwrap()), "{stderr}");
}

#[test]
fn compiler_messages_name_workspace_files_relative_to_the_root() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\"]\n",
    );
    p.write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("app/src/main.rs", "fn main() { let x: u32 = \"no\"; }\n");
    let out = p.rune(&["build"]);
    assert!(!out.status.success());
    let text = text(&out);
    assert!(text.contains("--> app/src/main.rs:1:26"), "{text}");
    let root = p.path().canonicalize().unwrap();
    assert!(!text.contains(root.to_str().unwrap()), "{text}");
    // One verdict, after the compiler's message.
    let verdict = text.find("could not compile app v0.1.0").expect(&text);
    assert!(verdict > text.find("mismatched types").unwrap(), "{text}");
    assert_eq!(text.matches("could not compile").count(), 1, "{text}");

    // `rune fix` finds the files through those relative paths.
    p.write(
        "app/src/main.rs",
        "#![warn(unused_parens)]\nfn main() { let x = (1); let _ = x; }\n",
    );
    p.ok(&["fix", "--allow-no-vcs"]);
    assert!(
        p.read("app/src/main.rs").contains("let x = 1;"),
        "{}",
        p.read("app/src/main.rs")
    );
}

#[test]
fn explain_says_what_changed_since_the_last_build() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"core\", \"app\"]\nresolver = \"2\"\n",
    );
    p.write(
        "core/Cargo.toml",
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("core/src/lib.rs", "pub fn f() -> u8 { 1 }\n");
    p.write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\ncore = { path = \"../core\" }\n",
    );
    p.write("app/src/main.rs", "fn main() { core::f(); }\n");
    p.ok(&["build"]);
    assert!(p.ok(&["explain"]).contains("nothing would be compiled"));

    // A file edited and one added: named, and the dependent follows.
    p.write("core/src/lib.rs", "pub fn f() -> u8 { 2 }\n");
    p.write("core/src/extra.rs", "\n");
    let out = p.ok(&["explain"]);
    assert!(
        out.contains("sources changed: src/extra.rs (added), src/lib.rs (modified)"),
        "{out}"
    );
    assert!(out.contains("a dependency changed: core"), "{out}");
    assert!(out.contains("2 of 2 units would be compiled"), "{out}");

    // After building, it is all in the store again.
    p.ok(&["build"]);
    assert!(p.ok(&["explain"]).contains("nothing would be compiled"));
}

#[test]
fn fmt_check_after_the_separator_works_as_in_ci() {
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    p.write("src/main.rs", "fn main(){let x=1;println!(\"{}\",x);}\n");
    // `cargo fmt --all -- --check`, the usual CI step.
    let out = p.rune(&["fmt", "--all", "--", "--check"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        p.read("src/main.rs").starts_with("fn main(){"),
        "--check must not write"
    );
    p.ok(&["fmt"]);
    p.ok(&["fmt", "--all", "--", "--check"]);
}

#[cfg(unix)]
#[test]
fn install_from_a_registry_lists_upgrades_and_uninstalls_like_cargo() {
    let reg = tempfile::tempdir().unwrap();
    let mut index = String::new();
    for version in ["1.0.0", "1.1.0"] {
        let main = format!("fn main() {{ println!(\"tool {version}\"); }}\n");
        let (bytes, sum) = make_crate_with("tool", version, &[("src/main.rs", &main)]);
        std::fs::write(reg.path().join(format!("tool-{version}.crate")), bytes).unwrap();
        index.push_str(&format!(
            "{{\"name\":\"tool\",\"vers\":\"{version}\",\"deps\":[],\"cksum\":\"{sum}\",\"features\":{{}},\"yanked\":false}}\n"
        ));
    }
    std::fs::create_dir_all(reg.path().join("index/to/ol")).unwrap();
    std::fs::write(reg.path().join("index/to/ol/tool"), index).unwrap();

    // No project: the configuration of the working directory applies.
    let p = Project::new();
    p.write(
        ".cargo/config.toml",
        &format!(
            "[source.crates-io]\nreplace-with = \"local\"\n[source.local]\nlocal-registry = {:?}\n",
            reg.path()
        ),
    );
    let root = p.path().join("installed");
    let root_arg = root.to_str().unwrap();
    let run_tool = || {
        let out = Command::new(root.join("bin/tool")).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    p.ok(&["install", "tool@1.0.0", "--root", root_arg]);
    assert_eq!(run_tool(), "tool 1.0.0");
    let list = p.ok(&["install", "--list", "--root", root_arg]);
    assert!(list.contains("tool v1.0.0:\n    tool"), "{list}");
    // Cargo's tracking file, so `cargo install --list` agrees.
    let tracking = std::fs::read_to_string(root.join(".crates.toml")).unwrap();
    assert!(
        tracking.contains(
            "\"tool 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"tool\"]"
        ),
        "{tracking}"
    );
    assert!(root.join(".crates2.json").is_file());

    // The same version again is a no-op; a newer one replaces it.
    let again = p.ok(&["install", "tool@1.0.0", "--root", root_arg]);
    assert!(again.contains("already installed"), "{again}");
    p.ok(&["install", "tool", "--root", root_arg]);
    assert_eq!(run_tool(), "tool 1.1.0");
    let tracking = std::fs::read_to_string(root.join(".crates.toml")).unwrap();
    assert!(
        tracking.contains("tool 1.1.0") && !tracking.contains("tool 1.0.0"),
        "{tracking}"
    );

    p.ok(&["uninstall", "tool", "--root", root_arg]);
    assert!(!root.join("bin/tool").exists());
    let out = p.rune(&["uninstall", "tool", "--root", root_arg]);
    assert!(!out.status.success());
    assert!(text(&out).contains("not installed"), "{}", text(&out));
    // Nothing is left behind from the scratch build.
    let scratch = p.home().join("install");
    assert!(
        !scratch.exists() || std::fs::read_dir(&scratch).unwrap().next().is_none(),
        "scratch builds were not cleaned up"
    );
}

#[cfg(unix)]
#[test]
fn a_cargo_lock_changed_outside_rune_is_respected_not_reverted() {
    // `git pull` / `git checkout` / `cargo update --precise` change
    // `Cargo.lock` behind Rune's back; its binary cache must not win.
    let reg = tempfile::tempdir().unwrap();
    let mut index = String::new();
    let mut sums = Vec::new();
    for (version, says) in [("1.0.0", "one-oh"), ("1.1.0", "one-one")] {
        let lib = format!("pub fn says() -> &'static str {{ \"{says}\" }}\n");
        let (bytes, sum) = make_crate("foo", version, &lib);
        std::fs::write(reg.path().join(format!("foo-{version}.crate")), bytes).unwrap();
        index.push_str(&format!(
            "{{\"name\":\"foo\",\"vers\":\"{version}\",\"deps\":[],\"cksum\":\"{sum}\",\"features\":{{}},\"yanked\":false}}\n"
        ));
        sums.push(sum);
    }
    std::fs::create_dir_all(reg.path().join("index/3/f")).unwrap();
    std::fs::write(reg.path().join("index/3/f/foo"), index).unwrap();

    let p = registry_app("\"1\"");
    p.write(
        ".cargo/config.toml",
        &format!(
            "[source.crates-io]\nreplace-with = \"local\"\n[source.local]\nlocal-registry = {:?}\n",
            reg.path()
        ),
    );
    assert!(p.ok(&["run"]).contains("foo says one-one"));

    // The lockfile now pins 1.0.0, as after checking out an older commit.
    let older = p
        .read("Cargo.lock")
        .replace("version = \"1.1.0\"", "version = \"1.0.0\"")
        .replace(&sums[1], &sums[0]);
    p.write("Cargo.lock", &older);
    let out = p.ok(&["run"]);
    assert!(out.contains("foo says one-oh"), "{out}");
    assert_eq!(p.read("Cargo.lock"), older);
    // `fetch` and `tree` do not move it either.
    p.ok(&["fetch"]);
    assert!(p.ok(&["tree"]).contains("foo v1.0.0"));
    assert_eq!(p.read("Cargo.lock"), older);
}

#[cfg(unix)]
#[test]
fn vendored_directories_replace_crates_io_for_offline_builds() {
    let (_, sum) = make_crate("foo", "1.0.0", FOO_LIB);
    let p = registry_app("\"1\"");
    p.write(
        "vendor/foo-1.0.0/Cargo.toml",
        "[package]\nname = \"foo\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
    );
    p.write("vendor/foo-1.0.0/src/lib.rs", FOO_LIB);
    p.write(
        "vendor/foo-1.0.0/.cargo-checksum.json",
        &format!("{{\"package\":\"{sum}\",\"files\":{{}}}}"),
    );
    p.write(
        ".cargo/config.toml",
        "[source.crates-io]\nreplace-with = \"vendored\"\n[source.vendored]\ndirectory = \"vendor\"\n",
    );
    let out = p.ok(&["run", "--offline"]);
    assert!(out.contains("foo says hello from the registry"), "{out}");
    assert!(
        p.read("Cargo.lock")
            .contains(&format!("checksum = \"{sum}\""))
    );
}

/// A minimal sparse registry: serves `config.json`, one index file and one
/// `.crate`, and answers 401 unless the request carries the expected token.
#[cfg(unix)]
fn serve_registry(token: &'static str, crate_bytes: Vec<u8>, index_line: String) -> u16 {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let crate_bytes = crate_bytes.clone();
            let index_line = index_line.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                // Read the request head (up to the blank line).
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let authorized = head.lines().any(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("authorization:")
                        .is_some_and(|v| v.trim() == token.to_ascii_lowercase())
                });
                let (status, body): (&str, Vec<u8>) = if !authorized {
                    ("401 Unauthorized", b"auth required".to_vec())
                } else if path == "/config.json" {
                    (
                        "200 OK",
                        format!("{{\"dl\":\"http://127.0.0.1:{port}/dl\",\"api\":\"http://127.0.0.1:{port}\",\"auth-required\":true}}")
                            .into_bytes(),
                    )
                } else if path == "/3/f/foo" {
                    ("200 OK", index_line.into_bytes())
                } else if path == "/dl/foo/1.0.0/download" {
                    ("200 OK", crate_bytes)
                } else {
                    ("404 Not Found", Vec::new())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    port
}

/// What the publishing mock registry holds: `(name, version)` → crate bytes,
/// and each crate's index lines.
#[cfg(unix)]
#[derive(Default)]
struct MockRegistry {
    crates: std::collections::HashMap<(String, String), Vec<u8>>,
    index: std::collections::BTreeMap<String, Vec<serde_json::Value>>,
}

/// A registry with a web API: reads are open, `/api/v1/…` writes need `token`.
#[cfg(unix)]
fn serve_publishing_registry(
    token: &'static str,
) -> (u16, std::sync::Arc<std::sync::Mutex<MockRegistry>>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let state = std::sync::Arc::new(std::sync::Mutex::new(MockRegistry::default()));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let state = shared.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let mut first = head.lines().next().unwrap_or("").split_whitespace();
                let method = first.next().unwrap_or("").to_string();
                let target = first.next().unwrap_or("/").to_string();
                let header = |name: &str| {
                    head.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                    })
                };
                let len: usize = header("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                if stream.read_exact(&mut body).is_err() {
                    return;
                }
                let authorized = header("authorization").as_deref() == Some(token);
                let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
                let json = |v: serde_json::Value| ("200 OK", v.to_string().into_bytes());
                let mut st = state.lock().unwrap();
                let (status, out): (&str, Vec<u8>) = match (method.as_str(), segs.as_slice()) {
                    ("GET", ["config.json"]) => json(serde_json::json!({
                        "dl": format!("http://127.0.0.1:{port}/dl"),
                        "api": format!("http://127.0.0.1:{port}"),
                    })),
                    ("GET", ["dl", name, version, "download"]) => {
                        match st.crates.get(&(name.to_string(), version.to_string())) {
                            Some(b) => ("200 OK", b.clone()),
                            None => ("404 Not Found", Vec::new()),
                        }
                    }
                    ("PUT", ["api", "v1", "crates", "new"]) if authorized => {
                        let n = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
                        let meta: serde_json::Value =
                            serde_json::from_slice(&body[4..4 + n]).unwrap();
                        let c = u32::from_le_bytes(body[4 + n..8 + n].try_into().unwrap()) as usize;
                        let bytes = body[8 + n..8 + n + c].to_vec();
                        let tmp = tempfile::NamedTempFile::new().unwrap();
                        std::fs::write(tmp.path(), &bytes).unwrap();
                        let sum = Command::new("sha256sum").arg(tmp.path()).output().unwrap();
                        let sum = String::from_utf8_lossy(&sum.stdout)
                            .split_whitespace()
                            .next()
                            .unwrap()
                            .to_string();
                        let name = meta["name"].as_str().unwrap().to_string();
                        let vers = meta["vers"].as_str().unwrap().to_string();
                        let deps: Vec<serde_json::Value> = meta["deps"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|d| {
                                let alias = d["explicit_name_in_toml"].as_str();
                                serde_json::json!({
                                    "name": alias.unwrap_or(d["name"].as_str().unwrap()),
                                    "package": alias.map(|_| d["name"].clone()),
                                    "req": d["version_req"],
                                    "features": d["features"],
                                    "optional": d["optional"],
                                    "default_features": d["default_features"],
                                    "target": d["target"],
                                    "kind": d["kind"],
                                    "registry": d["registry"],
                                })
                            })
                            .collect();
                        st.index
                            .entry(name.clone())
                            .or_default()
                            .push(serde_json::json!({
                                "name": name, "vers": vers, "deps": deps, "cksum": sum,
                                "features": meta["features"], "yanked": false,
                            }));
                        st.crates.insert((name, vers), bytes);
                        json(
                            serde_json::json!({"warnings": {"invalid_categories": [], "invalid_badges": [], "other": []}}),
                        )
                    }
                    ("DELETE" | "PUT", ["api", "v1", "crates", name, version, action])
                        if authorized && (*action == "yank" || *action == "unyank") =>
                    {
                        for line in st.index.get_mut(*name).into_iter().flatten() {
                            if line["vers"] == *version {
                                line["yanked"] = serde_json::json!(*action == "yank");
                            }
                        }
                        json(serde_json::json!({"ok": true}))
                    }
                    ("GET", ["api", "v1", "crates", _, "owners"]) if authorized => {
                        json(serde_json::json!({"users": [{"login": "ada", "name": "Ada"}]}))
                    }
                    ("GET", ["api", "v1", "crates"]) => {
                        let q = query
                            .split('&')
                            .find_map(|kv| kv.strip_prefix("q="))
                            .unwrap_or("");
                        let hits: Vec<serde_json::Value> = st
                            .index
                            .iter()
                            .filter(|(n, _)| n.contains(q))
                            .map(|(n, lines)| {
                                serde_json::json!({
                                    "name": n,
                                    "max_version": lines.last().unwrap()["vers"],
                                    "description": "mock crate",
                                })
                            })
                            .collect();
                        json(serde_json::json!({"crates": hits, "meta": {"total": hits.len()}}))
                    }
                    ("GET", [.., name]) if st.index.contains_key(*name) => {
                        let text: String =
                            st.index[*name].iter().map(|l| format!("{l}\n")).collect();
                        ("200 OK", text.into_bytes())
                    }
                    ("PUT" | "DELETE", ["api", ..]) => (
                        "403 Forbidden",
                        br#"{"errors":[{"detail":"must be logged in"}]}"#.to_vec(),
                    ),
                    _ => ("404 Not Found", Vec::new()),
                };
                drop(st);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    out.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&out);
                let _ = stream.flush();
            });
        }
    });
    (port, state)
}

#[cfg(unix)]
#[test]
fn packages_are_published_installed_yanked_and_owned_through_a_registry_api() {
    let (port, state) = serve_publishing_registry("tok-123");
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"lib\", \"app\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.3.0\"\nedition = \"2021\"\nlicense = \"MIT\"\n",
    );
    p.write(
        "lib/Cargo.toml",
        "[package]\nname = \"mocklib\"\nversion.workspace = true\nedition.workspace = true\nlicense.workspace = true\ndescription = \"lib\"\n",
    );
    p.write("lib/src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    p.write(
        "app/Cargo.toml",
        "[package]\nname = \"mockapp\"\nversion.workspace = true\nedition.workspace = true\nlicense.workspace = true\ndescription = \"app\"\n\n[dependencies]\nmocklib = { path = \"../lib\", version = \"0.3.0\", registry = \"mock\" }\n",
    );
    p.write(
        "app/src/main.rs",
        "fn main() { println!(\"published answer {}\", mocklib::answer()); }\n",
    );
    p.write(".gitignore", ".rune\n");
    p.write(
        ".cargo/config.toml",
        &format!("[registries.mock]\nindex = \"sparse+http://127.0.0.1:{port}/\"\n"),
    );
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(p.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success()
        )
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);

    // Credentials go to a CARGO_HOME of the test's own.
    let cargo_home = tempfile::tempdir().unwrap();
    let rune = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(args)
            .current_dir(p.path())
            .env("RUNE_HOME", p.home())
            .env("CARGO_HOME", cargo_home.path())
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    };
    let ok = |args: &[&str]| {
        let out = rune(args);
        assert!(
            out.status.success(),
            "`rune {}` failed:\n{}",
            args.join(" "),
            text(&out)
        );
        text(&out)
    };

    // Without a token the registry refuses, with a clear message.
    let out = rune(&[
        "publish",
        "-p",
        "mocklib",
        "--registry",
        "mock",
        "--no-verify",
    ]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("rune login --registry mock"),
        "{}",
        text(&out)
    );

    ok(&["login", "--registry", "mock", "tok-123"]);
    let creds = std::fs::read_to_string(cargo_home.path().join("credentials.toml")).unwrap();
    assert!(
        creds.contains("[registries.mock]") && creds.contains("tok-123"),
        "{creds}"
    );

    let out = ok(&["publish", "-p", "mocklib", "--registry", "mock"]);
    assert!(out.contains("uploaded mocklib v0.3.0"), "{out}");
    // The app depends on the library through the registry now.
    ok(&["publish", "-p", "mockapp", "--registry", "mock"]);
    {
        let st = state.lock().unwrap();
        let app = &st.index["mockapp"][0];
        assert_eq!(app["deps"][0]["name"], "mocklib");
        assert_eq!(app["deps"][0]["req"], "^0.3.0");
        assert!(app["deps"][0]["registry"].is_null(), "{app}");
        // The uploaded manifest has no path dependency left.
        let bytes = st.crates[&("mockapp".to_string(), "0.3.0".to_string())].clone();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.crate"), bytes).unwrap();
        let manifest = Command::new("tar")
            .args(["xzf", "a.crate", "-O", "mockapp-0.3.0/Cargo.toml"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let manifest = String::from_utf8_lossy(&manifest.stdout);
        assert!(!manifest.contains("path ="), "{manifest}");
        assert!(manifest.contains("version = \"0.3.0\""), "{manifest}");
    }

    // Anyone can install it from there.
    let root = p.path().join("bin-root");
    ok(&[
        "install",
        "mockapp",
        "--registry",
        "mock",
        "--root",
        root.to_str().unwrap(),
    ]);
    let run = Command::new(root.join("bin/mockapp")).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run.stdout),
        "published answer 42\n"
    );

    let found = ok(&["search", "mock", "--registry", "mock"]);
    assert!(found.contains("mocklib = \"0.3.0\""), "{found}");
    assert!(ok(&["owner", "--list", "mocklib", "--registry", "mock"]).contains("ada (Ada)"));
    ok(&["yank", "mocklib@0.3.0", "--registry", "mock"]);
    assert_eq!(state.lock().unwrap().index["mocklib"][0]["yanked"], true);
    ok(&["yank", "mocklib@0.3.0", "--undo", "--registry", "mock"]);
    assert_eq!(state.lock().unwrap().index["mocklib"][0]["yanked"], false);

    ok(&["logout", "--registry", "mock"]);
    let creds = std::fs::read_to_string(cargo_home.path().join("credentials.toml")).unwrap();
    assert!(!creds.contains("tok-123"), "{creds}");
}

#[cfg(unix)]
#[test]
fn registry_tokens_come_from_external_credential_providers() {
    let (port, state) = serve_publishing_registry("tok-from-provider");
    state
        .lock()
        .unwrap()
        .index
        .entry("foo".into())
        .or_default()
        .push(serde_json::json!({"name": "foo", "vers": "1.0.0", "deps": [], "cksum": "00", "features": {}, "yanked": false}));
    let p = Project::new();
    // A provider per Cargo's credential protocol, and nothing in
    // credentials.toml.
    p.write(
        "provider.sh",
        "#!/bin/sh\necho '{\"v\":[1]}'\nread req\necho '{\"Ok\":{\"kind\":\"get\",\"token\":\"tok-from-provider\",\"cache\":\"session\",\"operation_independent\":true}}'\n",
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        p.path().join("provider.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    p.write(
        ".cargo/config.toml",
        &format!(
            "[registries.mock]\nindex = \"sparse+http://127.0.0.1:{port}/\"\ncredential-provider = {:?}\n",
            p.path().join("provider.sh").display().to_string()
        ),
    );
    let cargo_home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["yank", "foo@1.0.0", "--registry", "mock"])
        .current_dir(p.path())
        .env("RUNE_HOME", p.home())
        .env("CARGO_HOME", cargo_home.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(state.lock().unwrap().index["foo"][0]["yanked"], true);
}

#[cfg(unix)]
#[test]
fn git_based_registry_indexes_resolve_download_and_lock_like_cargo() {
    // The index is a git repository; the `.crate` files come from the `dl`
    // URL its `config.json` names.
    let (port, state) = serve_publishing_registry("unused");
    let (bytes, sum) = make_crate("foo", "1.0.0", FOO_LIB);
    state
        .lock()
        .unwrap()
        .crates
        .insert(("foo".into(), "1.0.0".into()), bytes);
    let index = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(index.path().join("3/f")).unwrap();
    std::fs::write(
        index.path().join("3/f/foo"),
        format!("{{\"name\":\"foo\",\"vers\":\"1.0.0\",\"deps\":[],\"cksum\":\"{sum}\",\"features\":{{}},\"yanked\":false}}\n"),
    )
    .unwrap();
    std::fs::write(
        index.path().join("config.json"),
        format!("{{\"dl\":\"http://127.0.0.1:{port}/dl\"}}"),
    )
    .unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "-A"],
        &["commit", "-qm", "index"],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(index.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success()
        );
    }
    let url = format!("file://{}", index.path().display());
    let p = registry_app("{ version = \"1\", registry = \"corp\" }");
    p.write(
        ".cargo/config.toml",
        &format!("[registries.corp]\nindex = \"{url}\"\n"),
    );
    let out = p.ok(&["run"]);
    assert!(out.contains("foo says hello from the registry"), "{out}");
    let lock = p.read("Cargo.lock");
    assert!(
        lock.contains(&format!("source = \"registry+{url}\"")),
        "{lock}"
    );
    assert!(lock.contains(&format!("checksum = \"{sum}\"")), "{lock}");
    // And again from the lockfile, offline.
    std::fs::remove_dir_all(p.path().join(".rune")).unwrap();
    assert!(p.ok(&["run", "--offline"]).contains("foo says"));
}

#[cfg(unix)]
#[test]
fn authenticated_alternative_registries_resolve_download_and_are_part_of_the_identity() {
    let (bytes, sum) = make_crate("foo", "1.0.0", FOO_LIB);
    let line = format!(
        "{{\"name\":\"foo\",\"vers\":\"1.0.0\",\"deps\":[],\"cksum\":\"{sum}\",\"features\":{{}},\"yanked\":false}}\n"
    );
    let port = serve_registry("secret-token", bytes, line);
    let index = format!("sparse+http://127.0.0.1:{port}/");

    let p = registry_app("{ version = \"1\", registry = \"corp\" }");
    p.write(
        ".cargo/config.toml",
        &format!("[registries.corp]\nindex = {index:?}\n"),
    );

    // Without credentials: a clear error that names the registry.
    let out = p.rune(&["build"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("requires authentication"),
        "{}",
        text(&out)
    );

    // With a token from the environment, exactly as Cargo reads it.
    let with_token = Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["run"])
        .current_dir(p.path())
        .env("RUNE_HOME", p.home())
        .env("NO_COLOR", "1")
        .env("CARGO_REGISTRIES_CORP_TOKEN", "secret-token")
        .output()
        .unwrap();
    assert!(with_token.status.success(), "{}", text(&with_token));
    assert!(
        text(&with_token).contains("foo says hello from the registry"),
        "{}",
        text(&with_token)
    );

    // The registry is recorded in the lockfile: `foo 1.0.0` from `corp` is not
    // `foo 1.0.0` from crates.io.
    let lock = p.read("Cargo.lock");
    assert!(lock.contains(&format!("source = \"{index}\"")), "{lock}");
    assert!(!lock.contains("crates.io-index"), "{lock}");
}

#[test]
fn an_undefined_registry_is_an_error_not_a_fallback_to_crates_io() {
    let p = registry_app("{ version = \"1\", registry = \"nowhere\" }");
    let out = p.rune(&["build"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("registry `nowhere` is not defined"),
        "{}",
        text(&out)
    );
}

// ---------------------------------------------------------------------------
// External inputs: env!, rerun-if-*, files outside the package
// ---------------------------------------------------------------------------

/// Runs `rune run` with one extra environment variable (or none).
fn run_with_env(p: &Project, var: &str, value: Option<&str>) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rune"));
    cmd.args(["run"])
        .current_dir(p.path())
        .env("RUNE_HOME", p.home())
        .env("NO_COLOR", "1")
        .env_remove(var);
    if let Some(v) = value {
        cmd.env(var, v);
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    text(&out)
}

#[test]
fn option_env_reads_are_part_of_the_cache_key() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "src/main.rs",
        "fn main() { println!(\"tag={}\", option_env!(\"RUNE_TEST_BUILD_TAG\").unwrap_or(\"none\")); }\n",
    );
    let a = run_with_env(&p, "RUNE_TEST_BUILD_TAG", Some("a"));
    assert!(a.contains("tag=a") && a.contains("1 compiled"), "{a}");
    // A different value must not be served from the artifact built for `a`…
    let b = run_with_env(&p, "RUNE_TEST_BUILD_TAG", Some("b"));
    assert!(
        b.contains("tag=b") && b.contains("1 compiled"),
        "stale artifact reused: {b}"
    );
    let unset = run_with_env(&p, "RUNE_TEST_BUILD_TAG", None);
    assert!(unset.contains("tag=none"), "{unset}");
    // …but going back to a value seen before is a cache hit, not a rebuild.
    let a_again = run_with_env(&p, "RUNE_TEST_BUILD_TAG", Some("a"));
    assert!(
        a_again.contains("tag=a") && a_again.contains("0 compiled"),
        "{a_again}"
    );
}

#[test]
fn build_script_rerun_if_env_changed_and_external_files_invalidate_the_cache() {
    let outside = tempfile::tempdir().unwrap();
    let header = outside.path().join("config.h");
    std::fs::write(&header, "first").unwrap();

    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n",
    );
    p.write(
        "build.rs",
        &format!(
            r#"fn main() {{
    println!("cargo::rerun-if-env-changed=RUNE_TEST_SCRIPT_VAR");
    println!("cargo::rerun-if-changed={}");
    let var = std::env::var("RUNE_TEST_SCRIPT_VAR").unwrap_or_default();
    let header = std::fs::read_to_string({:?}).unwrap();
    println!("cargo::rustc-env=SEEN={{}}/{{}}", var, header);
}}
"#,
            header.display(),
            header.display()
        ),
    );
    p.write(
        "src/main.rs",
        "fn main() { println!(\"seen={}\", env!(\"SEEN\")); }\n",
    );

    let first = run_with_env(&p, "RUNE_TEST_SCRIPT_VAR", Some("1"));
    assert!(first.contains("seen=1/first"), "{first}");
    // The declared environment variable changed.
    let second = run_with_env(&p, "RUNE_TEST_SCRIPT_VAR", Some("2"));
    assert!(
        second.contains("seen=2/first"),
        "stale build-script result: {second}"
    );
    // Back to a known state: cached.
    let third = run_with_env(&p, "RUNE_TEST_SCRIPT_VAR", Some("1"));
    assert!(
        third.contains("seen=1/first") && third.contains("0 compiled"),
        "{third}"
    );
    // The declared file outside the package changed.
    std::fs::write(&header, "second").unwrap();
    let fourth = run_with_env(&p, "RUNE_TEST_SCRIPT_VAR", Some("1"));
    assert!(
        fourth.contains("seen=1/second"),
        "stale build-script result: {fourth}"
    );
}

// ---------------------------------------------------------------------------
// runner, rustdoc identity
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn target_runners_wrap_cross_runs() {
    use std::os::unix::fs::PermissionsExt;
    let p = lib_project();
    let host = host_triple();
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("runner.log");
    let runner = scratch.path().join("runner.sh");
    std::fs::write(
        &runner,
        format!(
            "#!/bin/sh\necho \"ran: $(basename \"$1\")\" >> {}\nexec \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}\n[[bin]]\nname = \"demo-bin\"\npath = \"src/main.rs\"\n"),
    );
    p.write(
        "src/main.rs",
        "fn main() { println!(\"hello from the binary\"); }\n",
    );
    p.write(
        ".cargo/config.toml",
        &format!(
            "[target.{host}]\nrunner = {:?}\n",
            runner.display().to_string()
        ),
    );

    // Native runs don't use the runner…
    let out = p.ok(&["run"]);
    assert!(
        out.contains("hello from the binary") && !log.exists(),
        "{out}"
    );
    // …cross runs (even to the same triple) do.
    let out = p.ok(&["run", "--target", &host]);
    assert!(out.contains("hello from the binary"), "{out}");
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("ran: demo-bin"),
        "the runner was not used"
    );
}

#[test]
fn documentation_is_regenerated_only_when_its_identity_changes() {
    let p = lib_project();
    p.write(
        "src/lib.rs",
        "/// Adds two numbers.\npub fn add(a: u32, b: u32) -> u32 { a + b }\n",
    );
    let first = p.ok(&["doc", "--no-deps"]);
    assert!(
        first.contains("documenting demo") && !first.contains("up to date"),
        "{first}"
    );
    assert!(p.path().join(".rune/target/doc/demo/index.html").is_file());

    let second = p.ok(&["doc", "--no-deps"]);
    assert!(second.contains("up to date"), "{second}");

    p.write(
        "src/lib.rs",
        "/// Adds two numbers, differently.\npub fn add(a: u32, b: u32) -> u32 { b + a }\n",
    );
    let third = p.ok(&["doc", "--no-deps"]);
    assert!(
        third.contains("documenting demo") && !third.contains("up to date"),
        "{third}"
    );
}

// ---------------------------------------------------------------------------
// The manifest is not hashed; its semantics are
// ---------------------------------------------------------------------------

#[test]
fn editing_an_unrelated_manifest_section_does_not_rebuild_the_package() {
    let p = lib_project();
    p.ok(&["build"]);
    // A comment, and a development dependency the library never uses: neither
    // changes what compiling the library produces.
    p.write(
        "Cargo.toml",
        &format!("# an edit that changes nothing\n{LIB_MANIFEST}\n[dev-dependencies]\nhelper = {{ path = \"helper\" }}\n"),
    );
    p.write(
        "helper/Cargo.toml",
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("helper/src/lib.rs", "");
    let again = p.ok(&["build"]);
    assert!(
        again.contains("0 compiled"),
        "the library was rebuilt for a manifest-only edit: {again}"
    );
}

#[test]
fn manifest_edits_that_change_the_compilation_still_rebuild() {
    let p = lib_project();
    p.ok(&["build"]);
    // A feature the crate's own code tests for, turned on by default.
    p.write(
        "src/lib.rs",
        "#[cfg(feature = \"extra\")]\npub fn extra() {}\npub fn add(a: u32, b: u32) -> u32 { a + b }\n",
    );
    p.ok(&["build"]);
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}[features]\ndefault = [\"extra\"]\nextra = []\n"),
    );
    let out = p.ok(&["build"]);
    assert!(
        out.contains("1 compiled"),
        "a changed feature set must rebuild: {out}"
    );
}

#[test]
fn a_crate_that_includes_its_own_manifest_notices_when_it_changes() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\ndescription = \"first\"\n",
    );
    p.write(
        "src/main.rs",
        "fn main() { println!(\"manifest says: {}\", include_str!(\"../Cargo.toml\").lines().find(|l| l.starts_with(\"description\")).unwrap()); }\n",
    );
    assert!(p.ok(&["run"]).contains("description = \"first\""));
    // The manifest is not part of the package hash, but the compiler *read*
    // it, so it became a declared input of this unit.
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\ndescription = \"second\"\n",
    );
    let out = p.ok(&["run"]);
    assert!(
        out.contains("description = \"second\""),
        "stale binary: {out}"
    );
}

#[test]
fn a_build_script_may_not_modify_an_immutable_package_source() {
    // A git dependency (immutable, shared by every project) whose build
    // script scribbles in its own directory instead of `OUT_DIR`.
    let repo = upstream("v1");
    let d = repo.path();
    std::fs::write(
        d.join("Cargo.toml"),
        "[package]\nname = \"gitdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n",
    )
    .unwrap();
    std::fs::write(
        d.join("build.rs"),
        "fn main() { std::fs::write(\"stray-output.txt\", \"oops\").unwrap(); }\n",
    )
    .unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "--quiet", "-m", "misbehaving script"]);
    let p = git_app(&format!("{{ git = {:?} }}", url(d)));
    let out = p.rune(&["build"]);
    assert!(!out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(
        t.contains("modified its own source directory") && t.contains("OUT_DIR"),
        "{t}"
    );
}

/// A chain of path crates (each with a build script) below one binary: enough
/// units, and enough shared files, for concurrent builds to collide.
fn chain_project(len: usize) -> Project {
    let p = Project::new();
    let members: Vec<String> = (0..len).map(|i| format!("\"c{i}\"")).collect();
    p.write(
        "Cargo.toml",
        &format!(
            "[workspace]\nresolver = \"2\"\nmembers = [\"app\", {}]\n",
            members.join(", ")
        ),
    );
    for i in 0..len {
        let dep = if i == 0 {
            String::new()
        } else {
            format!("c{} = {{ path = \"../c{}\" }}\n", i - 1, i - 1)
        };
        p.write(
            &format!("c{i}/Cargo.toml"),
            &format!(
                "[package]\nname = \"c{i}\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\n[dependencies]\n{dep}"
            ),
        );
        p.write(
            &format!("c{i}/build.rs"),
            "fn main() {\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(format!(\"{out}/gen.rs\"), \"pub const GEN: u32 = 7;\").unwrap();\n}\n",
        );
        let call = if i == 0 {
            "0".to_string()
        } else {
            format!("c{}::value()", i - 1)
        };
        p.write(
            &format!("c{i}/src/lib.rs"),
            &format!(
                "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\npub fn value() -> u32 {{ {call} + GEN }}\n"
            ),
        );
    }
    p.write(
        "app/Cargo.toml",
        &format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nc{} = {{ path = \"../c{}\" }}\n",
            len - 1,
            len - 1
        ),
    );
    p.write(
        "app/src/main.rs",
        &format!(
            "fn main() {{ println!(\"value={{}}\", c{}::value()); }}\n",
            len - 1
        ),
    );
    p
}

#[test]
fn concurrent_builds_of_one_project_all_succeed_and_share_the_work() {
    let p = chain_project(8);
    // Six processes start at once on an empty target directory and CAS: what an
    // editor's background check and a terminal build do to each other.
    let outputs: Vec<Output> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..6).map(|_| s.spawn(|| p.rune(&["build"]))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (i, out) in outputs.iter().enumerate() {
        assert!(
            out.status.success(),
            "concurrent build #{i} failed:\n{}",
            text(out)
        );
    }
    // Nothing was left half-written: the result runs, and is fully cached.
    let ran = p.ok(&["run", "-p", "app"]);
    assert!(ran.contains("value=56"), "{ran}");
    assert!(p.ok(&["build"]).contains("0 compiled"));
}

#[test]
fn concurrent_check_and_build_do_not_break_each_other() {
    let p = chain_project(6);
    let outputs: Vec<(&str, Output)> = std::thread::scope(|s| {
        let p = &p;
        let handles: Vec<_> = ["check", "build", "check", "build"]
            .into_iter()
            .map(|cmd| (cmd, s.spawn(move || p.rune(&[cmd]))))
            .collect();
        handles
            .into_iter()
            .map(|(cmd, h)| (cmd, h.join().unwrap()))
            .collect()
    });
    for (cmd, out) in &outputs {
        assert!(
            out.status.success(),
            "concurrent `{cmd}` failed:\n{}",
            text(out)
        );
    }
    assert!(p.ok(&["run", "-p", "app"]).contains("value=42"));
}

#[cfg(unix)]
#[test]
fn deps_only_builds_dependencies_and_leaves_the_workspace_alone() {
    let (_, sum) = make_crate("foo", "1.0.0", FOO_LIB);
    let p = registry_app("\"1\"");
    p.write(
        "vendor/foo-1.0.0/Cargo.toml",
        "[package]\nname = \"foo\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
    );
    p.write("vendor/foo-1.0.0/src/lib.rs", FOO_LIB);
    p.write(
        "vendor/foo-1.0.0/.cargo-checksum.json",
        &format!("{{\"package\":\"{sum}\",\"files\":{{}}}}"),
    );
    p.write(
        ".cargo/config.toml",
        "[source.crates-io]\nreplace-with = \"vendored\"\n[source.vendored]\ndirectory = \"vendor\"\n",
    );

    let out = p.ok(&["build", "--deps-only", "--offline"]);
    assert!(out.contains("1 compiled"), "only `foo` builds: {out}");
    // The application itself was not built…
    assert!(!p.path().join(".rune/target/debug/app").exists());

    // …so a normal build has just the application left to do, and the
    // dependency comes from what `--deps-only` produced (a container layer or
    // CI cache keyed on Cargo.lock would hold exactly this).
    let out = p.ok(&["build", "--offline"]);
    assert!(
        out.contains("1 compiled") && out.contains("1 cached"),
        "{out}"
    );
    assert!(p.ok(&["run", "--offline"]).contains("foo says hello"));
}

/// stdout of a `--message-format json` run, parsed line by line.
fn json_lines(out: &Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l:?} ({e})")))
        .collect()
}

#[test]
fn test_no_run_json_names_every_test_executable_like_cargo() {
    // What `cargo nextest` (and IDEs) run to find the test binaries.
    let p = lib_project();
    p.write("tests/it.rs", "#[test] fn it() {}\n");
    let out = p.rune(&[
        "test",
        "--no-run",
        "--message-format",
        "json-render-diagnostics",
    ]);
    assert!(out.status.success(), "{}", text(&out));
    let tests: Vec<(String, String)> = json_lines(&out)
        .iter()
        .filter(|l| l["reason"] == "compiler-artifact" && l["profile"]["test"] == true)
        .map(|l| {
            (
                l["target"]["kind"][0].as_str().unwrap().to_string(),
                l["executable"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let kinds: Vec<&str> = tests.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        kinds.contains(&"lib") && kinds.contains(&"test"),
        "{tests:?}"
    );
    for (kind, exe) in &tests {
        assert!(!exe.is_empty(), "{kind} test has no executable: {tests:?}");
        let run = Command::new(exe).output().unwrap();
        assert!(run.status.success() && String::from_utf8_lossy(&run.stdout).contains("1 passed"));
    }
}

#[test]
fn message_format_json_reports_diagnostics_artifacts_and_the_end_of_the_build() {
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    p.write("src/lib.rs", "pub fn f() { let unused = 1; }\n");

    for (round, expect_fresh) in [("first", false), ("second (from the cache)", true)] {
        let out = p.rune(&["check", "--message-format", "json"]);
        assert!(out.status.success(), "{round}: {}", text(&out));
        let lines = json_lines(&out);
        let reasons: Vec<&str> = lines
            .iter()
            .map(|l| l["reason"].as_str().unwrap())
            .collect();
        assert_eq!(
            reasons.last(),
            Some(&"build-finished"),
            "{round}: {reasons:?}"
        );
        assert_eq!(lines.last().unwrap()["success"], true);

        // The warning is replayed on a cache hit, in rustc's own structure.
        let msg = lines
            .iter()
            .find(|l| l["reason"] == "compiler-message")
            .unwrap_or_else(|| panic!("{round}: no compiler-message in {reasons:?}"));
        assert_eq!(msg["message"]["level"], "warning");
        assert!(
            msg["message"]["message"]
                .as_str()
                .unwrap()
                .contains("unused variable")
        );
        assert!(
            msg["message"]["spans"][0]["file_name"]
                .as_str()
                .unwrap()
                .ends_with("lib.rs")
        );
        assert!(msg["package_id"].as_str().unwrap().contains("demo@0.1.0"));
        assert_eq!(msg["target"]["kind"][0], "lib");
        assert!(
            msg["target"]["src_path"]
                .as_str()
                .unwrap()
                .ends_with("src/lib.rs")
        );

        let artifact = lines
            .iter()
            .find(|l| l["reason"] == "compiler-artifact")
            .unwrap();
        assert_eq!(artifact["fresh"], expect_fresh, "{round}");
        assert!(!artifact["filenames"].as_array().unwrap().is_empty());
    }
}

#[test]
fn message_format_json_reports_errors_and_a_failed_build() {
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    p.write("src/lib.rs", "pub fn f() -> u32 { \"not a number\" }\n");
    let out = p.rune(&["build", "--message-format=json"]);
    assert!(!out.status.success());
    let lines = json_lines(&out);
    let err = lines
        .iter()
        .find(|l| l["reason"] == "compiler-message" && l["message"]["level"] == "error")
        .unwrap_or_else(|| panic!("no error message: {lines:?}"));
    assert_eq!(err["message"]["code"]["code"], "E0308");
    assert_eq!(lines.last().unwrap()["reason"], "build-finished");
    assert_eq!(lines.last().unwrap()["success"], false);
}

#[test]
fn an_unknown_message_format_is_rejected() {
    let p = lib_project();
    let out = p.rune(&["build", "--message-format", "xml"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("unknown --message-format"));
}

#[test]
fn watch_runs_again_when_a_file_changes_and_shows_the_new_result() {
    use std::io::Read;
    use std::process::Stdio;
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    p.write("src/lib.rs", "pub fn f() {}\n");

    let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["watch", "--interval-ms", "50", "--max-runs", "2"])
        .current_dir(p.path())
        .env("RUNE_HOME", p.home())
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Wait for the first run to finish, then edit: the second run must see a
    // new warning that the first did not have.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    p.write(
        "src/lib.rs",
        "pub fn f() { let unused_in_second_run = 1; }\n",
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "watch did not run a second time"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let (mut out, mut err) = (String::new(), String::new());
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let all = format!("{out}{err}");
    assert_eq!(all.matches("rune check").count(), 2, "{all}");
    assert!(all.contains("unused_in_second_run"), "{all}");
    assert!(all.contains("waiting for changes"), "{all}");
}

#[test]
fn a_failing_build_script_that_needs_a_system_package_says_which() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"needs-ssl\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n",
    );
    p.write(
        "build.rs",
        "fn main() {\n    eprintln!(\"Could not find directory of OpenSSL installation\");\n    std::process::exit(1);\n}\n",
    );
    p.write("src/lib.rs", "");
    let out = p.rune(&["build"]);
    assert!(!out.status.success());
    let t = text(&out);
    assert!(
        t.contains("Could not find directory of OpenSSL installation"),
        "{t}"
    );
    assert!(
        t.contains("hint: this looks like the OpenSSL development files is missing"),
        "{t}"
    );
    assert!(t.contains("OPENSSL_DIR"), "{t}");
}

fn git_in(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git is required for this test");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn affected_lists_the_changed_packages_and_everything_that_depends_on_them() {
    // app -> c2 -> c1 -> c0
    let p = chain_project(3);
    git_in(p.path(), &["init", "-q", "-b", "main"]);
    git_in(p.path(), &["add", "-A"]);
    git_in(p.path(), &["commit", "-q", "-m", "base"]);

    let affected = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["affected"];
        args.extend_from_slice(extra);
        p.ok(&args)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()
    };

    assert!(affected(&[]).is_empty(), "a clean tree affects nothing");

    // An uncommitted edit in the middle of the chain.
    p.write(
        "c1/src/lib.rs",
        "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\npub fn value() -> u32 { 1 + GEN }\n",
    );
    assert_eq!(affected(&[]), ["app", "c1", "c2"]);

    // A new, untracked file counts.
    p.write("c0/src/extra.rs", "");
    assert_eq!(affected(&[]), ["app", "c0", "c1", "c2"]);
    assert_eq!(
        affected(&["--format", "args"]),
        ["-p app -p c0 -p c1 -p c2"]
    );
    let json: Vec<String> = serde_json::from_str(&p.ok(&["affected", "--format", "json"])).unwrap();
    assert_eq!(json, ["app", "c0", "c1", "c2"]);

    // Committed, and compared with the parent: the same set; with HEAD: nothing.
    git_in(p.path(), &["add", "-A"]);
    git_in(p.path(), &["commit", "-q", "-m", "change"]);
    assert!(affected(&[]).is_empty());
    assert_eq!(affected(&["--since", "HEAD~1"]), ["app", "c0", "c1", "c2"]);
}

#[test]
fn scripts_run_with_clean_output_arguments_exit_codes_and_path_dependencies() {
    let p = Project::new();
    // A dependency the script names by path, so the test needs no network.
    p.write(
        "dep/Cargo.toml",
        "[package]\nname = \"greeter\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "dep/src/lib.rs",
        "pub fn hello(who: &str) -> String { format!(\"hello, {who}\") }\n",
    );
    p.write(
        "hello.rs",
        &format!(
            "#!/usr/bin/env -S rune script\n---cargo\n[dependencies]\ngreeter = {{ path = {:?} }}\n---\n\
             fn main() {{\n    let args: Vec<String> = std::env::args().skip(1).collect();\n    \
             println!(\"{{}}\", greeter::hello(&args.join(\"+\")));\n    \
             if args.iter().any(|a| a == \"fail\") {{ std::process::exit(7); }}\n}}\n",
            p.path().join("dep")
        ),
    );

    let run = |args: &[&str]| {
        let mut all = vec!["script", "hello.rs"];
        all.extend_from_slice(args);
        p.rune(&all)
    };
    let out = run(&["a", "-b", "--c"]);
    assert!(out.status.success(), "{}", text(&out));
    // Nothing of Rune's own on stdout: it is the script's output, verbatim.
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello, a+-b+--c\n");

    // Second run: same result, and nothing is rebuilt.
    assert_eq!(
        String::from_utf8_lossy(&run(&["again"]).stdout),
        "hello, again\n"
    );

    // The exit code of the script is Rune's.
    assert_eq!(run(&["fail"]).status.code(), Some(7));

    // Editing the script rebuilds it.
    p.write(
        "hello.rs",
        &format!(
            "---cargo\n[dependencies]\ngreeter = {{ path = {:?} }}\n---\n\
             fn main() {{ println!(\"{{}}\", greeter::hello(\"edited\")); }}\n",
            p.path().join("dep")
        ),
    );
    assert_eq!(String::from_utf8_lossy(&run(&[]).stdout), "hello, edited\n");
}

#[test]
fn a_script_with_a_compile_error_reports_it_with_the_right_line_number() {
    let p = Project::new();
    p.write(
        "bad.rs",
        "---cargo\n[dependencies]\n---\nfn main() {\n    let x: u32 = \"no\";\n}\n",
    );
    let out = p.rune(&["script", "bad.rs"]);
    assert!(!out.status.success());
    let t = text(&out);
    assert!(t.contains("mismatched types"), "{t}");
    assert!(
        t.contains("src/main.rs:5"),
        "the error is on line 5 of the file: {t}"
    );
}

#[test]
fn cached_tests_are_skipped_until_something_that_matters_changes() {
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    // The marker lives outside the package: each *execution* of the test binary
    // appends a line, so its line count says how often it really ran.
    let marker = p.home().join("marker.txt");
    let body = |pass: bool, extra: &str| {
        format!(
            "pub fn f() -> u32 {{ 1 {extra} }}\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn t() {{\n        use std::io::Write;\n        let path = std::env::var(\"RUNE_MARKER\").unwrap();\n        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();\n        writeln!(f, \"ran\").unwrap();\n        assert!({pass});\n    }}\n}}\n"
        )
    };
    let runs = || std::fs::read_to_string(&marker).map_or(0, |t| t.lines().count());
    let test = |args: &[&str]| -> (bool, String) {
        let mut all = vec!["test", "--no-doc"];
        all.extend_from_slice(args);
        let out = Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(&all)
            .current_dir(p.path())
            .env("RUNE_HOME", p.home())
            .env("RUNE_MARKER", &marker)
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        (out.status.success(), text(&out))
    };

    p.write("src/lib.rs", &body(true, ""));
    // Without the flag nothing is ever skipped.
    assert!(test(&[]).0);
    assert!(test(&[]).0);
    assert_eq!(runs(), 2);

    // With it: the first run executes and remembers…
    let (ok, t) = test(&["--cached"]);
    assert!(ok && !t.contains("cached"), "{t}");
    assert_eq!(runs(), 3);
    // …the next does not run the binary at all…
    let (ok, t) = test(&["--cached"]);
    assert!(ok && t.contains("cached"), "{t}");
    assert!(
        t.contains("1 passed"),
        "the remembered counts are shown: {t}"
    );
    assert_eq!(runs(), 3, "the binary was not executed");
    // …unless you insist,
    assert!(test(&["--cached=false"]).0);
    assert_eq!(runs(), 4);
    // …or the arguments differ,
    assert!(test(&["--cached", "--", "--test-threads=1"]).0);
    assert_eq!(runs(), 5);
    // …or the code changed.
    p.write("src/lib.rs", &body(true, "+ 1"));
    let (ok, t) = test(&["--cached"]);
    assert!(ok && !t.contains("cached"), "{t}");
    assert_eq!(runs(), 6);
    assert!(test(&["--cached"]).1.contains("cached"));
    assert_eq!(runs(), 6);

    // A fixture file inside the package counts as an input.
    p.write("fixtures/data.txt", "one");
    assert!(!test(&["--cached"]).1.contains("cached"));
    assert_eq!(runs(), 7);

    // Failures are never remembered: a failing test runs every time.
    p.write("src/lib.rs", &body(false, ""));
    for expected in [8, 9] {
        let (ok, t) = test(&["--cached"]);
        assert!(!ok && !t.contains("cached"), "{t}");
        assert_eq!(runs(), expected);
    }
}

fn rune_with_env(p: &Project, args: &[&str], env: &[(&str, &std::ffi::OsStr)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rune"));
    cmd.args(args)
        .current_dir(p.path())
        .env("RUNE_HOME", p.home())
        .env("NO_COLOR", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

#[test]
fn a_shared_store_lets_another_machine_skip_what_was_already_built() {
    let shared = tempfile::tempdir().unwrap();
    let remote: &std::ffi::OsStr = shared.path().as_os_str();

    // "CI": builds and publishes. Eight units, several with build scripts.
    let ci = chain_project(6);
    let out = rune_with_env(&ci, &["build"], &[("RUNE_CAS_REMOTE", remote)]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("compiled") && !text(&out).contains("0 compiled"),
        "{}",
        text(&out)
    );
    assert!(
        shared.path().join("objects").is_dir(),
        "CI published to the shared store"
    );

    // A developer: another checkout, another path, empty local store, read-only remote.
    let dev = chain_project(6);
    assert_ne!(ci.path(), dev.path());
    let out = rune_with_env(
        &dev,
        &["build"],
        &[
            ("RUNE_CAS_REMOTE", remote),
            ("RUNE_CAS_REMOTE_WRITE", "false".as_ref()),
        ],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        t.contains("0 compiled"),
        "everything came from the shared store: {t}"
    );
    // …and what was restored actually works, build-script output included.
    let ran = rune_with_env(&dev, &["run", "-p", "app"], &[]);
    assert!(text(&ran).contains("value=42"), "{}", text(&ran));

    // Without the remote the same checkout would have had to compile.
    let alone = chain_project(6);
    let t = text(&rune_with_env(&alone, &["build"], &[]));
    assert!(!t.contains("0 compiled"), "{t}");
}

// ---------------------------------------------------------------------------
// Rune.toml as a manifest
// ---------------------------------------------------------------------------

const RUNE_MANIFEST: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

/// A project with a `Rune.toml` manifest and no `Cargo.toml` at all.
fn rune_only_project() -> Project {
    let p = Project::new();
    p.write("Rune.toml", RUNE_MANIFEST);
    p.write(
        "src/main.rs",
        "fn main() { println!(\"hello from rune.toml\"); }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn it_runs() { assert_eq!(2 + 2, 4); }\n}\n",
    );
    p
}

#[test]
fn a_project_with_only_a_rune_toml_builds_runs_and_tests() {
    let p = rune_only_project();
    assert!(!p.path().join("Cargo.toml").exists());

    assert!(p.ok(&["run"]).contains("hello from rune.toml"));
    assert!(
        p.ok(&["build"]).contains("0 compiled"),
        "cached on the second build"
    );
    let t = p.ok(&["test"]);
    assert!(t.contains("1 passed"), "{t}");
    p.ok(&["check"]);
    // Rune never invents a Cargo.toml for a project that does not have one.
    assert!(!p.path().join("Cargo.toml").exists());
    assert!(
        p.path().join("Cargo.lock").exists(),
        "the lockfile is still Cargo's format"
    );
}

#[test]
fn the_manifest_and_rune_settings_can_share_one_rune_toml() {
    let p = Project::new();
    p.write(
        "Rune.toml",
        &format!(
            "{RUNE_MANIFEST}\n[dependencies]\n\n[build]\njobs = 3\n\n[test]\ndoctests = false\n"
        ),
    );
    p.write("src/lib.rs", "pub fn f() {}\n");
    // The settings in the manifest file are the project's settings…
    let listing = p.ok(&["config", "list", "--show-origin"]);
    assert!(
        listing.contains("build.jobs") && listing.contains("Rune.toml"),
        "{listing}"
    );
    assert_eq!(p.ok(&["config", "get", "build.jobs"]).trim(), "3");
    // …`rune config set` writes into the same file and keeps the manifest.
    p.ok(&["config", "set", "build.jobs", "5"]);
    let text = p.read("Rune.toml");
    assert!(
        text.contains("name = \"demo\"") && text.contains("jobs = 5"),
        "{text}"
    );
    p.ok(&["build"]);
}

#[test]
fn editing_settings_in_the_manifest_file_does_not_rebuild_but_editing_the_package_does() {
    let p = rune_only_project();
    assert!(p.ok(&["build"]).contains("1 compiled"));
    // A setting is not part of any unit's identity…
    p.write(
        "Rune.toml",
        &format!("{RUNE_MANIFEST}\n[build]\njobs = 2\n"),
    );
    assert!(p.ok(&["build"]).contains("0 compiled"));
    // …a manifest field that matters is.
    p.write(
        "Rune.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2018\"\n\n[build]\njobs = 2\n",
    );
    assert!(p.ok(&["build"]).contains("1 compiled"));
}

#[test]
fn a_workspace_can_mix_cargo_and_rune_manifests_and_path_dependencies_can_be_rune_only() {
    let p = Project::new();
    p.write(
        "Rune.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"lib-cargo\", \"lib-rune\"]\n",
    );
    p.write(
        "app/Rune.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nlib-cargo = { path = \"../lib-cargo\" }\nlib-rune = { path = \"../lib-rune\" }\nshared = { path = \"../shared\" }\n",
    );
    p.write(
        "app/src/main.rs",
        "fn main() { println!(\"{} {} {}\", lib_cargo::name(), lib_rune::name(), shared::name()); }\n",
    );
    p.write(
        "lib-cargo/Cargo.toml",
        "[package]\nname = \"lib-cargo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "lib-cargo/src/lib.rs",
        "pub fn name() -> &'static str { \"cargo\" }\n",
    );
    p.write(
        "lib-rune/Rune.toml",
        "[package]\nname = \"lib-rune\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "lib-rune/src/lib.rs",
        "pub fn name() -> &'static str { \"rune\" }\n",
    );
    // Not a workspace member: a plain path dependency next door.
    p.write(
        "shared/Rune.toml",
        "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "shared/src/lib.rs",
        "pub fn name() -> &'static str { \"shared\" }\n",
    );

    assert!(p.ok(&["run", "-p", "app"]).contains("cargo rune shared"));
    let meta: serde_json::Value = serde_json::from_str(&p.ok(&["metadata"])).unwrap();
    let manifests: Vec<String> = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|pkg| pkg["manifest_path"].as_str())
        .map(|m| {
            Path::new(m)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        manifests.contains(&"Rune.toml".to_string())
            && manifests.contains(&"Cargo.toml".to_string()),
        "{manifests:?}"
    );
    // The workspace root is found from a member directory, too.
    let out = p.rune_in(&p.path().join("lib-rune"), &["build"]);
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn workspace_inheritance_works_from_a_rune_toml_root() {
    let p = Project::new();
    p.write(
        "Rune.toml",
        "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nversion = \"2.5.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "a/Rune.toml",
        "[package]\nname = \"a\"\nversion.workspace = true\nedition.workspace = true\n",
    );
    p.write(
        "a/src/main.rs",
        "fn main() { println!(\"v{}\", env!(\"CARGO_PKG_VERSION\")); }\n",
    );
    assert!(p.ok(&["run"]).contains("v2.5.0"));
}

#[test]
fn when_both_files_are_manifests_rune_toml_wins_and_a_settings_only_one_does_not() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"from-cargo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "Rune.toml",
        "[package]\nname = \"from-rune\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "src/main.rs",
        "fn main() { println!(\"{}\", env!(\"CARGO_PKG_NAME\")); }\n",
    );
    assert!(p.ok(&["run"]).contains("from-rune"));

    // Only settings in Rune.toml: Cargo.toml stays the manifest (how it always was).
    p.write("Rune.toml", "[build]\njobs = 2\n");
    assert!(p.ok(&["run"]).contains("from-cargo"));
}

#[test]
fn locate_project_and_json_messages_name_the_real_manifest() {
    let p = rune_only_project();
    let located: serde_json::Value = serde_json::from_str(&p.ok(&["locate-project"])).unwrap();
    assert!(
        located["root"].as_str().unwrap().ends_with("Rune.toml"),
        "{located}"
    );

    let out = p.rune(&["check", "--message-format", "json"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let artifact = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["reason"] == "compiler-artifact")
        .expect("an artifact line");
    assert!(
        artifact["manifest_path"]
            .as_str()
            .unwrap()
            .ends_with("Rune.toml"),
        "{artifact}"
    );
}

#[test]
fn add_and_remove_edit_the_rune_toml_and_leave_its_settings_alone() {
    let p = Project::new();
    p.write(
        "dep/Rune.toml",
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("dep/src/lib.rs", "pub fn help() -> u32 { 42 }\n");
    p.write(
        "app/Rune.toml",
        &format!(
            "{}\n[dependencies]\n\n# my settings\n[build]\njobs = 2\n",
            RUNE_MANIFEST.replace("demo", "app")
        ),
    );
    p.write(
        "app/src/main.rs",
        "fn main() { println!(\"{}\", helper::help()); }\n",
    );

    let app = p.path().join("app");
    let dep = p.path().join("dep");
    let added = p.rune_in(&app, &["add", "helper", "--path", dep.to_str().unwrap()]);
    assert!(added.status.success(), "{}", text(&added));
    let manifest = std::fs::read_to_string(app.join("Rune.toml")).unwrap();
    assert!(
        manifest.contains("helper")
            && manifest.contains("# my settings")
            && manifest.contains("jobs = 2"),
        "{manifest}"
    );
    assert!(!app.join("Cargo.toml").exists());
    assert!(text(&p.rune_in(&app, &["run"])).contains("42"));

    let removed = p.rune_in(&app, &["remove", "helper"]);
    assert!(removed.status.success(), "{}", text(&removed));
    let manifest = std::fs::read_to_string(app.join("Rune.toml")).unwrap();
    assert!(
        !manifest.contains("helper") && manifest.contains("jobs = 2"),
        "{manifest}"
    );
}

#[test]
fn new_and_init_can_create_a_rune_toml_project() {
    let p = Project::new();
    let out = p.rune(&["new", "fresh", "--manifest", "rune", "--no-gitignore"]);
    assert!(out.status.success(), "{}", text(&out));
    let dir = p.path().join("fresh");
    assert!(dir.join("Rune.toml").is_file() && !dir.join("Cargo.toml").exists());
    let run = p.rune_in(&dir, &["run"]);
    assert!(text(&run).contains("Hello, world!"), "{}", text(&run));

    // `init` in a directory that already has a settings-only Rune.toml keeps the settings.
    let existing = p.path().join("existing");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("Rune.toml"), "[build]\njobs = 7\n").unwrap();
    let out = p.rune_in(
        &existing,
        &[
            "init",
            "--manifest",
            "rune",
            "--name",
            "existing",
            "--no-gitignore",
        ],
    );
    assert!(out.status.success(), "{}", text(&out));
    let text_after = std::fs::read_to_string(existing.join("Rune.toml")).unwrap();
    assert!(
        text_after.contains("name = \"existing\"") && text_after.contains("jobs = 7"),
        "{text_after}"
    );
    assert!(p.rune_in(&existing, &["build"]).status.success());

    // And it refuses to overwrite a project that already has a manifest.
    let again = p.rune_in(
        &existing,
        &["init", "--manifest", "cargo", "--no-gitignore"],
    );
    assert!(!again.status.success());
    assert!(text(&again).contains("already exists"), "{}", text(&again));
}

#[test]
fn a_git_dependency_can_be_a_rune_toml_only_repository() {
    let repo = tempfile::tempdir().unwrap();
    let d = repo.path();
    git(d, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(
        d.join("Rune.toml"),
        "[package]\nname = \"gitdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        d.join("src/lib.rs"),
        "pub fn version() -> &'static str { \"rune-only git dependency\" }\n",
    )
    .unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "--quiet", "-m", "first"]);

    let p = git_app(&format!("{{ git = {:?} }}", url(d)));
    let out = p.ok(&["run"]);
    assert!(
        out.contains("gitdep says rune-only git dependency"),
        "{out}"
    );
}

#[test]
fn an_unparsable_rune_toml_manifest_is_reported_not_skipped() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"fine\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("Rune.toml", "[package]\nname = \nversion = \"0.1.0\"\n");
    p.write("src/main.rs", "fn main() {}\n");
    let out = p.rune(&["build"]);
    assert!(
        !out.status.success(),
        "must not silently fall back to Cargo.toml"
    );
    assert!(text(&out).contains("Rune.toml"), "{}", text(&out));
}

#[test]
fn doctor_warns_when_rune_toml_shadows_a_cargo_toml() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "Rune.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("src/lib.rs", "");
    assert!(
        p.ok(&["doctor"])
            .contains("both `Rune.toml` and `Cargo.toml` define this project")
    );
    // A settings-only Rune.toml next to a Cargo.toml is the normal case: no warning.
    p.write("Rune.toml", "[build]\njobs = 2\n");
    assert!(!p.ok(&["doctor"]).contains("both `Rune.toml`"));
}

// ---------------------------------------------------------------------------
// Build scripts as their own step
// ---------------------------------------------------------------------------

/// A build script that appends `<name> <event> <nanoseconds>` to the file
/// named by `RUNE_T_STAMPS`, optionally sleeping in between.
fn stamping_script(name: &str, sleep_ms: u64, extra: &str) -> String {
    format!(
        r#"use std::io::Write;
fn stamp(event: &str) {{
    let path = std::env::var("RUNE_T_STAMPS").unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(f, "{name} {{event}} {{now}}").unwrap();
}}
fn main() {{
    stamp("start");
    std::thread::sleep(std::time::Duration::from_millis({sleep_ms}));
    stamp("end");
    println!("cargo::rerun-if-changed=build.rs");
    {extra}
}}
"#
    )
}

fn stamps(path: &Path) -> std::collections::HashMap<String, u128> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| {
            let mut it = l.split_whitespace();
            let (who, what, at) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
            (format!("{who} {what}"), at.parse().unwrap())
        })
        .collect()
}

#[test]
fn a_build_script_runs_while_its_packages_dependencies_are_still_compiling() {
    // `app`'s script needs nothing from `slow` (an ordinary dependency), so it
    // must not wait for `slow` — whose own script takes 1.5 s — to finish.
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\nslow = { path = \"slow\" }\n",
    );
    p.write("build.rs", &stamping_script("app", 0, ""));
    p.write("src/lib.rs", "pub fn v() -> u32 { slow::v() + 1 }\n");
    p.write(
        "slow/Cargo.toml",
        "[package]\nname = \"slow\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("slow/build.rs", &stamping_script("slow", 1500, ""));
    p.write("slow/src/lib.rs", "pub fn v() -> u32 { 1 }\n");
    let log = p.home().join("stamps.txt");
    let out = Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["build", "-j", "4"])
        .current_dir(p.path())
        .env("RUNE_HOME", p.home())
        .env("RUNE_T_STAMPS", &log)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let s = stamps(&log);
    assert!(
        s["app start"] < s["slow end"],
        "app's build script waited for its dependency: {s:?}"
    );
}

#[test]
fn check_only_type_checks_the_ordinary_dependencies_of_a_crate_with_a_build_script() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\nplain = { path = \"plain\" }\n\
         [build-dependencies]\nhelper = { path = \"helper\" }\n",
    );
    p.write(
        "build.rs",
        "fn main() { assert_eq!(helper::h(), 7); println!(\"cargo::rerun-if-changed=build.rs\"); }\n",
    );
    p.write("src/lib.rs", "pub fn v() -> u32 { plain::v() }\n");
    for (name, body) in [
        ("plain", "pub fn v() -> u32 { 1 }\n"),
        ("helper", "pub fn h() -> u32 { 7 }\n"),
    ] {
        p.write(
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        );
        p.write(&format!("{name}/src/lib.rs"), body);
    }
    p.ok(&["check"]);
    let deps: Vec<String> = std::fs::read_dir(p.path().join(".rune/target/debug/deps"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let has = |prefix: &str, ext: &str| {
        deps.iter()
            .any(|f| f.starts_with(prefix) && f.ends_with(ext))
    };
    assert!(has("libplain-", ".rmeta"), "{deps:?}");
    assert!(
        !has("libplain-", ".rlib"),
        "an ordinary dependency was compiled to machine code by `check`: {deps:?}"
    );
    // The build script links `helper`, so that one is really built.
    assert!(has("libhelper-", ".rlib"), "{deps:?}");
}

#[test]
fn a_dependency_that_learns_an_outside_input_late_still_rekeys_its_dependents() {
    // `base`'s script is slow and declares an environment variable; `app`'s
    // script has long finished by then. `app` must still be stored under a
    // key that includes `base`'s input: an identical second build is fully
    // cached, and changing the variable rebuilds both.
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\nbase = { path = \"base\" }\n",
    );
    p.write("build.rs", &stamping_script("app", 0, ""));
    p.write(
        "src/main.rs",
        "fn main() { println!(\"mode={}\", base::mode()); }\n",
    );
    p.write(
        "base/Cargo.toml",
        "[package]\nname = \"base\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "base/build.rs",
        &stamping_script(
            "base",
            800,
            r#"println!("cargo::rerun-if-env-changed=RUNE_T_MODE");
    println!("cargo::rustc-env=MODE={}", std::env::var("RUNE_T_MODE").unwrap_or_default());"#,
        ),
    );
    p.write(
        "base/src/lib.rs",
        "pub fn mode() -> &'static str { env!(\"MODE\") }\n",
    );
    let log = p.home().join("stamps.txt");
    let run = |mode: &str, args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(args)
            .current_dir(p.path())
            .env("RUNE_HOME", p.home())
            .env("RUNE_T_STAMPS", &log)
            .env("RUNE_T_MODE", mode)
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        text(&out)
    };
    run("one", &["build", "-j", "4"]);
    let s = stamps(&log);
    assert!(
        s["app end"] < s["base end"],
        "the race did not happen: {s:?}"
    );
    let again = run("one", &["build", "-j", "4"]);
    assert!(again.contains("0 compiled"), "{again}");
    assert!(run("two", &["run", "-j", "4"]).contains("mode=two"));
    let back = run("one", &["run", "-j", "4"]);
    assert!(back.contains("mode=one"), "{back}");
    assert!(back.contains("0 compiled"), "{back}");
}

// ---------------------------------------------------------------------------
// outdated / upgrade
// ---------------------------------------------------------------------------

/// Adds `"rust_version":"<rv>"` to one release in a local registry's index.
#[cfg(unix)]
fn require_rust(reg: &tempfile::TempDir, name: &str, version: &str, rv: &str) {
    let path = reg.path().join("index/1").join(name);
    let text = std::fs::read_to_string(&path).unwrap().replace(
        &format!("\"vers\":\"{version}\""),
        &format!("\"vers\":\"{version}\",\"rust_version\":\"{rv}\""),
    );
    std::fs::write(path, text).unwrap();
}

#[cfg(unix)]
#[test]
fn outdated_lists_what_the_lockfile_and_the_manifest_hold_back() {
    let reg = local_registry(&[
        ("a", "1.0.0", &[]),
        ("a", "1.2.0", &[]),
        ("a", "2.0.0", &[]),
        ("b", "0.3.0", &[]),
    ]);
    let p = app_with(&reg, "a = \"1.0\"\nb = \"0.3\"\n");
    p.write("Cargo.lock", "");
    std::fs::remove_file(p.path().join("Cargo.lock")).unwrap();
    // Lock the old release on purpose.
    p.ok(&["generate-lockfile"]);
    p.ok(&["update", "-p", "a", "--precise", "1.0.0"]);

    let out = p.rune(&["outdated", "--exit-code"]);
    let table = text(&out);
    assert_eq!(out.status.code(), Some(1), "{table}");
    let row = table
        .lines()
        .find(|l| l.starts_with("a "))
        .unwrap_or_else(|| panic!("no row for `a`:\n{table}"));
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(
        &cells[..5],
        ["a", "1.0", "1.0.0", "1.2.0", "2.0.0"],
        "{row}"
    );
    assert!(
        !table.lines().any(|l| l.starts_with("b ")),
        "b is current:\n{table}"
    );

    let json = p.rune(&["outdated", "--format", "json"]);
    let rows: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(rows[0]["name"], "a");
    assert_eq!(rows[0]["compatible"], "1.2.0");
}

#[cfg(unix)]
#[test]
fn upgrade_raises_requirements_keeping_the_manifests_formatting() {
    let reg = local_registry(&[
        ("a", "1.0.0", &[]),
        ("a", "1.2.0", &[]),
        ("a", "2.0.0", &[]),
        ("bb", "0.3.0", &[]),
        ("bb", "0.4.1", &[]),
    ]);
    let p = app_with(
        &reg,
        "a = \"1.0\" # keep this comment\nbb = { version = \"0.3\", default-features = false }\n",
    );
    p.ok(&["generate-lockfile"]);
    p.ok(&["update", "-p", "a", "--precise", "1.0.0"]);

    // Compatible: `a` to 1.2, `bb` stays (0.4 is a breaking release).
    let out = p.ok(&["upgrade"]);
    let manifest = p.read("Cargo.toml");
    assert!(
        manifest.contains("a = \"1.2\" # keep this comment"),
        "{manifest}\n{out}"
    );
    assert!(
        manifest.contains("bb = { version = \"0.3\", default-features = false }"),
        "{manifest}"
    );
    assert!(
        locked(&p).contains(&"a 1.2.0".to_string()),
        "{:?}",
        locked(&p)
    );

    // Incompatible, one dependency: only `bb`, table form kept.
    p.ok(&["upgrade", "--incompatible", "bb"]);
    let manifest = p.read("Cargo.toml");
    assert!(
        manifest.contains("bb = { version = \"0.4\", default-features = false }"),
        "{manifest}"
    );
    assert!(manifest.contains("a = \"1.2\""), "{manifest}");
    assert!(
        locked(&p).contains(&"bb 0.4.1".to_string()),
        "{:?}",
        locked(&p)
    );

    // A dry run writes nothing.
    let before = p.read("Cargo.toml");
    p.ok(&["upgrade", "--incompatible", "--dry-run"]);
    assert_eq!(p.read("Cargo.toml"), before);
}

#[cfg(unix)]
#[test]
fn upgrade_and_outdated_respect_the_packages_rust_version() {
    let reg = local_registry(&[
        ("a", "1.0.0", &[]),
        ("a", "1.5.0", &[]),
        ("a", "1.9.0", &[]),
    ]);
    require_rust(&reg, "a", "1.5.0", "1.50");
    require_rust(&reg, "a", "1.9.0", "9.99");
    let p = app_with(&reg, "a = \"1.0.0\"\n");
    let manifest = p.read("Cargo.toml").replace(
        "edition = \"2021\"",
        "edition = \"2021\"\nrust-version = \"1.60\"",
    );
    p.write("Cargo.toml", &manifest);
    p.ok(&["upgrade"]);
    assert!(
        p.read("Cargo.toml").contains("a = \"1.5.0\""),
        "{}",
        p.read("Cargo.toml")
    );
    let table = p.ok(&["outdated"]);
    assert!(table.contains("1.9.0 needs a newer Rust"), "{table}");
    p.ok(&["upgrade", "--ignore-rust-version"]);
    assert!(
        p.read("Cargo.toml").contains("a = \"1.9.0\""),
        "{}",
        p.read("Cargo.toml")
    );
}

#[cfg(unix)]
#[test]
fn upgrade_edits_workspace_dependencies_where_members_inherit_them() {
    let reg = local_registry(&[("a", "1.0.0", &[]), ("a", "1.3.0", &[])]);
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\"]\n\n[workspace.dependencies]\na = \"1.0\"\n",
    );
    p.write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\na = { workspace = true }\n",
    );
    p.write("app/src/main.rs", "fn main() {}\n");
    p.write(
        ".cargo/config.toml",
        &format!(
            "[source.crates-io]\nreplace-with = \"local\"\n[source.local]\nlocal-registry = {:?}\n",
            reg.path()
        ),
    );
    p.ok(&["upgrade"]);
    assert!(
        p.read("Cargo.toml").contains("a = \"1.3\""),
        "{}",
        p.read("Cargo.toml")
    );
    assert!(
        p.read("app/Cargo.toml")
            .contains("a = { workspace = true }")
    );
    // `-p app` only looks at the member's own manifest, which inherits.
    assert!(p.ok(&["upgrade", "-p", "app"]).contains("already allows"));
}

#[test]
fn out_dir_holds_only_what_the_script_generated_and_script_errors_are_shown() {
    let p = build_script_project();
    p.ok(&["build"]);
    let build = p.path().join(".rune/target/debug/build");
    let dirs: Vec<_> = std::fs::read_dir(&build).unwrap().flatten().collect();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    let out: Vec<String> = std::fs::read_dir(dirs[0].path().join("out"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        out,
        ["generated.rs"],
        "the script binary does not belong in OUT_DIR"
    );

    p.write("build.rs", "fn main() { let x: u32 = \"not a number\"; }\n");
    let failed = p.rune(&["build"]);
    assert!(!failed.status.success());
    let msg = text(&failed);
    assert!(
        msg.contains("mismatched types"),
        "the compiler's error is shown:\n{msg}"
    );
}

// ---------------------------------------------------------------------------
// unused
// ---------------------------------------------------------------------------

#[test]
fn unused_dependencies_are_found_by_asking_the_compiler() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\nused = { path = \"used\" }\nidle = { path = \"idle\" }\n\
         tests-only = { path = \"tests-only\" }\n\
         [dev-dependencies]\ndev-used = { path = \"dev-used\" }\ndev-idle = { path = \"dev-idle\" }\n\
         [build-dependencies]\nbuild-used = { path = \"build-used\" }\nbuild-idle = { path = \"build-idle\" }\n",
    );
    for name in [
        "used",
        "idle",
        "tests-only",
        "dev-used",
        "dev-idle",
        "build-used",
        "build-idle",
    ] {
        p.write(
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        );
        p.write(&format!("{name}/src/lib.rs"), "pub fn f() -> u32 { 1 }\n");
    }
    p.write("src/lib.rs", "pub fn g() -> u32 { used::f() }\n");
    p.write(
        "tests/it.rs",
        "#[test] fn t() { assert_eq!(dev_used::f() + tests_only::f() + app::g(), 3); }\n",
    );
    p.write(
        "build.rs",
        "fn main() { let _ = build_used::f(); println!(\"cargo::rerun-if-changed=build.rs\"); }\n",
    );

    for round in 0..2 {
        let out = p.rune(&["unused"]);
        let msg = text(&out);
        assert_eq!(out.status.code(), Some(1), "round {round}:\n{msg}");
        let unused_line = msg.lines().find(|l| l.contains("unused:")).unwrap_or("");
        for name in ["idle", "dev-idle", "build-idle"] {
            assert!(
                unused_line.contains(&format!("{name} ")),
                "round {round}: `{name}` is unused:\n{msg}"
            );
        }
        for name in ["used [", "dev-used", "build-used"] {
            assert!(
                !unused_line.contains(name),
                "round {round}: `{name}`:\n{msg}"
            );
        }
        let dev_line = msg
            .lines()
            .find(|l| l.contains("could be dev-dependencies"))
            .unwrap_or("");
        assert!(dev_line.contains("tests-only"), "round {round}:\n{msg}");
    }

    p.ok(&["unused", "--fix"]);
    let manifest = p.read("Cargo.toml");
    for gone in ["idle = ", "dev-idle", "build-idle"] {
        assert!(!manifest.contains(gone), "{manifest}");
    }
    assert!(
        manifest.contains("used = { path = \"used\" }"),
        "{manifest}"
    );
    let again = p.ok(&["unused"]);
    assert!(again.contains("no unused dependencies"), "{again}");
    p.ok(&["test"]);
}

#[test]
fn a_dependency_used_only_under_another_cfg_is_reported_but_never_removed() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\nelsewhere = { path = \"elsewhere\" }\n",
    );
    p.write(
        "elsewhere/Cargo.toml",
        "[package]\nname = \"elsewhere\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("elsewhere/src/lib.rs", "pub fn f() {}\n");
    p.write(
        "src/lib.rs",
        "#[cfg(target_os = \"plan9\")]\npub fn g() { elsewhere::f() }\n",
    );
    let out = p.ok(&["unused", "--fix"]);
    assert!(out.contains("unused in this configuration"), "{out}");
    assert!(
        p.read("Cargo.toml").contains("elsewhere = { path"),
        "must be kept"
    );
}

// ---------------------------------------------------------------------------
// sbom
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn sbom_lists_what_ships_with_checksums_purls_and_edges() {
    let reg = local_registry(&[
        ("a", "1.0.0", &[("b", "1")]),
        ("b", "1.1.0", &[]),
        ("t", "0.5.0", &[]),
    ]);
    let p = app_with(
        &reg,
        "a = \"1\"\nhelper = { path = \"helper\" }\n[dev-dependencies]\nt = \"0.5\"\n",
    );
    p.write(
        "helper/Cargo.toml",
        "[package]\nname = \"helper\"\nversion = \"0.2.0\"\nedition = \"2021\"\nlicense = \"MIT\"\n",
    );
    p.write("helper/src/lib.rs", "");
    let manifest = p.read("Cargo.toml").replace(
        "edition = \"2021\"",
        "edition = \"2021\"\nlicense = \"Apache-2.0\"\ndescription = \"demo\"",
    );
    p.write("Cargo.toml", &manifest);

    let out = p.rune(&["sbom"]);
    assert!(out.status.success(), "{}", text(&out));
    let bom: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(bom["bomFormat"], "CycloneDX");
    assert_eq!(bom["metadata"]["component"]["purl"], "pkg:cargo/app@0.1.0");
    assert_eq!(
        bom["metadata"]["component"]["licenses"][0]["expression"],
        "Apache-2.0"
    );
    let names: Vec<&str> = bom["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 3, "{names:?}");
    for n in ["a", "b", "helper"] {
        assert!(names.contains(&n), "{names:?}");
    }
    let a = bom["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "a")
        .unwrap();
    assert_eq!(a["purl"], "pkg:cargo/a@1.0.0");
    assert_eq!(a["hashes"][0]["alg"], "SHA-256");
    let lock = p.read("Cargo.lock");
    assert!(lock.contains(a["hashes"][0]["content"].as_str().unwrap()));
    let edges_of_a = bom["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["ref"] == "pkg:cargo/a@1.0.0")
        .unwrap();
    assert_eq!(edges_of_a["dependsOn"][0], "pkg:cargo/b@1.1.0");

    // Development dependencies on request.
    let with_dev: serde_json::Value =
        serde_json::from_slice(&p.rune(&["sbom", "--dev"]).stdout).unwrap();
    assert_eq!(with_dev["components"].as_array().unwrap().len(), 4);

    // SPDX, to a file.
    p.ok(&["sbom", "--format", "spdx", "-o", "bom.spdx.json"]);
    let spdx: serde_json::Value = serde_json::from_str(&p.read("bom.spdx.json")).unwrap();
    assert_eq!(spdx["spdxVersion"], "SPDX-2.3");
    let rel = spdx["relationships"].as_array().unwrap();
    assert!(rel.iter().any(|r| r["relationshipType"] == "DESCRIBES"
        && r["relatedSpdxElement"] == "SPDXRef-Package-app-0.1.0"));
    assert!(
        rel.iter()
            .any(|r| r["spdxElementId"] == "SPDXRef-Package-a-1.0.0"
                && r["relatedSpdxElement"] == "SPDXRef-Package-b-1.1.0")
    );
}

#[test]
fn exec_surface_lists_build_scripts_and_proc_macros_with_their_chain() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\nmid = { path = \"mid\" }\n",
    );
    p.write("src/lib.rs", "pub use mid::*;\n");
    p.write(
        "mid/Cargo.toml",
        "[package]\nname = \"mid\"\nversion = \"0.1.0\"\nedition = \"2021\"\nlinks = \"midnative\"\n\
         [dependencies]\nmacros = { path = \"../macros\" }\n",
    );
    p.write("mid/build.rs", "fn main() {}\n");
    p.write("mid/src/lib.rs", "pub use macros::*;\n");
    p.write(
        "macros/Cargo.toml",
        "[package]\nname = \"macros\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\nproc-macro = true\n",
    );
    p.write("macros/src/lib.rs", "");
    let human = p.ok(&["tree", "--exec-surface"]);
    assert!(human.contains("2 of 3 packages run code"), "{human}");
    let json = p.rune(&["tree", "--exec-surface", "--format", "json"]);
    let items: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let by_name = |n: &str| {
        items
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == n)
            .cloned()
            .unwrap_or_else(|| panic!("{n} missing: {items}"))
    };
    let mid = by_name("mid");
    assert_eq!(mid["runs"], serde_json::json!(["build-script", "links"]));
    assert_eq!(mid["links"], "midnative");
    assert_eq!(mid["via"], serde_json::json!(["app", "mid"]));
    let macros = by_name("macros");
    assert_eq!(macros["runs"], serde_json::json!(["proc-macro"]));
    assert_eq!(macros["via"], serde_json::json!(["app", "mid", "macros"]));
}

/// Sets `pubtime` of one release in a local registry's index.
#[cfg(unix)]
fn published(reg: &tempfile::TempDir, name: &str, version: &str, when: &str) {
    let path = reg.path().join("index/1").join(name);
    let text = std::fs::read_to_string(&path).unwrap().replace(
        &format!("\"vers\":\"{version}\""),
        &format!("\"vers\":\"{version}\",\"pubtime\":\"{when}\""),
    );
    std::fs::write(path, text).unwrap();
}

#[cfg(unix)]
#[test]
fn a_minimum_release_age_keeps_brand_new_releases_out_until_they_mature() {
    let reg = local_registry(&[
        ("a", "1.0.0", &[]),
        ("a", "1.1.0", &[]),
        ("b", "2.0.0", &[]),
    ]);
    published(&reg, "a", "1.0.0", "2020-01-01T00:00:00Z");
    published(&reg, "a", "1.1.0", "2099-01-01T00:00:00Z");
    published(&reg, "b", "2.0.0", "2099-01-01T00:00:00Z");
    let p = app_with(&reg, "a = \"1\"\nb = \"2\"\n");
    let with_age = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(args)
            .current_dir(p.path())
            .env("RUNE_HOME", p.home())
            .env("RUNE_RESOLVE_MIN_RELEASE_AGE", "7d")
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        text(&out)
    };
    let out = with_age(&["generate-lockfile"]);
    // `a` has a mature release; `b` has none, so the young one is taken with
    // a warning rather than failing the resolution.
    assert_eq!(locked(&p), ["a 1.0.0", "app 0.1.0", "b 2.0.0"]);
    assert!(
        out.contains("younger than resolve.min-release-age"),
        "{out}"
    );
    // Without the setting, the newest wins.
    p.ok(&["update"]);
    assert_eq!(locked(&p), ["a 1.1.0", "app 0.1.0", "b 2.0.0"]);
    // A locked young version stays locked when the setting comes back.
    with_age(&["build"]);
    assert_eq!(locked(&p), ["a 1.1.0", "app 0.1.0", "b 2.0.0"]);
}

// ---------------------------------------------------------------------------
// test runner: retries, flaky tests, JUnit
// ---------------------------------------------------------------------------

#[test]
fn retries_mark_flaky_tests_and_junit_reports_every_test() {
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    let lib = r#"pub fn one() -> u32 { 1 }
#[cfg(test)]
mod tests {
    #[test]
    fn flaky() {
        let marker = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("first-run-done");
        if !marker.exists() {
            std::fs::write(&marker, "").unwrap();
            panic!("fails the first time only");
        }
    }
    #[test]
    fn solid() { assert_eq!(super::one(), 1); }
    #[test]
    fn broken() { assert_eq!(super::one(), 2, "never passes"); }
    #[test]
    #[ignore]
    fn skipped() {}
}
"#;
    p.write("src/lib.rs", lib);
    let out = p.rune(&["test", "--retries", "2", "--junit", "report.xml"]);
    let msg = text(&out);
    assert_eq!(out.status.code(), Some(101), "`broken` still fails:\n{msg}");
    assert!(
        msg.contains("flaky test(s)") && msg.contains("tests::flaky"),
        "{msg}"
    );

    let xml = p.read("report.xml");
    assert!(xml.contains("tests=\"4\" failures=\"1\""), "{xml}");
    assert!(
        xml.contains("name=\"flaky\">\n      <system-out>flaky:"),
        "{xml}"
    );
    assert!(xml.contains("name=\"broken\">\n      <failure"), "{xml}");
    assert!(xml.contains("never passes"), "{xml}");
    assert!(xml.contains("name=\"skipped\">\n      <skipped/>"), "{xml}");
    assert!(xml.contains("name=\"solid\"/>"), "{xml}");

    // With the real failure gone, a flaky failure alone does not fail the run.
    std::fs::remove_file(p.path().join("first-run-done")).unwrap();
    p.write(
        "src/lib.rs",
        &lib.replace("super::one(), 2", "super::one(), 1"),
    );
    let out = p.rune(&["test", "--retries", "1"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("flaky test(s)"), "{}", text(&out));
    // Without retries, it fails as before.
    std::fs::remove_file(p.path().join("first-run-done")).unwrap();
    assert_eq!(p.rune(&["test"]).status.code(), Some(101));
}

#[test]
fn test_results_are_json_lines_with_message_format_json() {
    let p = lib_project();
    let out = p.rune(&["test", "--message-format", "json"]);
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect();
    let suite = lines
        .iter()
        .find(|l| l["reason"] == "test-suite-finished")
        .expect("a test-suite-finished line");
    assert_eq!(suite["outcome"], "passed");
    assert_eq!(suite["tests"][0]["name"], "tests::adds");
    assert_eq!(suite["tests"][0]["status"], "passed");
}

// ---------------------------------------------------------------------------
// cas export / import, affected --partition
// ---------------------------------------------------------------------------

#[test]
fn a_store_bundle_carries_a_build_to_a_fresh_machine() {
    let p = build_script_project();
    p.ok(&["build"]);
    p.ok(&["test", "--no-run"]);
    let bundle = p.home().join("ci-cache.tar.gz");
    let out = p.ok(&[
        "cas",
        "export",
        "-o",
        bundle.to_str().unwrap(),
        "--for",
        "build",
        "--for",
        "test",
    ]);
    assert!(out.contains("wrote"), "{out}");

    // A fresh store and a clean checkout: everything comes from the bundle.
    let fresh = tempfile::tempdir().unwrap();
    std::fs::remove_dir_all(p.path().join(".rune")).unwrap();
    let rune = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(args)
            .current_dir(p.path())
            .env("RUNE_HOME", fresh.path())
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        text(&out)
    };
    assert!(rune(&["cas", "import", bundle.to_str().unwrap()]).contains("imported"));
    let build = rune(&["build"]);
    assert!(build.contains(" 0 compiled"), "{build}");
    // After an edit the package compiles again, but its build script's
    // result comes from the bundle too: the script is not compiled.
    let main = std::fs::read_to_string(p.path().join("src/main.rs")).unwrap();
    std::fs::write(p.path().join("src/main.rs"), main.clone() + "\n// edited\n").unwrap();
    let edited = rune(&["build"]);
    assert!(!edited.contains(" 0 compiled"), "{edited}");
    std::fs::write(p.path().join("src/main.rs"), main).unwrap();
    let scripts: Vec<_> = std::fs::read_dir(p.path().join(".rune/target/debug/build"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("script").exists())
        .collect();
    assert!(scripts.is_empty(), "{scripts:?}");
    let run = rune(&["run"]);
    assert!(
        run.contains("cfg=true env=ok"),
        "the restored program runs:\n{run}"
    );
    let test = rune(&["test", "--no-run"]);
    assert!(
        test.contains(" 0 compiled") || !test.contains("compiled"),
        "{test}"
    );
}

#[test]
fn affected_partitions_split_the_packages_between_machines() {
    // app -> c2 -> c1 -> c0; an edit to c0 affects all four.
    let p = chain_project(3);
    git_in(p.path(), &["init", "-q", "-b", "main"]);
    git_in(p.path(), &["add", "-A"]);
    git_in(p.path(), &["commit", "-q", "-m", "base"]);
    p.write("c0/src/extra.rs", "");
    let shard = |n: &str| -> Vec<String> {
        p.ok(&["affected", "--partition", n])
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()
    };
    assert_eq!(shard("1/2"), ["app", "c1"]);
    assert_eq!(shard("2/2"), ["c0", "c2"]);
    let bad = p.rune(&["affected", "--partition", "3/2"]);
    assert!(!bad.status.success());
    assert!(text(&bad).contains("N/M"), "{}", text(&bad));
}

// ---------------------------------------------------------------------------
// build scripts rerun only when what they watch changes
// ---------------------------------------------------------------------------

/// A package whose build script counts its runs in `runs.log` (outside the
/// package) and reads `schema.txt` into a generated constant.
fn watching_script_project(watch: bool) -> Project {
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    let rerun = if watch {
        "println!(\"cargo::rerun-if-changed=build.rs\");\n    println!(\"cargo::rerun-if-changed=schema.txt\");\n    println!(\"cargo::rerun-if-changed=build\");"
    } else {
        ""
    };
    p.write(
        "build.rs",
        &format!(
            r#"#[path = "build/helper.rs"] mod helper;
use std::io::Write;
fn main() {{
    let log = std::env::var("RUNE_T_RUNS").unwrap();
    std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap().write_all(b"run\n").unwrap();
    let schema = std::fs::read_to_string("schema.txt").unwrap();
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("gen.rs"), format!("pub const SCHEMA: &str = {{:?}};", helper::decorate(schema.trim()))).unwrap();
    {rerun}
}}
"#
        ),
    );
    p.write(
        "build/helper.rs",
        "pub fn decorate(s: &str) -> String { format!(\"<{s}>\") }\n",
    );
    p.write("schema.txt", "v1\n");
    p.write(
        "src/lib.rs",
        "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\npub fn schema() -> &'static str { SCHEMA }\n#[test] fn t() { assert!(SCHEMA.starts_with('<')); }\n",
    );
    p
}

fn build_counting_runs(p: &Project) -> usize {
    let log = p.home().join("runs.log");
    let out = Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["build"])
        .current_dir(p.path())
        .env("RUNE_HOME", p.home())
        .env("RUNE_T_RUNS", &log)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    std::fs::read_to_string(&log).map_or(0, |t| t.lines().count())
}

#[test]
fn a_build_script_reruns_only_when_a_file_it_watches_changes() {
    let p = watching_script_project(true);
    assert_eq!(build_counting_runs(&p), 1);
    let bin = std::fs::read_dir(p.path().join(".rune/target/debug/build"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path()
        .join("script/build_script_build");
    let compiled_at = std::fs::metadata(&bin).unwrap().modified().unwrap();

    // The crate's own sources are not something the script reads.
    p.write("src/lib.rs", &(p.read("src/lib.rs") + "pub fn more() {}\n"));
    assert_eq!(
        build_counting_runs(&p),
        1,
        "an unwatched edit must not run the script"
    );

    // A watched input: the script runs again, but is not compiled again.
    std::thread::sleep(std::time::Duration::from_millis(20));
    p.write("schema.txt", "v2\n");
    assert_eq!(build_counting_runs(&p), 2);
    assert_eq!(
        std::fs::metadata(&bin).unwrap().modified().unwrap(),
        compiled_at
    );

    // A module of the script: compiled and run again.
    p.write(
        "build/helper.rs",
        "pub fn decorate(s: &str) -> String { format!(\"<<{s}>>\") }\n",
    );
    assert_eq!(build_counting_runs(&p), 3);
    assert_ne!(
        std::fs::metadata(&bin).unwrap().modified().unwrap(),
        compiled_at
    );

    // Going back to an earlier state finds its result in the store.
    p.write("schema.txt", "v1\n");
    p.write(
        "build/helper.rs",
        "pub fn decorate(s: &str) -> String { format!(\"<{s}>\") }\n",
    );
    assert_eq!(build_counting_runs(&p), 3);
}

#[test]
fn a_build_script_that_watches_nothing_reruns_on_any_change_like_cargo() {
    let p = watching_script_project(false);
    assert_eq!(build_counting_runs(&p), 1);
    p.write("src/lib.rs", &(p.read("src/lib.rs") + "pub fn more() {}\n"));
    assert_eq!(build_counting_runs(&p), 2);
}

// ---------------------------------------------------------------------------
// rune graph
// ---------------------------------------------------------------------------

#[test]
fn graph_draws_the_package_graph_as_dot_and_mermaid() {
    let p = chain_project(2);
    let dot = p.ok(&["graph"]);
    assert!(dot.starts_with("digraph rune {"), "{dot}");
    let id = |name: &str| {
        let line = dot
            .lines()
            .find(|l| l.contains(&format!("label=\"{name} 0.1.0\"")))
            .unwrap_or_else(|| panic!("no node {name}:\n{dot}"));
        line.split_whitespace().next().unwrap().to_string()
    };
    let (app, c1, c0) = (id("app"), id("c1"), id("c0"));
    assert!(dot.contains(&format!("{app} -> {c1};")), "{dot}");
    assert!(dot.contains(&format!("{c1} -> {c0};")), "{dot}");
    assert!(!dot.contains(&format!("{app} -> {c0};")), "{dot}");

    let mermaid = p.ok(&["graph", "--format", "mermaid", "-p", "c1"]);
    assert!(mermaid.starts_with("flowchart LR"), "{mermaid}");
    assert!(mermaid.contains("[\"c1 0.1.0\"]:::member"), "{mermaid}");
    assert!(!mermaid.contains("app"), "only what c1 reaches:\n{mermaid}");
}

#[test]
fn graph_units_lists_the_build_plan_with_keys_and_cache_state() {
    let p = chain_project(2);
    let plan = |p: &Project| -> serde_json::Value {
        let out = p.rune(&["graph", "--units", "--format", "json"]);
        assert!(out.status.success(), "{}", text(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let before = plan(&p);
    let units = before["units"].as_array().unwrap();
    let packages: std::collections::BTreeSet<&str> = units
        .iter()
        .map(|u| u["package"].as_str().unwrap())
        .collect();
    assert_eq!(
        packages.into_iter().collect::<Vec<_>>(),
        ["app", "c0", "c1"]
    );
    assert!(units.iter().all(|u| u["cached"] == false), "{before:#}");
    // Dependencies point at other units, by index.
    let c1 = units.iter().find(|u| u["package"] == "c1").unwrap();
    let to = c1["dependencies"][0]["index"].as_u64().unwrap() as usize;
    assert_eq!(units[to]["package"], "c0");
    assert_eq!(c1["dependencies"][0]["extern_crate_name"], "c0");

    p.ok(&["build"]);
    let after = plan(&p);
    let units = after["units"].as_array().unwrap();
    assert!(units.iter().all(|u| u["cached"] == true), "{after:#}");
    // The keys are the ones the build used, so they are stable.
    assert_eq!(plan(&p)["units"], after["units"]);
}

#[cfg(unix)]
#[test]
fn a_dependencys_build_script_warnings_stay_quiet_but_the_workspaces_show() {
    let reg = tempfile::tempdir().unwrap();
    let (bytes, sum) = make_crate_with(
        "noisy",
        "1.0.0",
        &[
            ("src/lib.rs", "pub fn f() {}\n"),
            (
                "build.rs",
                "fn main() { println!(\"cargo::warning=from the dependency\"); }\n",
            ),
        ],
    );
    std::fs::write(reg.path().join("noisy-1.0.0.crate"), bytes).unwrap();
    let dir = reg.path().join("index/no/is");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("noisy"),
        format!(
            "{{\"name\":\"noisy\",\"vers\":\"1.0.0\",\"deps\":[],\"cksum\":\"{sum}\",\"features\":{{}},\"yanked\":false}}\n"
        ),
    )
    .unwrap();
    let p = app_with(&reg, "noisy = \"1\"\n");
    p.write(
        "build.rs",
        "fn main() { println!(\"cargo::warning=from the workspace\"); }\n",
    );
    let out = p.ok(&["build"]);
    assert!(out.contains("from the workspace"), "{out}");
    // As with Cargo, a registry package's script warnings are not shown.
    assert!(!out.contains("from the dependency"), "{out}");
}

// ---------------------------------------------------------------------------
// rune tree: sections, platforms, edges, features
// ---------------------------------------------------------------------------

#[test]
fn tree_shows_what_a_build_here_uses_like_cargo_tree() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"lib1\", \"bld\", \"devd\", \"win\"]\n",
    );
    for (name, extra) in [
        (
            "lib1",
            "[features]\ndefault = [\"std\"]\nstd = []\nextra = []\n",
        ),
        ("bld", ""),
        ("devd", ""),
        ("win", ""),
    ] {
        p.write(
            &format!("{name}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}"
            ),
        );
        p.write(&format!("{name}/src/lib.rs"), "");
    }
    p.write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nlib1 = { path = \"../lib1\", features = [\"extra\"] }\n\n\
         [build-dependencies]\nbld = { path = \"../bld\" }\n\n\
         [dev-dependencies]\ndevd = { path = \"../devd\" }\n\n\
         [target.'cfg(windows)'.dependencies]\nwin = { path = \"../win\" }\n",
    );
    p.write("app/src/lib.rs", "");
    p.write("app/build.rs", "fn main() {}\n");

    let stdout = |args: &[&str]| {
        let out = p.rune(args);
        assert!(out.status.success(), "{}", text(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let tree = stdout(&["tree", "-p", "app"]);
    let lines: Vec<&str> = tree.lines().collect();
    assert!(lines[0].starts_with("app v0.1.0 ("), "{tree}");
    assert!(lines[1].starts_with("└── lib1 v0.1.0"), "{tree}");
    assert_eq!(lines[2], "[build-dependencies]", "{tree}");
    assert!(lines[3].starts_with("└── bld v0.1.0"), "{tree}");
    assert_eq!(lines[4], "[dev-dependencies]", "{tree}");
    assert!(lines[5].starts_with("└── devd v0.1.0"), "{tree}");
    // Not a Windows machine (these tests run on Linux and macOS).
    assert!(!tree.contains("win v0.1.0"), "{tree}");
    assert!(stdout(&["tree", "-p", "app", "--target", "all"]).contains("win v0.1.0"));
    assert!(
        stdout(&["tree", "-p", "app", "--target", "x86_64-pc-windows-msvc"]).contains("win v0.1.0")
    );

    let normal = stdout(&["tree", "-p", "app", "-e", "normal", "--prefix", "none"]);
    let names: Vec<&str> = normal
        .lines()
        .map(|l| l.split(' ').next().unwrap())
        .collect();
    assert_eq!(names, ["app", "lib1"], "{normal}");

    // Why is lib1's `std` on? Its `default`, which app did not turn off.
    let why = stdout(&["tree", "-e", "features", "-i", "lib1", "--prefix", "depth"]);
    let lines: Vec<&str> = why.lines().collect();
    let std = lines
        .iter()
        .position(|l| *l == "1lib1 feature \"std\"")
        .unwrap();
    assert!(
        lines[std + 1].starts_with("2lib1 feature \"default\""),
        "{why}"
    );
    let default = lines
        .iter()
        .position(|l| *l == "1lib1 feature \"default\"")
        .unwrap();
    assert!(lines[default + 1].starts_with("2app v0.1.0"), "{why}");
    assert!(lines.contains(&"1lib1 feature \"extra\""), "{why}");
}

#[test]
fn test_builds_the_examples_first_and_runs_those_marked_test() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [[example]]\nname = \"checked\"\ntest = true\n",
    );
    p.write("src/lib.rs", "pub fn one() -> u32 { 1 }\n");
    // A plain example: built, never run by `test`.
    p.write(
        "examples/tool.rs",
        "fn main() { println!(\"tool ran\"); }\n",
    );
    // `test = true`: its #[test]s run.
    p.write(
        "examples/checked.rs",
        "fn main() {}\n#[test]\nfn example_test() { assert_eq!(demo::one(), 1); }\n",
    );
    // An integration test that starts the example, as `once_cell`'s does:
    // it must exist by the time the test runs.
    p.write(
        "tests/runs_example.rs",
        r#"#[test]
fn example_binary_is_there() {
    let mut exe = std::env::current_exe().unwrap();
    exe.pop();
    exe.pop();
    let tool = exe.join("examples").join("tool").with_extension(std::env::consts::EXE_EXTENSION);
    let out = std::process::Command::new(&tool).output().expect("the example was built");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "tool ran");
}
"#,
    );
    let out = p.ok(&["test"]);
    assert!(out.contains("(example checked) · 1 passed"), "{out}");
    assert!(out.contains("(test runs_example) · 1 passed"), "{out}");
    assert!(
        !out.contains("example tool)"),
        "a plain example is not run: {out}"
    );
    assert!(out.contains("✓ 2 passed"), "{out}");
}

#[cfg(unix)]
#[test]
fn panic_abort_profiles_still_test_with_unwinding_like_cargo() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [profile.dev]\npanic = \"abort\"\n",
    );
    p.write(
        "src/lib.rs",
        "pub fn two() -> u32 { 2 }\n#[test]\nfn unit() { assert_eq!(two(), 2); }\n",
    );
    p.write(
        "src/main.rs",
        "fn main() { if std::env::args().count() > 1 { panic!(\"boom\") } }\n",
    );
    p.write(
        "tests/it.rs",
        "#[test]\n#[should_panic]\nfn unwinds() { panic!(\"caught by the harness\") }\n",
    );
    let out = p.ok(&["test"]);
    assert!(out.contains("✓ 2 passed"), "{out}");

    // An ordinary build keeps the profile's `panic = "abort"`.
    p.ok(&["build"]);
    use std::os::unix::process::ExitStatusExt;
    let status = Command::new(p.path().join(".rune/target/debug/app"))
        .arg("panic")
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.signal(), Some(6), "aborted, not unwound: {status:?}");
}

#[test]
fn store_entries_unused_for_a_month_are_removed_in_the_background() {
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    let cas = p.home().join("cas");
    let manifests = || -> std::collections::BTreeSet<PathBuf> {
        walkdir::WalkDir::new(cas.join("objects"))
            .into_iter()
            .flatten()
            .filter(|e| e.file_name() == "manifest.json")
            .map(|e| e.into_path())
            .collect()
    };
    // An old version of the code, then the current one.
    p.write("src/lib.rs", "pub fn f() {}\n");
    p.ok(&["build"]);
    let old = manifests();
    p.write("src/lib.rs", "pub fn f() {}\npub fn g() {}\n");
    p.ok(&["build"]);
    let current: Vec<PathBuf> = manifests().difference(&old).cloned().collect();
    assert!(!old.is_empty() && !current.is_empty());
    let stamp = cas.join("last-gc");
    assert!(stamp.exists(), "a new store only gets its stamp");

    // The old version's entries were stored and last used 40 days ago.
    let days_ago =
        |d: u64| std::time::SystemTime::now() - std::time::Duration::from_secs(d * 86_400);
    for m in &old {
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(m).unwrap()).unwrap();
        let then = days_ago(40)
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        json["stored_at"] = then.into();
        let mut perms = std::fs::metadata(m).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(m, perms).unwrap();
        std::fs::write(m, json.to_string()).unwrap();
        std::fs::File::open(m)
            .unwrap()
            .set_modified(days_ago(40))
            .unwrap();
    }
    // Not due yet (the last run was today): nothing happens.
    p.ok(&["build"]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(old.iter().all(|m| m.exists()));

    // A day after the last run, the next build starts one in the background.
    std::fs::File::open(&stamp)
        .unwrap()
        .set_modified(days_ago(2))
        .unwrap();
    p.ok(&["build"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while old.iter().any(|m| m.exists()) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(old.iter().all(|m| !m.exists()), "unused entries removed");
    assert!(current.iter().all(|m| m.exists()), "what builds use stays");
}

#[test]
fn a_cached_dependencys_script_output_still_reaches_rebuilt_dependents() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"sys\"]\n",
    );
    p.write(
        "sys/Cargo.toml",
        "[package]\nname = \"sys\"\nversion = \"0.1.0\"\nedition = \"2021\"\nlinks = \"sysx\"\n",
    );
    p.write(
        "sys/build.rs",
        "fn main() {\n    println!(\"cargo::rerun-if-changed=build.rs\");\n    println!(\"cargo::metadata=greeting=hello\");\n}\n",
    );
    p.write("sys/src/lib.rs", "");
    p.write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nsys = { path = \"../sys\" }\n",
    );
    // Reruns on any change of its package, and needs `sys`'s metadata.
    p.write(
        "app/build.rs",
        "fn main() {\n    let g = std::env::var(\"DEP_SYSX_GREETING\").expect(\"DEP_SYSX_GREETING\");\n    println!(\"cargo::rustc-env=GREETING={g}\");\n}\n",
    );
    p.write(
        "app/src/main.rs",
        "fn main() { println!(\"{}\", env!(\"GREETING\")); }\n",
    );
    p.ok(&["build"]);
    // Only `app` changes: `sys` comes from the store, its script does not
    // run again, and `app`'s script must still see what it reported.
    p.write(
        "app/src/main.rs",
        "fn main() { println!(\"{} again\", env!(\"GREETING\")); }\n",
    );
    let out = p.ok(&["run", "-p", "app"]);
    assert!(out.contains("hello again"), "{out}");

    // The same on a machine that got the store from a bundle.
    let bundle = p.home().join("bundle.tar");
    p.ok(&["cas", "export", "-o", bundle.to_str().unwrap()]);
    let fresh = tempfile::tempdir().unwrap();
    std::fs::remove_dir_all(p.path().join(".rune")).unwrap();
    let rune = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(args)
            .current_dir(p.path())
            .env("RUNE_HOME", fresh.path())
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        text(&out)
    };
    rune(&["cas", "import", bundle.to_str().unwrap()]);
    p.write(
        "app/src/main.rs",
        "fn main() { println!(\"{} once more\", env!(\"GREETING\")); }\n",
    );
    let out = rune(&["run", "-p", "app"]);
    assert!(out.contains("hello once more"), "{out}");
}

#[test]
fn json_messages_report_build_scripts_for_rust_analyzer() {
    let p = build_script_project();
    let executed = |p: &Project| -> Vec<serde_json::Value> {
        let out = p.rune(&["check", "--message-format", "json"]);
        assert!(out.status.success(), "{}", text(&out));
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["reason"] == "build-script-executed")
            .collect()
    };
    // Fresh, and again when everything comes from the store: once each time.
    for _ in 0..2 {
        let lines = executed(&p);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let m = &lines[0];
        assert!(
            m["package_id"]
                .as_str()
                .unwrap()
                .contains("#demo@1.2.3-beta.1"),
            "{m}"
        );
        assert_eq!(m["cfgs"], serde_json::json!(["from_script"]), "{m}");
        assert_eq!(m["env"], serde_json::json!([["SCRIPT_VAR", "ok"]]), "{m}");
        let out_dir = PathBuf::from(m["out_dir"].as_str().unwrap());
        assert!(out_dir.join("generated.rs").is_file(), "{m}");
    }
}

#[test]
fn a_declared_target_without_its_file_is_reported_like_cargo() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [[example]]\nname = \"gone\"\npath = \"examples/gone.rs\"\n",
    );
    p.write("src/lib.rs", "");
    // Not asked for: no error (plain `build` does not build examples).
    p.ok(&["build"]);
    let out = p.rune(&["test"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("can't find example `gone` at path"),
        "{}",
        text(&out)
    );
}

#[test]
fn a_links_package_with_integration_tests_is_not_its_own_conflict() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"natlib\"\nversion = \"0.1.0\"\nedition = \"2021\"\nlinks = \"natlib_0_1\"\n",
    );
    p.write("build.rs", "fn main() {}\n");
    p.write("src/lib.rs", "pub fn one() -> u32 { 1 }\n");
    p.write(
        "tests/aead_tests.rs",
        "#[test]\nfn works() { assert_eq!(natlib::one(), 1); }\n",
    );
    let out = p.ok(&["test"]);
    assert!(out.contains("(test aead_tests) · 1 passed"), "{out}");
}

#[cfg(unix)]
#[test]
fn a_workspace_crate_does_not_stand_in_for_its_published_release() {
    // `helper` (a dev-dependency) depends on the published `core_lib 1.0.0`;
    // the workspace develops `core_lib 1.1.0`. As with Cargo, `helper` gets
    // the published release, not the local one.
    let reg = local_registry(&[
        ("core_lib", "1.0.0", &[]),
        ("helper", "1.0.0", &[("core_lib", "^1.0")]),
    ]);
    let p = app_with(&reg, "");
    p.write(
        "Cargo.toml",
        "[package]\nname = \"core_lib\"\nversion = \"1.1.0\"\nedition = \"2021\"\n\n\
         [dev-dependencies]\nhelper = \"1\"\n",
    );
    p.write("src/lib.rs", "pub fn local() {}\n");
    p.ok(&["generate-lockfile"]);
    assert_eq!(
        locked(&p),
        ["core_lib 1.0.0", "core_lib 1.1.0", "helper 1.0.0"]
    );
    let lock = p.read("Cargo.lock");
    assert!(lock.contains("\"core_lib 1.0.0\""), "{lock}");
    p.ok(&["test"]);
}

#[test]
fn a_proc_macro_crates_doc_tests_run_like_cargo() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"pm\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n",
    );
    p.write(
        "src/lib.rs",
        r#"//! ```
//! pm::make_answer!();
//! assert_eq!(answer(), 42);
//! ```
use proc_macro::TokenStream;

#[proc_macro]
pub fn make_answer(_: TokenStream) -> TokenStream {
    "fn answer() -> u32 { 42 }".parse().unwrap()
}
"#,
    );
    let out = p.ok(&["test"]);
    assert!(out.contains("(doc-tests) · 1 passed"), "{out}");
}

#[test]
fn a_target_that_is_not_installed_is_reported_before_building() {
    let p = lib_project();
    let out = p.rune(&["build", "--target", "x86_64-unknown-linux-gnux"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("unknown target `x86_64-unknown-linux-gnux`"),
        "{}",
        text(&out)
    );

    // A real target without its standard library: what to run, not
    // rustc's "can't find crate for `core`" in every crate.
    let sysroot = String::from_utf8(
        Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let missing = [
        "riscv64gc-unknown-none-elf",
        "thumbv7em-none-eabihf",
        "wasm32v1-none",
    ]
    .into_iter()
    .find(|t| {
        !std::path::Path::new(sysroot.trim())
            .join("lib/rustlib")
            .join(t)
            .exists()
    });
    if let Some(t) = missing {
        let out = p.rune(&["build", "--target", t]);
        assert!(!out.status.success());
        let msg = text(&out);
        assert!(
            msg.contains(&format!("the standard library for `{t}` is not installed")),
            "{msg}"
        );
        assert!(!msg.contains("can't find crate"), "{msg}");
    }
}

#[test]
fn features_says_which_are_on_and_who_turned_them_on() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"lib\"]\n",
    );
    p.write(
        "lib/Cargo.toml",
        "[package]\nname = \"lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [features]\ndefault = [\"std\"]\nstd = []\nfast = [\"simd\"]\nsimd = []\nunused = []\n",
    );
    p.write("lib/src/lib.rs", "");
    p.write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nlib = { path = \"../lib\", features = [\"fast\"] }\n\n\
         [features]\nturbo = [\"lib/simd\"]\n",
    );
    p.write("app/src/main.rs", "fn main() {}\n");

    let out = p.ok(&["features", "lib"]);
    let line = |f: &str| {
        out.lines()
            .find(|l| l.contains(&format!("+ {f} ")) || l.trim_end().ends_with(&format!("+ {f}")))
            .unwrap_or_else(|| panic!("no `{f}` line in:\n{out}"))
            .to_string()
    };
    assert!(
        out.contains("lib 0.1.0") && out.contains("4 of 5 features on"),
        "{out}"
    );
    assert!(line("fast").contains("app"), "{out}");
    assert!(line("simd").contains("feature `fast`"), "{out}");
    assert!(line("default").contains("default features of app"), "{out}");
    assert!(line("std").contains("feature `default`"), "{out}");
    assert!(!out.contains("unused"), "{out}");
    assert!(p.ok(&["features", "lib", "--all"]).contains("- unused"));

    // Without an argument: what the workspace's packages declare.
    let out = p.ok(&["features"]);
    assert!(out.contains("turbo") && out.contains("lib/simd"), "{out}");

    let out = p.rune(&["features", "nope"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("`nope` is not in what a build"),
        "{}",
        text(&out)
    );
}

#[cfg(unix)]
#[test]
fn a_lockfile_from_another_resolution_is_never_trusted() {
    // Two `rune` processes at once (an editor's `rune check` and a build),
    // or a manifest saved while a resolution runs, could leave the lockfile
    // of one resolution next to the fingerprint of another: the next build
    // then trusted a lockfile without the new dependency, and rustc said
    // "undeclared crate". Recreate exactly that pairing.
    let reg = local_registry(&[("foo", "1.0.0", &[])]);
    let p = app_with(&reg, "");
    p.ok(&["build"]);
    let lock_path = p.path().join("Rune.lock.bin");
    let old = std::fs::read(&lock_path).unwrap();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nfoo = \"1\"\n",
    );
    p.write("src/main.rs", "fn main() { foo::f(); }\n");
    p.ok(&["build"]);
    let new = std::fs::read(&lock_path).unwrap();
    // `RUNELOCK\x03`, a u16 fingerprint length, the fingerprint, the lock.
    assert_eq!(&new[..9], b"RUNELOCK\x03");
    let header = |b: &[u8]| 11 + u16::from_le_bytes([b[9], b[10]]) as usize;
    let mut spliced = new[..header(&new)].to_vec();
    spliced.extend_from_slice(&old[header(&old)..]);
    std::fs::write(&lock_path, spliced).unwrap();

    p.write("src/main.rs", "fn main() { foo::f(); foo::f(); }\n");
    let out = p.ok(&["build"]);
    assert!(!out.contains("undeclared"), "{out}");
    // …and the lockfile is whole again.
    let fixed = std::fs::read(&lock_path).unwrap();
    assert_eq!(fixed, std::fs::read(&lock_path).unwrap());
    assert!(p.read("Cargo.lock").contains("name = \"foo\""));

    // A truncated lockfile (an interrupted write before writes were atomic)
    // is simply resolved again.
    std::fs::write(&lock_path, &fixed[..fixed.len() / 2]).unwrap();
    p.write("src/main.rs", "fn main() { foo::f(); }\n");
    p.ok(&["build"]);
}

#[test]
fn workspace_feature_unification_shares_dependency_builds_across_selections() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
    );
    p.write(
        "crates/shared/Cargo.toml",
        "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nextra = []\n",
    );
    p.write(
        "crates/shared/src/lib.rs",
        "pub fn extra() -> bool { cfg!(feature = \"extra\") }\n",
    );
    p.write(
        "crates/a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nshared = { path = \"../shared\" }\n",
    );
    p.write(
        "crates/a/src/main.rs",
        "fn main() { println!(\"extra={}\", shared::extra()); }\n",
    );
    // `b` asks for the feature — and does not compile, so it must never be
    // built when only `a` is selected.
    p.write(
        "crates/b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nshared = { path = \"../shared\", features = [\"extra\"] }\n",
    );
    p.write("crates/b/src/lib.rs", "this does not compile\n");

    // By default only the selected package's requests count.
    assert!(p.ok(&["run", "-p", "a"]).contains("extra=false"));

    // Rune's setting: `b`'s request counts, `b` itself is not built.
    p.write(
        "Rune.toml",
        "[build]\nfeature-unification = \"workspace\"\n",
    );
    assert!(p.ok(&["run", "-p", "a"]).contains("extra=true"));

    // Cargo's config key does the same.
    p.write("Rune.toml", "");
    p.write(
        ".cargo/config.toml",
        "[resolver]\nfeature-unification = \"workspace\"\n",
    );
    assert!(p.ok(&["run", "-p", "a"]).contains("extra=true"));

    p.write(
        ".cargo/config.toml",
        "[resolver]\nfeature-unification = \"everything\"\n",
    );
    let bad = p.rune(&["build", "-p", "a"]);
    assert!(!bad.status.success());
    assert!(
        text(&bad).contains("unknown feature unification"),
        "{}",
        text(&bad)
    );
}

#[test]
fn pinned_workspaces_survive_cleanup_and_idle_entries_are_compressed() {
    let p = Project::new();
    p.write("Cargo.toml", LIB_MANIFEST);
    p.write(
        "src/lib.rs",
        &format!(
            "pub fn f() -> &'static str {{ \"{}\" }}\n",
            "x".repeat(20_000)
        ),
    );
    p.ok(&["build"]);
    let cas = p.home().join("cas");
    let manifests: Vec<PathBuf> = walkdir::WalkDir::new(cas.join("objects"))
        .into_iter()
        .flatten()
        .filter(|e| e.file_name() == "manifest.json")
        .map(|e| e.into_path())
        .collect();
    assert!(!manifests.is_empty());
    // Everything was stored and last used 40 days ago.
    let then = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86_400);
    let age = || {
        for m in &manifests {
            let mut json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(m).unwrap()).unwrap();
            json["stored_at"] = then
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .into();
            let mut perms = std::fs::metadata(m).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            std::fs::set_permissions(m, perms).unwrap();
            std::fs::write(m, json.to_string()).unwrap();
            std::fs::File::open(m).unwrap().set_modified(then).unwrap();
        }
    };
    age();

    let out = p.ok(&["cas", "pin"]);
    assert!(out.contains("pinned"), "{out}");
    assert!(p.ok(&["cas", "pins"]).contains("entries"));

    // Pinned: kept, but compressed.
    let out = p.ok(&["cas", "gc", "--max-age", "30d", "--compress-idle", "7d"]);
    assert!(out.contains("0 entries"), "{out}");
    assert!(out.contains("kept for pinned"), "{out}");
    assert!(manifests.iter().all(|m| m.exists()));
    let compressed = |m: &PathBuf| {
        std::fs::read_dir(m.parent().unwrap())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".zst"))
    };
    assert!(manifests.iter().any(compressed), "{out}");

    // A fresh build restores from the compressed entries.
    p.ok(&["clean"]);
    assert!(p.ok(&["build"]).contains("0 compiled · 1 cached"));
    assert!(!manifests.iter().any(compressed));

    // Unpinned, old entries go.
    age();
    assert!(p.ok(&["cas", "unpin"]).contains("unpinned"));
    p.ok(&["cas", "gc", "--max-age", "30d"]);
    assert!(manifests.iter().all(|m| !m.exists()));
}

#[test]
fn tests_can_be_scheduled_one_batch_at_a_time_across_binaries() {
    let p = lib_project();
    let mut tests = String::new();
    for i in 0..12 {
        tests.push_str(&format!(
            "#[test] fn t{i}() {{ std::thread::sleep(std::time::Duration::from_millis(50)); }}\n"
        ));
    }
    tests.push_str("#[test] #[ignore] fn slow() {}\n");
    p.write("tests/many.rs", &tests);
    p.write("tests/other.rs", "#[test] fn alone() {}\n");

    let whole = p.ok(&["test", "--no-doc"]);
    let split = p.ok(&["test", "--no-doc", "--schedule", "tests"]);
    // The same totals either way: every test ran exactly once.
    assert!(split.contains("12 passed, 1 ignored"), "{split}");
    assert!(split.contains("14 passed · 1 ignored"), "{split}");
    assert!(whole.contains("14 passed · 1 ignored"), "{whole}");

    // A filter: the rest counts as filtered out, once.
    let out = p.ok(&["test", "--no-doc", "--schedule", "tests", "t1"]);
    assert!(out.contains("3 passed"), "{out}"); // t1, t10, t11

    // A failure in one batch fails the binary, and the setting works too.
    p.write("Rune.toml", "[test]\nschedule = \"tests\"\n");
    p.write(
        "tests/other.rs",
        "#[test] fn alone() {}\n#[test] fn broken() { panic!(\"nope\") }\n",
    );
    let out = p.rune(&["test", "--no-doc", "--no-fail-fast"]);
    assert!(!out.status.success());
    let t = text(&out);
    assert!(t.contains("nope") && t.contains("1 failed"), "{t}");
}

#[cfg(unix)]
#[test]
fn resolution_moves_back_from_a_release_whose_dependency_does_not_exist() {
    // The newest `b` needs a `c` that was never published; Cargo takes the
    // older `b`.
    let reg = local_registry(&[
        ("b", "1.0.0", &[("c", "^1.0")]),
        ("b", "1.1.0", &[("c", "^2.0")]),
        ("c", "1.0.0", &[]),
    ]);
    let p = app_with(&reg, "b = \"1\"\n");
    let out = p.ok(&["build"]);
    assert_eq!(locked(&p), ["app 0.1.0", "b 1.0.0", "c 1.0.0"], "{out}");
}

#[cfg(unix)]
#[test]
fn an_impossible_version_choice_is_explained() {
    let reg = local_registry(&[
        ("a", "1.0.0", &[]),
        ("a", "1.1.0", &[]),
        ("b", "1.0.0", &[("a", "=1.1.0")]),
    ]);
    let p = app_with(&reg, "a = \"=1.0.0\"\nb = \"1\"\n");
    let out = p.rune(&["build"]);
    let t = text(&out);
    // Cargo refuses; Rune explains, then builds with both versions.
    assert!(t.contains("b 1.0.0 depends on a 1.1.0"), "{t}");
    assert!(t.contains("side by side"), "{t}");
    assert!(out.status.success(), "{t}");
}

#[cfg(target_os = "linux")]
#[test]
fn the_sandbox_confines_build_scripts() {
    let p = lib_project();
    // Some systems (Ubuntu 23.10+ with AppArmor) do not allow the
    // namespaces the sandbox needs.
    let probe = p.rune(&["__sandbox", "--", "true"]);
    if !probe.status.success() {
        eprintln!("skipped: {}", text(&probe));
        return;
    }
    let outside = p.home().join("planted");
    p.write(
        "build.rs",
        &format!(
            r#"
fn main() {{
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{{out}}/generated.rs"), "pub const OK: u8 = 1;").unwrap();
    let planted = std::fs::write({outside:?}, "x").is_ok();
    let net = std::net::TcpStream::connect_timeout(
        &"1.1.1.1:80".parse().unwrap(),
        std::time::Duration::from_millis(500),
    )
    .map_err(|e| e.kind());
    println!("cargo::warning=planted={{planted}} net_unreachable={{}}",
        net.as_ref().err() == Some(&std::io::ErrorKind::NetworkUnreachable));
}}
"#
        ),
    );
    p.write(
        "src/lib.rs",
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n",
    );
    // Without the sandbox a build script can write anywhere.
    let out = p.ok(&["build"]);
    assert!(out.contains("planted=true"), "{out}");
    std::fs::remove_file(&outside).unwrap();

    p.write("Rune.toml", "[build]\nsandbox = true\n");
    let out = p.ok(&["build", "--rebuild-all"]);
    // OUT_DIR works; nothing else is writable, and there is no network.
    assert!(out.contains("planted=false"), "{out}");
    assert!(out.contains("net_unreachable=true"), "{out}");
    assert!(!outside.exists());
}

#[cfg(unix)]
#[test]
fn audit_reports_advisories_for_locked_versions() {
    let reg = local_registry(&[
        ("smallvec", "1.6.0", &[]),
        ("oldcrate", "0.3.1", &[]),
        ("middle", "1.0.0", &[("smallvec", "^1.6")]),
    ]);
    let p = app_with(&reg, "middle = \"1\"\noldcrate = \"0.3\"\n");
    p.ok(&["build"]);
    let db = p.path().join("advisories");
    std::fs::create_dir_all(&db).unwrap();
    let advisory = |id: &str, aliases: &[&str], name: &str, events: &str, informational: &str| {
        format!(
            r#"{{"id":"{id}","aliases":{aliases:?},"summary":"Problem in {name}","details":"Details.",
               "affected":[{{"package":{{"name":"{name}","ecosystem":"crates.io"}},
               "ranges":[{{"type":"SEMVER","events":[{events}]}}],
               "ecosystem_specific":{{"affects":{{"functions":["{name}::f"]}}}},
               "database_specific":{{"informational":{informational}}}}}],
               "severity":[{{"type":"CVSS_V3","score":"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"}}]}}"#
        )
    };
    let write = |file: &str, text: String| std::fs::write(db.join(file), text).unwrap();
    write(
        "RUSTSEC-2021-0003.json",
        advisory(
            "RUSTSEC-2021-0003",
            &["GHSA-43w2-9j62-hq99"],
            "smallvec",
            r#"{"introduced":"1.0.0"},{"fixed":"1.6.1"}"#,
            "null",
        ),
    );
    // GitHub's copy of the same advisory is not reported twice.
    write(
        "GHSA-43w2-9j62-hq99.json",
        advisory(
            "GHSA-43w2-9j62-hq99",
            &["RUSTSEC-2021-0003"],
            "smallvec",
            r#"{"introduced":"1.0.0"},{"fixed":"1.6.1"}"#,
            "null",
        ),
    );
    write(
        "RUSTSEC-2020-0001.json",
        advisory(
            "RUSTSEC-2020-0001",
            &[],
            "oldcrate",
            r#"{"introduced":"0"}"#,
            "\"unmaintained\"",
        ),
    );
    // Fixed before the locked version: not reported.
    write(
        "RUSTSEC-2019-0001.json",
        advisory(
            "RUSTSEC-2019-0001",
            &[],
            "smallvec",
            r#"{"introduced":"0"},{"fixed":"1.0.0"}"#,
            "null",
        ),
    );
    let db_arg = db.to_str().unwrap();

    let out = p.rune(&["audit", "--db", db_arg]);
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(
        t.contains("smallvec 1.6.0 · RUSTSEC-2021-0003 · critical 9.8"),
        "{t}"
    );
    assert!(
        t.contains("app 0.1.0 → middle 1.0.0 → smallvec 1.6.0"),
        "{t}"
    );
    assert!(t.contains("rune update smallvec --precise 1.6.1"), "{t}");
    assert!(
        t.contains("oldcrate 0.3.1 · RUSTSEC-2020-0001 · unmaintained"),
        "{t}"
    );
    assert!(
        !t.contains("GHSA-43w2") && !t.contains("RUSTSEC-2019-0001"),
        "{t}"
    );
    assert!(t.contains("1 vulnerability found · 1 warning"), "{t}");

    // Accepted: only the warning is left, which does not fail…
    let out = p.rune(&["audit", "--db", db_arg, "--ignore", "RUSTSEC-2021-0003"]);
    assert!(out.status.success(), "{}", text(&out));
    // …unless warnings are denied.
    let out = p.rune(&[
        "audit",
        "--db",
        db_arg,
        "--ignore",
        "GHSA-43w2-9j62-hq99",
        "--deny",
        "unmaintained",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    p.write("Rune.toml", "[audit]\nignore = \"RUSTSEC-2021-0003\"\n");
    assert!(p.rune(&["audit", "--db", db_arg]).status.success());
    p.write("Rune.toml", "");

    let out = p.rune(&["audit", "--db", db_arg, "--format", "json"]);
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(&out)));
    assert_eq!(json["vulnerabilities"]["count"], 1);
    assert_eq!(
        json["vulnerabilities"]["list"][0]["advisory"]["severity"],
        "critical"
    );
    assert_eq!(
        json["warnings"]["unmaintained"][0]["package"]["name"],
        "oldcrate"
    );
}

/// Whether the toolchain has llvm-cov (the `llvm-tools` component).
fn have_llvm_tools() -> bool {
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    std::path::Path::new(&sysroot)
        .join("lib/rustlib")
        .join(host_triple())
        .join("bin/llvm-cov")
        .is_file()
}

#[test]
fn coverage_counts_what_tests_run_including_programs_they_start() {
    if !have_llvm_tools() {
        eprintln!("skipped: no llvm-tools component");
        return;
    }
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"cov\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write(
        "src/lib.rs",
        "pub fn used(x: i32) -> i32 {\n    if x > 0 {\n        x\n    } else {\n        -x\n    }\n}\n\npub fn unused() -> i32 {\n    42\n}\n",
    );
    p.write(
        "src/main.rs",
        "fn main() {\n    println!(\"{}\", cov::used(-3));\n}\n",
    );
    // The integration test runs the program; its coverage counts too.
    p.write(
        "tests/run.rs",
        "#[test]\nfn runs() {\n    let out = std::process::Command::new(env!(\"CARGO_BIN_EXE_cov\")).output().unwrap();\n    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), \"3\");\n}\n",
    );
    let out = p.ok(&[
        "coverage",
        "--show-missing-lines",
        "--lcov",
        "-o",
        "lcov.info",
    ]);
    assert!(out.contains("src/lib.rs"), "{out}");
    assert!(out.contains("src/main.rs"), "{out}");
    // `used` ran only through the program, with a negative number.
    assert!(out.contains("not run: 3, 9-11"), "{out}");
    let lcov = p.read("lcov.info");
    assert!(lcov.contains("FNDA:1,cov::used"), "{lcov}");
    assert!(lcov.contains("FNDA:0,cov::unused"), "{lcov}");

    // A failing test: no report unless asked to keep going; the exit code
    // is the test runner's.
    p.write("tests/fail.rs", "#[test]\nfn fails() { panic!(\"no\") }\n");
    let out = p.rune(&["coverage"]);
    assert_eq!(out.status.code(), Some(101), "{}", text(&out));
    assert!(!text(&out).contains("of lines covered"), "{}", text(&out));
    let out = p.rune(&["coverage", "--no-fail-fast"]);
    assert_eq!(out.status.code(), Some(101), "{}", text(&out));
    assert!(text(&out).contains("of lines covered"), "{}", text(&out));
    std::fs::remove_file(p.path().join("tests/fail.rs")).unwrap();

    let out = p.rune(&["coverage", "--fail-under", "99"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("is below the required 99 %"));
    // Plain builds are not instrumented afterwards.
    p.ok(&["build"]);
}

#[test]
fn manifest_mistakes_cargo_refuses_are_refused_with_the_reason() {
    // A dependency cycle is an error naming the cycle (it used to hang).
    let p = Project::new();
    p.write("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n");
    p.write(
        "a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nb = { path = \"../b\" }\n",
    );
    p.write("a/src/lib.rs", "");
    p.write(
        "b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\na = { path = \"../a\" }\n",
    );
    p.write("b/src/lib.rs", "");
    let out = p.rune(&["build"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("a v0.1.0 → b v0.1.0 → a v0.1.0"),
        "{}",
        text(&out)
    );

    // No library and no binary.
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[package]\nname = \"empty\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    let out = p.rune(&["build"]);
    assert!(
        text(&out).contains("no targets specified"),
        "{}",
        text(&out)
    );

    // A feature naming nothing.
    p.write(
        "Cargo.toml",
        "[package]\nname = \"f\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\ndefault = [\"nope\"]\n",
    );
    p.write("src/lib.rs", "");
    let out = p.rune(&["build"]);
    assert!(
        text(&out).contains(
            "feature `default` includes `nope` which is neither a dependency nor another feature"
        ),
        "{}",
        text(&out)
    );
    // `dep:` on a dependency that is not optional.
    p.write(
        "Cargo.toml",
        "[package]\nname = \"f\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nx = { path = \"x\" }\n[features]\nextra = [\"dep:x\"]\n",
    );
    p.write(
        "x/Cargo.toml",
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    p.write("x/src/lib.rs", "");
    let out = p.rune(&["build"]);
    assert!(
        text(&out).contains("`x` is not an optional dependency"),
        "{}",
        text(&out)
    );
    // Optional dependencies (also target-specific ones) are fine.
    p.write(
        "Cargo.toml",
        "[package]\nname = \"f\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[target.'cfg(unix)'.dependencies]\nx = { path = \"x\", optional = true }\n[features]\nextra = [\"dep:x\", \"x/std\"]\nplain = [\"x\"]\n",
    );
    p.write(
        "x/Cargo.toml",
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nstd = []\n",
    );
    p.ok(&["build", "--features", "extra"]);
    // …but not a feature of a dependency that it does not have.
    p.write(
        "x/Cargo.toml",
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    let out = p.rune(&["build", "--features", "extra"]);
    assert!(
        text(&out)
            .contains("depends on `x` with feature `std`, but `x` does not have that feature"),
        "{}",
        text(&out)
    );

    // Two members with one name.
    let p = Project::new();
    p.write("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n");
    for d in ["a", "b"] {
        p.write(
            &format!("{d}/Cargo.toml"),
            "[package]\nname = \"same\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        p.write(&format!("{d}/src/lib.rs"), "");
    }
    let out = p.rune(&["build"]);
    assert!(
        text(&out).contains("two packages named `same`"),
        "{}",
        text(&out)
    );

    // Names of the standard library's crates.
    let p = Project::new();
    let out = p.rune(&["new", "test"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("built-in test library"),
        "{}",
        text(&out)
    );
}

#[test]
fn build_script_results_are_never_shared_between_different_packages() {
    // Two unrelated projects with a package of the same name and version, in
    // one store. Both scripts watch a file; each must be keyed by its own
    // script and its own files, never by the record of the other's run.
    let p = Project::new();
    let manifest = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    p.write("one/Cargo.toml", manifest);
    p.write(
        "one/build.rs",
        "fn main(){ println!(\"cargo::rerun-if-changed=data.txt\"); println!(\"cargo::rustc-env=DATA={}\", std::fs::read_to_string(\"data.txt\").unwrap().trim()); }",
    );
    p.write("one/data.txt", "from one");
    p.write(
        "one/src/main.rs",
        "fn main(){ println!(\"{}\", env!(\"DATA\")) }",
    );
    p.write("two/Cargo.toml", manifest);
    p.write(
        "two/build.rs",
        "fn main(){ println!(\"cargo::rerun-if-env-changed=MY_VAR\"); println!(\"cargo::rustc-env=V=[{}]\", std::env::var(\"MY_VAR\").unwrap_or_default()); }",
    );
    p.write(
        "two/src/main.rs",
        "fn main(){ println!(\"{}\", env!(\"V\")) }",
    );
    let run = |dir: &str| {
        let out = p.rune_in(&p.path().join(dir), &["run", "-q"]);
        assert!(out.status.success(), "{dir}: {}", text(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(run("one"), "from one");
    assert_eq!(run("two"), "[]");

    // The same script in a third checkout reads its own file.
    for f in ["Cargo.toml", "build.rs", "src/main.rs"] {
        p.write(&format!("three/{f}"), &p.read(&format!("one/{f}")));
    }
    p.write("three/data.txt", "from three");
    assert_eq!(run("three"), "from three");
    // …and one with the same file contents reuses the result.
    for f in ["Cargo.toml", "build.rs", "src/main.rs", "data.txt"] {
        p.write(&format!("four/{f}"), &p.read(&format!("one/{f}")));
    }
    assert_eq!(run("four"), "from one");
}

#[test]
fn default_run_and_metadata_nodes_match_cargo() {
    let p = Project::new();
    p.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"helper\"]\n",
    );
    p.write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\ndefault-run = \"second\"\n[dependencies]\nhelper = { path = \"../helper\" }\n",
    );
    p.write("app/src/bin/first.rs", "fn main(){ println!(\"first\") }");
    p.write(
        "app/src/bin/second.rs",
        "fn main(){ println!(\"second {}\", helper::X) }",
    );
    p.write(
        "helper/Cargo.toml",
        "[package]\nname = \"helper\"\nversion = \"0.2.0\"\nedition = \"2021\"\n",
    );
    p.write("helper/src/lib.rs", "pub const X: u8 = 1;");
    let out = p.rune_in(&p.path().join("app"), &["run", "-q"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "second 1",
        "{}",
        text(&out)
    );

    let out = p.rune(&["metadata", "--format-version", "1"]);
    let m: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let dir = p.path().canonicalize().unwrap();
    let app_id = format!("path+file://{}/app#0.1.0", dir.display());
    let helper_id = format!("path+file://{}/helper#0.2.0", dir.display());
    let nodes = m["resolve"]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2, "{m}");
    let app = nodes
        .iter()
        .find(|n| n["id"] == app_id.as_str())
        .expect("app node");
    assert_eq!(app["deps"][0]["name"], "helper");
    assert_eq!(app["deps"][0]["pkg"], helper_id.as_str());
    let pkg = m["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "app")
        .unwrap();
    assert_eq!(pkg["default_run"], "second");
    assert_eq!(pkg["targets"].as_array().unwrap().len(), 2);
}
