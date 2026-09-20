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
    let p = patch_project("[patch.crates-io]\nfoo = { path = \"foo-fork\" }\n");
    let out = p.ok(&["run"]);
    assert!(out.contains("direct=the fork via_mid=the fork"), "{out}");
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
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(format!("{name}-{version}"));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), lib_rs).unwrap();
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
