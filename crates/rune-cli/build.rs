//! Records which commit Rune was built from, for `rune --version`.

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
    println!("cargo::rerun-if-env-changed=RUNE_BUILD_COMMIT");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    // Release builds from a source archive can name their commit.
    let commit = std::env::var("RUNE_BUILD_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| git(&["rev-parse", "--short=9", "HEAD"]));
    let date = git(&["log", "-1", "--format=%cs"]);
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        // A new commit or checkout changes the version string.
        let git_dir = Path::new(&git_dir);
        println!("cargo::rerun-if-changed={}", git_dir.join("HEAD").display());
        if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!(
                "cargo::rerun-if-changed={}",
                git_dir.join(head_ref).display()
            );
        }
        println!(
            "cargo::rerun-if-changed={}",
            git_dir.join("packed-refs").display()
        );
    }
    let long = match (&commit, &date) {
        (Some(c), Some(d)) => format!("{version} ({c} {d})"),
        (Some(c), None) => format!("{version} ({c})"),
        _ => version,
    };
    println!("cargo::rustc-env=RUNE_VERSION={long}");
    println!(
        "cargo::rustc-env=RUNE_COMMIT_HASH={}",
        commit.unwrap_or_default()
    );
    println!(
        "cargo::rustc-env=RUNE_COMMIT_DATE={}",
        date.unwrap_or_default()
    );
    println!(
        "cargo::rustc-env=RUNE_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
}
