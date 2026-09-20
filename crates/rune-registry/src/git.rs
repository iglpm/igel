//! Git dependencies.
//!
//! A git dependency names a moving target — a branch, a tag, the default
//! branch — but a build needs one exact revision. This module fetches a
//! repository into a shared database, resolves the reference to a **commit**,
//! and checks that commit out (submodules included) into a directory that is
//! immutable from then on. The commit (and the state of the submodules) is
//! the package's *source identity*: it, not the URL or the branch name, goes
//! into cache keys and the lockfile.
//!
//! Rune drives the `git` command line rather than embedding a git library, so
//! authentication (ssh agents, credential helpers, `insteadOf` rewrites)
//! behaves exactly as it does for the user's own `git`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("could not run `git` (is it installed and on PATH?): {0}")]
    NoGit(std::io::Error),
    #[error("`git {args}` failed:\n{stderr}")]
    Command { args: String, stderr: String },
    #[error("git dependency `{url}` is not available offline (run without `--offline` once)")]
    Offline { url: String },
    #[error("could not find {what} in `{url}`")]
    NoSuchRef { url: String, what: String },
    #[error("no package named `{name}` found in the git repository `{url}`")]
    NoPackage { url: String, name: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GitError>;

/// What a manifest asks for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GitRef {
    /// Whatever the remote's `HEAD` is.
    DefaultBranch,
    Branch(String),
    Tag(String),
    /// A commit hash (possibly abbreviated) or another ref such as
    /// `refs/pull/1/head`.
    Rev(String),
}

impl GitRef {
    pub fn from_spec(branch: Option<&str>, tag: Option<&str>, rev: Option<&str>) -> GitRef {
        match (branch, tag, rev) {
            (_, _, Some(r)) => GitRef::Rev(r.to_string()),
            (_, Some(t), _) => GitRef::Tag(t.to_string()),
            (Some(b), _, _) => GitRef::Branch(b.to_string()),
            _ => GitRef::DefaultBranch,
        }
    }

    /// The inverse of [`GitRef::lock_query`].
    pub fn from_query(query: Option<&str>) -> GitRef {
        match query.and_then(|q| q.split_once('=')) {
            Some(("branch", b)) => GitRef::Branch(b.to_string()),
            Some(("tag", t)) => GitRef::Tag(t.to_string()),
            Some(("rev", r)) => GitRef::Rev(r.to_string()),
            _ => GitRef::DefaultBranch,
        }
    }

    /// The `branch=..`/`tag=..`/`rev=..` part of a lockfile source.
    pub fn lock_query(&self) -> Option<String> {
        match self {
            GitRef::DefaultBranch => None,
            GitRef::Branch(b) => Some(format!("branch={b}")),
            GitRef::Tag(t) => Some(format!("tag={t}")),
            GitRef::Rev(r) => Some(format!("rev={r}")),
        }
    }
}

/// A repository checked out at one exact commit.
#[derive(Debug, Clone)]
pub struct GitCheckout {
    pub url: String,
    pub reference: GitRef,
    /// The resolved 40-hex commit.
    pub commit: String,
    /// The working tree; never modified after creation.
    pub dir: PathBuf,
    /// `path=commit` of every submodule, sorted and comma-joined.
    pub submodules: String,
}

impl GitCheckout {
    /// The lockfile `source` for a package from this checkout.
    pub fn lock_source(&self) -> String {
        match self.reference.lock_query() {
            Some(q) => format!("git+{}?{q}#{}", self.url, self.commit),
            None => format!("git+{}#{}", self.url, self.commit),
        }
    }
}

/// Splits a lockfile git source into `(url, query, commit)`.
pub fn parse_lock_source(source: &str) -> Option<(String, Option<String>, String)> {
    let rest = source.strip_prefix("git+")?;
    let (rest, commit) = rest.rsplit_once('#')?;
    match rest.split_once('?') {
        Some((url, q)) => Some((url.to_string(), Some(q.to_string()), commit.to_string())),
        None => Some((rest.to_string(), None, commit.to_string())),
    }
}

/// Process-wide behaviour of git dependencies.
#[derive(Debug, Clone, Default)]
pub struct GitOptions {
    /// Never touch the network.
    pub offline: bool,
    /// Ignore `pins` and move to the newest commit each reference points to
    /// (`rune update`).
    pub update: bool,
    /// `(url, lock query, commit)` from the lockfile: a build resolves a
    /// reference to *this* commit rather than to wherever a branch has moved.
    pub pins: Vec<(String, Option<String>, String)>,
}

static OPTIONS: OnceLock<Mutex<GitOptions>> = OnceLock::new();
static MEMO: OnceLock<Mutex<HashMap<(String, GitRef), GitCheckout>>> = OnceLock::new();

fn options() -> &'static Mutex<GitOptions> {
    OPTIONS.get_or_init(Default::default)
}

fn memo() -> &'static Mutex<HashMap<(String, GitRef), GitCheckout>> {
    MEMO.get_or_init(Default::default)
}

/// Sets the process-wide options and forgets earlier resolutions.
pub fn configure(opts: GitOptions) {
    *options().lock().unwrap() = opts;
    memo().lock().unwrap().clear();
}

/// Every checkout resolved so far in this process.
pub fn resolved_checkouts() -> Vec<GitCheckout> {
    memo().lock().unwrap().values().cloned().collect()
}

/// The checkout whose working tree contains `dir`.
pub fn locate_dir(dir: &Path) -> Option<GitCheckout> {
    memo()
        .lock()
        .unwrap()
        .values()
        .find(|c| dir.starts_with(&c.dir))
        .cloned()
}

fn git_root() -> PathBuf {
    rune_common::settings::rune_home().join("git")
}

fn repo_key(url: &str) -> String {
    let name = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("repo")
        .trim_end_matches(".git");
    let hash = blake3::hash(url.as_bytes()).to_hex();
    format!("{name}-{}", &hash[..16])
}

fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    // A dependency fetch must never stop to ask a question nobody can answer.
    cmd.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C");
    cmd.args(args);
    let out = cmd.output().map_err(GitError::NoGit)?;
    if !out.status.success() {
        return Err(GitError::Command {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn has_commit(db: &Path, commit: &str) -> bool {
    run_git(
        Some(db),
        &["cat-file", "-e", &format!("{commit}^{{commit}}")],
    )
    .is_ok()
}

/// Resolves `reference` of `url` to a commit and checks it out.
///
/// The result is remembered for the life of the process, so every part of a
/// build agrees on which commit a branch meant.
pub fn checkout(url: &str, reference: &GitRef) -> Result<GitCheckout> {
    let key = (url.to_string(), reference.clone());
    if let Some(hit) = memo().lock().unwrap().get(&key) {
        return Ok(hit.clone());
    }
    let opts = options().lock().unwrap().clone();
    let db = git_root().join("db").join(repo_key(url));

    let pinned = (!opts.update)
        .then(|| {
            opts.pins
                .iter()
                .find(|(u, q, _)| u == url && *q == reference.lock_query())
                .map(|(_, _, c)| c.clone())
        })
        .flatten();

    if !db.join("HEAD").is_file() {
        if opts.offline {
            return Err(GitError::Offline { url: url.into() });
        }
        std::fs::create_dir_all(&db)?;
        run_git(Some(&db), &["init", "--bare", "--quiet"])?;
    }

    let commit = match pinned.filter(|c| has_commit(&db, c)) {
        Some(c) => c,
        None => {
            if opts.offline {
                return Err(GitError::Offline { url: url.into() });
            }
            fetch(&db, url, reference)?;
            resolve_commit(&db, url, reference)?
        }
    };

    let dir = git_root()
        .join("checkouts")
        .join(repo_key(url))
        .join(&commit[..12.min(commit.len())]);
    let submodules = materialize(&db, &dir, &commit)?;
    let checkout = GitCheckout {
        url: url.to_string(),
        reference: reference.clone(),
        commit,
        dir,
        submodules,
    };
    memo().lock().unwrap().insert(key, checkout.clone());
    Ok(checkout)
}

fn fetch(db: &Path, url: &str, reference: &GitRef) -> Result<()> {
    match reference {
        GitRef::Rev(r) if !is_hex(r) => {
            // A ref such as `refs/pull/1/head`: not covered by the branch and
            // tag globs below.
            run_git(Some(db), &["fetch", "--force", "--quiet", url, r])?;
        }
        _ => {
            run_git(
                Some(db),
                &[
                    "fetch",
                    "--force",
                    "--quiet",
                    "--tags",
                    url,
                    "+refs/heads/*:refs/heads/*",
                    "+refs/tags/*:refs/tags/*",
                ],
            )?;
        }
    }
    Ok(())
}

fn is_hex(s: &str) -> bool {
    s.len() >= 4 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn resolve_commit(db: &Path, url: &str, reference: &GitRef) -> Result<String> {
    let rev_parse = |spec: &str| run_git(Some(db), &["rev-parse", "--verify", "--quiet", spec]);
    let missing = |what: String| GitError::NoSuchRef {
        url: url.to_string(),
        what,
    };
    match reference {
        GitRef::Branch(b) => rev_parse(&format!("refs/heads/{b}^{{commit}}"))
            .map_err(|_| missing(format!("branch `{b}`"))),
        GitRef::Tag(t) => rev_parse(&format!("refs/tags/{t}^{{commit}}"))
            .map_err(|_| missing(format!("tag `{t}`"))),
        GitRef::Rev(r) => {
            if let Ok(c) = rev_parse(&format!("{r}^{{commit}}")) {
                return Ok(c);
            }
            // Not reachable from a branch or tag: try fetching it directly.
            run_git(Some(db), &["fetch", "--force", "--quiet", url, r])
                .and_then(|_| rev_parse("FETCH_HEAD^{commit}"))
                .map_err(|_| missing(format!("revision `{r}`")))
        }
        GitRef::DefaultBranch => {
            // Ask the remote what its HEAD is, then use our copy of that branch.
            let listing = run_git(None, &["ls-remote", "--symref", url, "HEAD"])?;
            let branch = listing
                .lines()
                .find_map(|l| l.strip_prefix("ref: refs/heads/"))
                .and_then(|l| l.split_whitespace().next())
                .map(str::to_string);
            match branch {
                Some(b) => rev_parse(&format!("refs/heads/{b}^{{commit}}"))
                    .map_err(|_| missing(format!("default branch `{b}`"))),
                None => {
                    rev_parse("HEAD^{commit}").map_err(|_| missing("the default branch".into()))
                }
            }
        }
    }
}

/// Makes `dir` a checkout of `commit` (a no-op if it already is one) and
/// returns the state of its submodules.
fn materialize(db: &Path, dir: &Path, commit: &str) -> Result<String> {
    let marker = dir.join(".rune-git-identity");
    if let Ok(text) = std::fs::read_to_string(&marker) {
        let mut lines = text.lines();
        if lines.next() == Some(commit) {
            return Ok(lines.next().unwrap_or("").to_string());
        }
    }
    let parent = dir.parent().expect("checkout dir has a parent");
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".{}.{}.tmp", commit, std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    let staging_s = staging.to_string_lossy().into_owned();
    let db_s = db.to_string_lossy().into_owned();
    let result = (|| -> Result<String> {
        run_git(
            None,
            &[
                "clone",
                "--quiet",
                "--local",
                "--no-checkout",
                &db_s,
                &staging_s,
            ],
        )?;
        run_git(
            Some(&staging),
            &["checkout", "--quiet", "--force", "--detach", commit],
        )?;
        // Submodules are part of the source: a package can `include!` from
        // them or build C code out of them.
        if staging.join(".gitmodules").is_file() {
            run_git(
                Some(&staging),
                &["submodule", "update", "--init", "--recursive", "--quiet"],
            )?;
        }
        let status =
            run_git(Some(&staging), &["submodule", "status", "--recursive"]).unwrap_or_default();
        let mut subs: Vec<String> = status
            .lines()
            .filter_map(|l| {
                let l = l.trim_start_matches([' ', '+', '-', 'U']);
                let mut parts = l.split_whitespace();
                let sha = parts.next()?;
                let path = parts.next()?;
                Some(format!("{path}={sha}"))
            })
            .collect();
        subs.sort();
        let submodules = subs.join(",");
        std::fs::write(
            staging.join(".rune-git-identity"),
            format!("{commit}\n{submodules}\n"),
        )?;
        Ok(submodules)
    })();
    match result {
        Ok(subs) => {
            if std::fs::rename(&staging, dir).is_err() && !dir.join(".rune-git-identity").is_file()
            {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(GitError::Io(std::io::Error::other(format!(
                    "could not move the checkout of {commit} into place"
                ))));
            }
            // Lost a race to another process: theirs is complete, ours is spare.
            let _ = std::fs::remove_dir_all(&staging);
            Ok(subs)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

/// The directory of the package called `name` inside a checkout: the
/// repository root if that is it, else the shallowest `Cargo.toml` (in path
/// order) that declares it — like Cargo, which searches the whole repository.
pub fn find_package(checkout: &GitCheckout, name: &str) -> Result<PathBuf> {
    let declares = |dir: &Path| -> bool {
        std::fs::read_to_string(dir.join("Cargo.toml"))
            .ok()
            .and_then(|t| t.parse::<toml::Table>().ok())
            .and_then(|t| {
                t.get("package")?
                    .as_table()?
                    .get("name")?
                    .as_str()
                    .map(|n| n == name)
            })
            .unwrap_or(false)
    };
    if declares(&checkout.dir) {
        return Ok(checkout.dir.clone());
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut stack = vec![(checkout.dir.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 8 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let fname = e.file_name();
            let fname = fname.to_string_lossy();
            if !path.is_dir() || fname == ".git" || fname == "target" {
                continue;
            }
            if declares(&path) {
                candidates.push(path.clone());
            }
            stack.push((path, depth + 1));
        }
    }
    candidates.sort_by_key(|p| (p.components().count(), p.clone()));
    candidates.into_iter().next().ok_or(GitError::NoPackage {
        url: checkout.url.clone(),
        name: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_sources_round_trip() {
        let c = GitCheckout {
            url: "https://example.com/a/b.git".into(),
            reference: GitRef::Branch("next".into()),
            commit: "a".repeat(40),
            dir: PathBuf::new(),
            submodules: String::new(),
        };
        let src = c.lock_source();
        assert_eq!(
            src,
            format!(
                "git+https://example.com/a/b.git?branch=next#{}",
                "a".repeat(40)
            )
        );
        let (url, q, commit) = parse_lock_source(&src).unwrap();
        assert_eq!(url, "https://example.com/a/b.git");
        assert_eq!(q.as_deref(), Some("branch=next"));
        assert_eq!(commit, "a".repeat(40));
        let (_, q, _) = parse_lock_source("git+https://x/y#abc").unwrap();
        assert_eq!(q, None);
    }

    #[test]
    fn references_from_specs() {
        assert_eq!(GitRef::from_spec(None, None, None), GitRef::DefaultBranch);
        assert_eq!(
            GitRef::from_spec(Some("b"), None, None),
            GitRef::Branch("b".into())
        );
        assert_eq!(
            GitRef::from_spec(None, Some("t"), None),
            GitRef::Tag("t".into())
        );
        assert_eq!(
            GitRef::from_spec(Some("b"), None, Some("r")),
            GitRef::Rev("r".into()),
            "an exact revision wins over a branch"
        );
    }

    #[test]
    fn repo_keys_differ_per_url_and_keep_a_readable_name() {
        let a = repo_key("https://github.com/o/foo.git");
        let b = repo_key("https://gitlab.com/o/foo.git");
        assert!(a.starts_with("foo-"));
        assert_ne!(a, b);
    }
}
