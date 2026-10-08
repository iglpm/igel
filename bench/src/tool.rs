//! The two tools under test, each in its own copy with a scrubbed environment.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub bin: PathBuf,
    pub version: String,
}

pub struct Ctx<'a> {
    pub tool: &'a Tool,
    pub dir: PathBuf,
    pub home: PathBuf,
}

const SCRUB: &[&str] = &[
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    "CARGO_INCREMENTAL",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
];

impl Tool {
    pub fn find(name: &str, given: Option<&Path>) -> Result<Tool> {
        let bin = match given {
            Some(p) => p.to_path_buf(),
            None => {
                which(name).with_context(|| format!("`{name}` is not on PATH (pass --{name})"))?
            }
        };
        // Absolute, but symlinks kept: `cargo` is usually a link to `rustup`, which looks at its name.
        let bin = if bin.is_absolute() {
            bin
        } else {
            std::env::current_dir()?.join(bin)
        };
        anyhow::ensure!(bin.is_file(), "no {name} at {}", bin.display());
        let out = Command::new(&bin)
            .arg("--version")
            .output()
            .with_context(|| format!("running {} --version", bin.display()))?;
        anyhow::ensure!(out.status.success(), "{} --version failed", bin.display());
        Ok(Tool {
            name: name.to_string(),
            bin,
            version: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        })
    }

    pub fn is_rune(&self) -> bool {
        self.name == "rune"
    }

    pub fn command(&self, ctx: &Ctx, args: &[&str]) -> Command {
        let mut c = Command::new(&self.bin);
        c.args(args).current_dir(&ctx.dir);
        for var in SCRUB {
            c.env_remove(var);
        }
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("RUNE_") {
                c.env_remove(k);
            }
        }
        c.env("NO_COLOR", "1")
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_TERM_PROGRESS_WHEN", "never");
        if self.is_rune() {
            c.env("RUNE_HOME", &ctx.home);
        }
        c
    }

    pub fn output_dir(&self, ctx: &Ctx) -> PathBuf {
        if self.is_rune() {
            ctx.dir.join(".rune")
        } else {
            ctx.dir.join("target")
        }
    }

    pub fn remove_output(&self, ctx: &Ctx) -> Result<()> {
        remove(&self.output_dir(ctx))
    }

    pub fn remove_caches(&self, ctx: &Ctx) -> Result<()> {
        self.remove_output(ctx)?;
        if self.is_rune() {
            remove(&ctx.home.join("cas"))?;
        }
        Ok(())
    }

    pub fn disk_use(&self, ctx: &Ctx) -> (u64, u64) {
        let out_dir = self.output_dir(ctx);
        let out = crate::measure::dir_size(&out_dir);
        let total = if self.is_rune() {
            crate::measure::dirs_size(&[&out_dir, &ctx.home.join("cas")])
        } else {
            out
        };
        (out, total)
    }
}

pub fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            make_writable(path);
            std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

fn make_writable(path: &Path) {
    for entry in walkdir::WalkDir::new(path).into_iter().flatten() {
        if let Ok(meta) = entry.metadata() {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(entry.path(), perms);
        }
    }
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .find(|p| p.is_file())
}
