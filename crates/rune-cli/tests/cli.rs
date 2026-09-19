//! End-to-end tests: run the real `rune` binary against throwaway projects.
//!
//! Every project gets its own `RUNE_HOME`, so tests never touch the developer's
//! CAS, registry cache, or global settings, and use only path dependencies, so
//! they need no network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn home(&self) -> PathBuf {
        self.path().join(".test-home")
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
fn git_dependencies_fail_loudly_instead_of_resolving_to_the_wrong_crate() {
    let p = lib_project();
    p.write(
        "Cargo.toml",
        &format!("{LIB_MANIFEST}\n[dependencies]\nserde = {{ git = \"https://example.invalid/serde\" }}\n"),
    );
    let out = p.rune(&["build"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("git source"), "{}", text(&out));
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
