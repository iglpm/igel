//! Records which commit Igel was built from, for `igl --version`.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo::rerun-if-env-changed=IGEL_BUILD_COMMIT");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let commit = std::env::var("IGEL_BUILD_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| git(&["rev-parse", "--short=9", "HEAD"]));
    let date = git(&["log", "-1", "--format=%cs"]);
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        // Only watch files that exist: Cargo treats a missing one as changed on every build.
        let git_dir = Path::new(&git_dir);
        let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| git_dir.to_path_buf());
        let watch = |path: std::path::PathBuf| {
            if path.is_file() {
                println!("cargo::rerun-if-changed={}", path.display());
            }
        };
        watch(git_dir.join("HEAD"));
        if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
            watch(common.join(head_ref));
        }
        watch(common.join("packed-refs"));
    }
    let long = match (&commit, &date) {
        (Some(c), Some(d)) => format!("{version} ({c} {d})"),
        (Some(c), None) => format!("{version} ({c})"),
        _ => version,
    };
    println!("cargo::rustc-env=IGEL_VERSION={long}");
    println!(
        "cargo::rustc-env=IGEL_COMMIT_HASH={}",
        commit.unwrap_or_default()
    );
    println!(
        "cargo::rustc-env=IGEL_COMMIT_DATE={}",
        date.unwrap_or_default()
    );
    println!(
        "cargo::rustc-env=IGEL_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
}
