//! Git-based registry indexes (`registry+https://…`, or a `[registries]`
//! index URL without `sparse+`): the protocol registries used before sparse
//! indexes, and still used by some private ones.
//!
//! The index is a git repository with the same file layout as a sparse index
//! (`3/f/foo`, `se/rd/serde`, `config.json` at the root). Rune keeps a shallow
//! clone under `RUNE_HOME/registry/git-index/`, made and updated with the
//! system `git` (so credential helpers and `insteadOf` rules apply), and
//! reads files from it. A clone is updated at most once per process: when
//! asked to (`rune update`), or when a crate or version is missing.

use crate::index::{CrateIndex, IndexError, SparseIndexClient};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

pub struct GitIndex {
    url: String,
    dir: PathBuf,
    offline: bool,
}

/// Clones updated in this process.
fn updated() -> &'static Mutex<HashSet<PathBuf>> {
    static UPDATED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    UPDATED.get_or_init(Default::default)
}

impl GitIndex {
    /// The clone of the index at `url` (`registry+` prefix optional),
    /// cloning it first if there is none yet.
    pub fn open(url: &str, offline: bool) -> Result<GitIndex, IndexError> {
        let url = url.strip_prefix("registry+").unwrap_or(url).to_string();
        let dir = rune_common::settings::rune_home()
            .join("registry")
            .join("git-index")
            .join(crate::index::registry_dir_key(&url));
        let index = GitIndex { url, dir, offline };
        if !index.dir.join(".git").exists() {
            if offline {
                return Err(IndexError::Config(format!(
                    "the git index `{}` has not been cloned yet and Rune is offline",
                    index.url
                )));
            }
            if let Some(parent) = index.dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = index.dir.with_extension("cloning");
            let _ = std::fs::remove_dir_all(&tmp);
            git(
                None,
                &[
                    "clone",
                    "--depth",
                    "1",
                    "--quiet",
                    &index.url,
                    &tmp.to_string_lossy(),
                ],
            )?;
            // Another process may have finished first; either clone will do.
            if std::fs::rename(&tmp, &index.dir).is_err() {
                let _ = std::fs::remove_dir_all(&tmp);
            }
            updated().lock().unwrap().insert(index.dir.clone());
        }
        Ok(index)
    }

    /// The working tree of the clone.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Brings the clone up to date with the remote (once per process).
    pub fn update(&self) -> Result<(), IndexError> {
        if self.offline || !updated().lock().unwrap().insert(self.dir.clone()) {
            return Ok(());
        }
        git(
            Some(&self.dir),
            &["fetch", "--depth", "1", "--quiet", "origin", "HEAD"],
        )?;
        git(
            Some(&self.dir),
            &["reset", "--hard", "--quiet", "FETCH_HEAD"],
        )?;
        Ok(())
    }

    fn read_local(&self, name: &str) -> Result<CrateIndex, IndexError> {
        let path = self.dir.join(SparseIndexClient::crate_index_path(name));
        let text = std::fs::read_to_string(&path)
            .map_err(|_| IndexError::CrateNotFound(name.to_string()))?;
        SparseIndexClient::parse_index_text(name, &text)
    }

    /// A crate's entries, updating the clone once if it does not know it.
    pub fn read(&self, name: &str) -> Result<CrateIndex, IndexError> {
        match self.read_local(name) {
            Err(IndexError::CrateNotFound(_)) if !self.offline => {
                self.update()?;
                self.read_local(name)
            }
            other => other,
        }
    }

    /// A crate's entries after bringing the clone up to date.
    pub fn refresh(&self, name: &str) -> Result<CrateIndex, IndexError> {
        self.update()?;
        self.read_local(name)
    }
}

fn git(cwd: Option<&Path>, args: &[&str]) -> Result<String, IndexError> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0").args(args);
    let out = cmd.output().map_err(|e| {
        IndexError::Config(format!(
            "git-based registry indexes need `git` on PATH: {e}"
        ))
    })?;
    if !out.status.success() {
        return Err(IndexError::Config(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_updates_an_index_repository() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: tests in this crate do not read RUNE_HOME concurrently.
        unsafe { std::env::set_var("RUNE_HOME", home.path()) };
        let remote = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(remote.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        let line = |v: &str| {
            format!(
                "{{\"name\":\"foo\",\"vers\":\"{v}\",\"deps\":[],\"cksum\":\"00\",\"features\":{{}},\"yanked\":false}}\n"
            )
        };
        std::fs::create_dir_all(remote.path().join("3/f")).unwrap();
        std::fs::write(remote.path().join("3/f/foo"), line("1.0.0")).unwrap();
        std::fs::write(
            remote.path().join("config.json"),
            "{\"dl\":\"http://x/dl\"}",
        )
        .unwrap();
        run(&["init", "-q"]);
        run(&["add", "-A"]);
        run(&["commit", "-qm", "one"]);

        let url = format!("file://{}", remote.path().display());
        let index = GitIndex::open(&format!("registry+{url}"), false).unwrap();
        assert_eq!(index.read("foo").unwrap().available_versions().len(), 1);
        assert!(index.dir().join("config.json").is_file());

        // A new release: seen after an update, not before.
        let text = format!("{}{}", line("1.0.0"), line("1.1.0"));
        std::fs::write(remote.path().join("3/f/foo"), text).unwrap();
        run(&["commit", "-qam", "two"]);
        updated().lock().unwrap().clear();
        let again = GitIndex::open(&url, false).unwrap();
        assert_eq!(again.refresh("foo").unwrap().available_versions().len(), 2);
    }
}
