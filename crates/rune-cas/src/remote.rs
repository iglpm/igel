//! A shared store behind the local one.
//!
//! Point `cas.remote` at something several machines can reach and a build first
//! looks there before compiling: an entry another machine already built is
//! copied into the local store and used exactly like a local one. New entries are
//! published back (unless `cas.remote-write = false`, the usual setting for
//! developers when only CI writes).
//!
//! Two kinds of shared store speak the same protocol — Rune's own layout:
//!
//! * a **directory** (a network share, a CI cache volume, a synced folder);
//! * one or more **Cairn servers** (`http://host:7070`), with a bearer token,
//!   optional sharding across servers, failover and a circuit breaker so a dead
//!   server costs one connection attempt, not one per unit.
//!
//! Each *slot* of an entry — its compiled artifacts, a stored value, a directory
//! tree — is copied file by file, and the slot's marker file (`manifest.json`,
//! `value.json`, `tree.json`) goes last: a reader either sees a complete slot or
//! none, however many machines write at once. Entries are immutable, so whoever
//! publishes first wins and everyone else skips.
//!
//! **Trust.** Keys identify what went into an artifact, not what came out, and a
//! pulled entry is checked only against its own manifest. Share a store only
//! between machines you would run each other's binaries on.

use serde::Deserialize;
use std::fmt::Debug;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Which part of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Compiled artifacts (`manifest.json` + the files it lists).
    Artifacts,
    /// One small JSON value (`value.json`).
    Value,
    /// A directory tree (`tree.json` + `tree/…`).
    Tree,
}

impl Slot {
    fn marker(self) -> &'static str {
        match self {
            Slot::Artifacts => "manifest.json",
            Slot::Value => "value.json",
            Slot::Tree => "tree.json",
        }
    }
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

/// Where shared entries live.
trait Transport: Send + Sync + Debug {
    /// A small file's bytes (markers). `Ok(None)`: it is not there.
    fn fetch(&self, key: &str, rel: &str) -> io::Result<Option<Vec<u8>>>;
    /// Writes a file atomically (read-only) to `dest`. `Ok(false)`: not there.
    fn fetch_to(&self, key: &str, rel: &str, dest: &Path) -> io::Result<bool>;
    fn exists(&self, key: &str, rel: &str) -> bool;
    /// Publishes `src` as `key/rel` unless it exists. `blake3` (hex), when
    /// known, lets a server verify the bytes it receives.
    fn store(&self, key: &str, rel: &str, src: &Path, blake3: Option<&str>) -> io::Result<()>;
    fn describe(&self) -> String;
}

/// Same sharding as the local store.
fn object_dir(root: &Path, key: &str) -> PathBuf {
    let prefix = if key.len() >= 2 { &key[0..2] } else { key };
    root.join("objects").join(prefix).join(key)
}

/// A scratch file next to `dst`, to be written and then [`finish_atomic`]ed.
fn begin_atomic(dst: &Path) -> io::Result<(PathBuf, fs::File)> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dst.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let file = fs::File::create(&tmp)?;
    Ok((tmp, file))
}

/// Makes a finished scratch file read-only (executable if `exec`) and renames
/// it into place — readers see the old file or the new one, never half.
fn finish_atomic(tmp: &Path, dst: &Path, exec: bool) -> io::Result<()> {
    let result = (|| {
        set_read_only(tmp, exec)?;
        fs::rename(tmp, dst)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

/// Runs `write` on a scratch file next to `dst`, then publishes it atomically.
///
/// `exec`: keep the file executable (a compiled binary must stay runnable).
fn atomic_write(dst: &Path, exec: bool, write: impl FnOnce(&mut fs::File) -> io::Result<()>) -> io::Result<()> {
    let (tmp, mut file) = begin_atomic(dst)?;
    let written = write(&mut file);
    drop(file);
    match written {
        Ok(()) => finish_atomic(&tmp, dst, exec),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Read-only, with the execute bits if `exec` (`0555` / `0444`).
fn set_read_only(path: &Path, exec: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(if exec { 0o555 } else { 0o444 }))
    }
    #[cfg(not(unix))]
    {
        let _ = exec;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms)
    }
}

/// Whether a file has any execute bit.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

#[derive(Debug)]
struct DirTransport {
    root: PathBuf,
}

impl Transport for DirTransport {
    fn fetch(&self, key: &str, rel: &str) -> io::Result<Option<Vec<u8>>> {
        match fs::read(object_dir(&self.root, key).join(rel)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn fetch_to(&self, key: &str, rel: &str, dest: &Path) -> io::Result<bool> {
        let src = object_dir(&self.root, key).join(rel);
        match fs::File::open(&src) {
            Ok(mut from) => {
                let exec = is_executable(&src);
                atomic_write(dest, exec, |to| io::copy(&mut from, to).map(|_| ())).map(|_| true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn exists(&self, key: &str, rel: &str) -> bool {
        object_dir(&self.root, key).join(rel).is_file()
    }

    fn store(&self, key: &str, rel: &str, src: &Path, _blake3: Option<&str>) -> io::Result<()> {
        let dst = object_dir(&self.root, key).join(rel);
        if dst.is_file() {
            return Ok(());
        }
        let mut from = fs::File::open(src)?;
        atomic_write(&dst, is_executable(src), |to| io::copy(&mut from, to).map(|_| ()))
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }
}

/// One Cairn server.
#[derive(Debug)]
struct Endpoint {
    base: reqwest::Url,
    /// Unix milliseconds until which this server is skipped after a connection failure.
    down_until: AtomicU64,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// How long a server that refused or timed out a connection is left alone.
const DOWN_FOR_MS: u64 = 30_000;

/// One or more Cairn servers.
///
/// With several, each key is placed by rendezvous hashing — every client
/// computes the same ordering of servers for a key without any coordination —
/// and lives on the first `replicas` servers that are up. Reads try those in
/// order; a server that cannot be reached is skipped for half a minute.
///
/// Requests run on a small private multi-thread runtime and are driven from
/// Rune's synchronous, multi-threaded code with `block_on`; unlike
/// `reqwest::blocking` (one I/O thread for everything) this scales with the
/// number of restoring threads.
#[derive(Debug)]
struct HttpTransport {
    endpoints: Vec<Endpoint>,
    replicas: usize,
    token: Option<String>,
    rt: tokio::runtime::Runtime,
    client: reqwest::Client,
    warned_auth: AtomicBool,
}

/// What one attempt against one server amounted to.
enum Attempt<T> {
    /// The server answered; `Some` settles the request.
    Answered(Option<T>),
    /// It could not be reached: skip it for a while.
    Down,
    /// It was reached but the exchange failed.
    Failed,
}

impl HttpTransport {
    fn new(urls: &[String], token: Option<String>, replicas: usize) -> Result<Self, String> {
        let mut endpoints = Vec::new();
        for u in urls {
            let base = reqwest::Url::parse(u.trim_end_matches('/'))
                .map_err(|e| format!("cas.remote `{u}`: {e}"))?;
            if base.cannot_be_a_base() {
                return Err(format!("cas.remote `{u}` is not a usable URL"));
            }
            endpoints.push(Endpoint { base, down_until: AtomicU64::new(0) });
        }
        if endpoints.is_empty() {
            return Err("cas.remote lists no servers".into());
        }
        let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 8);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("rune-shared-store")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(300))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(32)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            endpoints,
            replicas: replicas.clamp(1, urls.len()),
            token,
            rt,
            client,
            warned_auth: AtomicBool::new(false),
        })
    }

    /// Server indices in rendezvous order for `key` (highest score first).
    fn order(&self, key: &str) -> Vec<usize> {
        let mut scored: Vec<(u64, usize)> = self
            .endpoints
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let h = blake3::hash(format!("{}\0{key}", e.base).as_bytes());
                (u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap()), i)
            })
            .collect();
        scored.sort_by(|a, b| b.cmp(a));
        scored.into_iter().map(|(_, i)| i).collect()
    }

    fn is_up(&self, i: usize) -> bool {
        self.endpoints[i].down_until.load(Ordering::Relaxed) <= now_ms()
    }

    fn mark_down(&self, i: usize) {
        self.endpoints[i].down_until.store(now_ms() + DOWN_FOR_MS, Ordering::Relaxed);
    }

    fn url(&self, i: usize, key: &str, rel: &str) -> reqwest::Url {
        let mut url = self.endpoints[i].base.clone();
        {
            let mut segs = url.path_segments_mut().expect("checked in new");
            segs.pop_if_empty().extend(["v1", "o", key]).extend(rel.split('/'));
        }
        url
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    fn note_status(&self, i: usize, status: reqwest::StatusCode) {
        if (status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN)
            && !self.warned_auth.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                "the shared store {} refused this machine's credentials ({status}); \
                 set cas.remote-token (or RUNE_CAS_REMOTE_TOKEN)",
                self.endpoints[i].base
            );
        }
    }

    fn classify(&self, i: usize, e: &reqwest::Error) -> bool {
        tracing::debug!("shared store {}: {e}", self.endpoints[i].base);
        let down = e.is_connect() || e.is_timeout();
        if down {
            self.mark_down(i);
        }
        down
    }

    /// Runs `attempt` against the servers holding `key`, in order, until one
    /// settles the request or `replicas` servers have answered.
    async fn try_servers<T, F, Fut>(&self, key: &str, mut attempt: F) -> Option<T>
    where
        F: FnMut(usize) -> Fut,
        Fut: std::future::Future<Output = Attempt<T>>,
    {
        let mut answered = 0;
        for i in self.order(key) {
            if answered >= self.replicas {
                break;
            }
            if !self.is_up(i) {
                continue;
            }
            match attempt(i).await {
                Attempt::Answered(Some(v)) => return Some(v),
                Attempt::Answered(None) | Attempt::Failed => answered += 1,
                Attempt::Down => {}
            }
        }
        None
    }
}

impl Transport for HttpTransport {
    fn fetch(&self, key: &str, rel: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(self.rt.block_on(self.try_servers(key, |i| async move {
            match self.auth(self.client.get(self.url(i, key, rel))).send().await {
                Ok(resp) => {
                    self.note_status(i, resp.status());
                    if !resp.status().is_success() {
                        return Attempt::Answered(None);
                    }
                    match resp.bytes().await {
                        Ok(b) => Attempt::Answered(Some(b.to_vec())),
                        Err(_) => Attempt::Failed,
                    }
                }
                Err(e) if self.classify(i, &e) => Attempt::Down,
                Err(_) => Attempt::Failed,
            }
        })))
    }

    fn fetch_to(&self, key: &str, rel: &str, dest: &Path) -> io::Result<bool> {
        Ok(self
            .rt
            .block_on(self.try_servers(key, |i| async move {
                let mut resp = match self.auth(self.client.get(self.url(i, key, rel))).send().await {
                    Ok(r) => r,
                    Err(e) if self.classify(i, &e) => return Attempt::Down,
                    Err(_) => return Attempt::Failed,
                };
                self.note_status(i, resp.status());
                if !resp.status().is_success() {
                    return Attempt::Answered(None);
                }
                let expected = resp.content_length();
                let exec = resp.headers().get("x-cairn-exec").is_some_and(|v| v == "1");
                let Ok((tmp, mut file)) = begin_atomic(dest) else {
                    return Attempt::Failed;
                };
                let mut written = 0u64;
                let ok = loop {
                    match resp.chunk().await {
                        Ok(Some(chunk)) => {
                            written += chunk.len() as u64;
                            if io::Write::write_all(&mut file, &chunk).is_err() {
                                break false;
                            }
                        }
                        Ok(None) => break expected.is_none_or(|n| n == written),
                        Err(_) => break false,
                    }
                };
                drop(file);
                if ok && finish_atomic(&tmp, dest, exec).is_ok() {
                    Attempt::Answered(Some(()))
                } else {
                    let _ = fs::remove_file(&tmp);
                    Attempt::Failed
                }
            }))
            .is_some())
    }

    fn exists(&self, key: &str, rel: &str) -> bool {
        self.rt
            .block_on(self.try_servers(key, |i| async move {
                match self.auth(self.client.head(self.url(i, key, rel))).send().await {
                    Ok(resp) => {
                        self.note_status(i, resp.status());
                        Attempt::Answered(resp.status().is_success().then_some(()))
                    }
                    Err(e) if self.classify(i, &e) => Attempt::Down,
                    Err(_) => Attempt::Failed,
                }
            }))
            .is_some()
    }

    fn store(&self, key: &str, rel: &str, src: &Path, blake3: Option<&str>) -> io::Result<()> {
        let len = fs::metadata(src)?.len();
        let exec = is_executable(src);
        self.rt.block_on(async {
            let mut stored = 0usize;
            let mut last_err: Option<String> = None;
            for i in self.order(key) {
                if stored >= self.replicas {
                    break;
                }
                if !self.is_up(i) {
                    continue;
                }
                let file = tokio::fs::File::open(src).await?;
                let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024));
                let mut rb = self
                    .auth(self.client.put(self.url(i, key, rel)))
                    .header(reqwest::header::CONTENT_LENGTH, len)
                    .body(body);
                if let Some(h) = blake3.filter(|h| h.len() == 64) {
                    rb = rb.header("x-cairn-blake3", h);
                }
                if exec {
                    rb = rb.header("x-cairn-exec", "1");
                }
                match rb.send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        self.note_status(i, status);
                        // 409: the server already holds a different file under this
                        // immutable name — nothing more to do about it.
                        if status.is_success() || status == reqwest::StatusCode::CONFLICT {
                            stored += 1;
                        } else {
                            last_err = Some(format!("{} answered {status}", self.endpoints[i].base));
                        }
                    }
                    Err(e) => {
                        self.classify(i, &e);
                        last_err = Some(e.to_string());
                    }
                }
            }
            if stored > 0 {
                Ok(())
            } else {
                Err(io::Error::other(last_err.unwrap_or_else(|| "no shared-store server is reachable".into())))
            }
        })
    }

    fn describe(&self) -> String {
        self.endpoints.iter().map(|e| e.base.to_string()).collect::<Vec<_>>().join(", ")
    }
}

// ---------------------------------------------------------------------------
// Slots
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Listed {
    artifacts: Vec<ListedFile>,
}

#[derive(Deserialize)]
struct ListedFile {
    filename: String,
    size: u64,
    #[serde(default)]
    hash: String,
}

/// The files a marker names, relative to the entry, with the size (and BLAKE3
/// hash) the marker records for them, where it does.
fn listed(slot: Slot, marker: &[u8]) -> io::Result<Vec<(String, Option<u64>, String)>> {
    let bad = |e: serde_json::Error| io::Error::new(io::ErrorKind::InvalidData, e);
    Ok(match slot {
        Slot::Artifacts => serde_json::from_slice::<Listed>(marker)
            .map_err(bad)?
            .artifacts
            .into_iter()
            .map(|a| (a.filename, Some(a.size), a.hash))
            .collect(),
        Slot::Tree => serde_json::from_slice::<Vec<String>>(marker)
            .map_err(bad)?
            .into_iter()
            .map(|r| (format!("tree/{r}"), None, String::new()))
            .collect(),
        Slot::Value => Vec::new(),
    })
}

/// Copies one slot from the shared store into the local object directory,
/// marker last. Does nothing if the local store already has the slot.
fn pull_slot(t: &dyn Transport, key: &str, local: &Path, slot: Slot) -> io::Result<bool> {
    if local.join(slot.marker()).is_file() {
        return Ok(true);
    }
    let Some(marker) = t.fetch(key, slot.marker())? else {
        return Ok(false);
    };
    for (rel, size, _) in listed(slot, &marker)? {
        let dest = local.join(&rel);
        if !t.fetch_to(key, &rel, &dest)? {
            return Ok(false);
        }
        // A file that is not what the manifest says is a broken entry: do not keep it.
        if let Some(size) = size {
            if fs::metadata(&dest)?.len() != size {
                let _ = fs::remove_file(&dest);
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{rel} does not match its manifest")));
            }
        }
    }
    atomic_write(&local.join(slot.marker()), false, |f| io::Write::write_all(f, &marker))?;
    Ok(true)
}

/// Publishes one local slot to the shared store, marker last. Does nothing if
/// the shared store already has it.
fn push_slot(t: &dyn Transport, key: &str, local: &Path, slot: Slot) -> io::Result<()> {
    if t.exists(key, slot.marker()) {
        return Ok(());
    }
    let mut marker = Vec::new();
    fs::File::open(local.join(slot.marker()))?.read_to_end(&mut marker)?;
    for (rel, _, hash) in listed(slot, &marker)? {
        t.store(key, &rel, &local.join(&rel), (hash.len() == 64).then_some(hash.as_str()))?;
    }
    t.store(key, slot.marker(), &local.join(slot.marker()), None)
}

// ---------------------------------------------------------------------------
// Remote
// ---------------------------------------------------------------------------

/// Where a store's shared layer is and how to reach it.
#[derive(Debug, Clone)]
pub struct Remote {
    transport: Arc<dyn Transport>,
    write: bool,
    root: Option<PathBuf>,
}

/// Publishes in the background, so copying artifacts to the shared store never
/// holds up a compiler slot. [`Publisher::flush`] waits for the queue to drain;
/// anything still queued when a process dies is simply not published (a slot is
/// only visible once its marker file is in place).
#[derive(Debug)]
pub struct Publisher {
    tx: std::sync::Mutex<std::sync::mpsc::Sender<(Remote, String, PathBuf, Slot)>>,
    outstanding: std::sync::Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>,
}

impl Publisher {
    /// Starts `threads` background workers.
    pub fn start(threads: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(Remote, String, PathBuf, Slot)>();
        let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
        let outstanding = std::sync::Arc::new((std::sync::Mutex::new(0usize), std::sync::Condvar::new()));
        for _ in 0..threads.max(1) {
            let (rx, outstanding) = (rx.clone(), outstanding.clone());
            std::thread::spawn(move || {
                loop {
                    let job = rx.lock().unwrap().recv();
                    let Ok((remote, key, local, slot)) = job else { return };
                    remote.push(&key, &local, slot);
                    let (count, cv) = &*outstanding;
                    *count.lock().unwrap() -= 1;
                    cv.notify_all();
                }
            });
        }
        Self {
            tx: std::sync::Mutex::new(tx),
            outstanding,
        }
    }

    pub fn submit(&self, remote: Remote, key: String, local: PathBuf, slot: Slot) {
        *self.outstanding.0.lock().unwrap() += 1;
        if self.tx.lock().unwrap().send((remote, key, local, slot)).is_err() {
            *self.outstanding.0.lock().unwrap() -= 1;
        }
    }

    /// Waits until everything submitted so far has been published (or failed).
    pub fn flush(&self) {
        let (count, cv) = &*self.outstanding;
        let mut n = count.lock().unwrap();
        while *n > 0 {
            n = cv.wait(n).unwrap();
        }
    }
}

impl Remote {
    /// A shared directory.
    pub fn new(root: PathBuf, write: bool) -> Self {
        Self {
            transport: Arc::new(DirTransport { root: root.clone() }),
            write,
            root: Some(root),
        }
    }

    /// One or more Cairn servers.
    pub fn http(urls: &[String], token: Option<String>, replicas: usize, write: bool) -> Result<Self, String> {
        Ok(Self {
            transport: Arc::new(HttpTransport::new(urls, token, replicas)?),
            write,
            root: None,
        })
    }

    /// Parses a `cas.remote` value: a directory, or `http(s)://…` URLs separated
    /// by commas or spaces.
    pub fn from_spec(spec: &str, write: bool, token: Option<String>, replicas: usize) -> Result<Self, String> {
        let trimmed = spec.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            let urls: Vec<String> = trimmed
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            Self::http(&urls, token, replicas, write)
        } else {
            Ok(Self::new(PathBuf::from(trimmed), write))
        }
    }

    /// The directory, for a directory-backed store.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Human-readable location.
    pub fn describe(&self) -> String {
        self.transport.describe()
    }

    /// Whether this store publishes.
    pub fn writes(&self) -> bool {
        self.write
    }

    /// Copies a slot the shared store has into the local object directory.
    /// Returns whether the local store now has it. Failures are not errors: a
    /// slow, missing or damaged remote is just a miss.
    pub fn pull(&self, key: &str, local: &Path, slot: Slot) -> bool {
        match pull_slot(&*self.transport, key, local, slot) {
            Ok(found) => found,
            Err(e) => {
                tracing::debug!("shared store: could not pull {key}: {e}");
                false
            }
        }
    }

    /// Publishes a local slot (best effort; only if writing is enabled).
    pub fn push(&self, key: &str, local: &Path, slot: Slot) {
        if !self.write {
            return;
        }
        if let Err(e) = push_slot(&*self.transport, key, local, slot) {
            tracing::debug!("shared store: could not publish {key}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::store::CasStore;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::{TempDir, tempdir};

    struct Pair {
        _dirs: Vec<TempDir>,
        remote: PathBuf,
        a: CasStore,
        b: CasStore,
    }

    /// Two machines (two local stores) sharing one remote directory.
    fn pair(a_writes: bool) -> Pair {
        let (ra, la, lb) = (tempdir().unwrap(), tempdir().unwrap(), tempdir().unwrap());
        let remote = ra.path().to_path_buf();
        let a = CasStore::new(la.path().to_path_buf())
            .unwrap()
            .with_remote(remote.clone(), a_writes);
        let b = CasStore::new(lb.path().to_path_buf())
            .unwrap()
            .with_remote(remote.clone(), false);
        Pair { _dirs: vec![ra, la, lb], remote, a, b }
    }

    fn artifact(name: &str, content: &str) -> (TempDir, PathBuf) {
        let d = tempdir().unwrap();
        let p = d.path().join(name);
        fs::write(&p, content).unwrap();
        (d, p)
    }

    #[test]
    fn what_one_machine_builds_another_restores() {
        let p = pair(true);
        let (_d, lib) = artifact("libfoo-abc.rlib", "compiled bytes");
        p.a.put("keyfoo", "foo", "1.0.0", &[lib]).unwrap();
        p.a.flush_remote();

        // B has never seen it; it comes from the shared store, verified and usable.
        let out = tempdir().unwrap();
        let linked = p.b.link_to("keyfoo", out.path()).unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(fs::read_to_string(&linked[0]).unwrap(), "compiled bytes");
        assert!(p.b.contains("keyfoo"), "and it is now local");
    }

    #[cfg(unix)]
    #[test]
    fn a_compiled_binary_is_still_executable_after_a_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        let p = pair(true);
        let (_d, bin) = artifact("mybin-abc123", "\x7fELF pretend");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let (_d2, rlib) = artifact("libmy-abc123.rlib", "rlib");
        p.a.put("keybin", "my", "1.0.0", &[bin, rlib]).unwrap();
        p.a.flush_remote();
        let out = tempdir().unwrap();
        let linked = p.b.link_to("keybin", out.path()).unwrap();
        let mode = |name: &str| {
            let f = linked.iter().find(|f| f.file_name().unwrap() == name).unwrap();
            fs::metadata(f).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode("mybin-abc123"), 0o555, "runnable, read-only");
        assert_eq!(mode("libmy-abc123.rlib"), 0o444, "not executable");
    }

    #[test]
    fn values_and_directory_trees_are_shared_too() {
        let p = pair(true);
        p.a.put_value("keyval", &vec!["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(p.b.get_value::<Vec<String>>("keyval"), Some(vec!["a".into(), "b".into()]));

        let tree = tempdir().unwrap();
        fs::create_dir_all(tree.path().join("sub")).unwrap();
        fs::write(tree.path().join("gen.rs"), "const A: u8 = 1;").unwrap();
        fs::write(tree.path().join("sub/x.txt"), "x").unwrap();
        p.a.put_dir_tree("keytree", tree.path()).unwrap();
        p.a.flush_remote();
        let restored = tempdir().unwrap();
        assert!(p.b.restore_dir_tree("keytree", restored.path()).unwrap());
        assert_eq!(fs::read_to_string(restored.path().join("gen.rs")).unwrap(), "const A: u8 = 1;");
        assert_eq!(fs::read_to_string(restored.path().join("sub/x.txt")).unwrap(), "x");
    }

    #[test]
    fn a_read_only_remote_is_never_written() {
        let p = pair(false);
        let (_d, lib) = artifact("libfoo-abc.rlib", "x");
        p.a.put("keyro", "foo", "1.0.0", &[lib]).unwrap();
        p.a.put_value("keyro2", &1u32).unwrap();
        assert!(!p.remote.join("objects").exists(), "nothing was published");
        assert!(p.b.get("keyro").is_none());
    }

    #[test]
    fn a_missing_or_unreachable_remote_is_just_a_miss() {
        let l = tempdir().unwrap();
        let store = CasStore::new(l.path().to_path_buf())
            .unwrap()
            .with_remote(PathBuf::from("/definitely/not/mounted"), true);
        assert!(store.get("nokey").is_none());
        let (_d, lib) = artifact("libfoo-abc.rlib", "x");
        store.put("nokey", "foo", "1.0.0", &[lib]).unwrap(); // still stores locally
        assert!(store.get("nokey").is_some());
    }

    #[test]
    fn an_incomplete_remote_entry_is_invisible() {
        let p = pair(true);
        let (_d, lib) = artifact("libfoo-abc.rlib", "bytes");
        p.a.put("keyhalf", "foo", "1.0.0", &[lib]).unwrap();
        p.a.flush_remote();
        // Simulate a publisher that died before writing its marker.
        let dir = p.remote.join("objects").join("ke").join("keyhalf");
        fs::remove_file(dir.join("manifest.json")).unwrap();
        assert!(p.b.get("keyhalf").is_none());
    }

    #[test]
    fn a_damaged_remote_entry_is_not_spread() {
        let p = pair(true);
        let (_d, lib) = artifact("libfoo-abc.rlib", "the right content");
        p.a.put("keybad", "foo", "1.0.0", &[lib]).unwrap();
        p.a.flush_remote();
        let dir = p.remote.join("objects").join("ke").join("keybad");
        let file = dir.join("libfoo-abc.rlib");
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::write(&file, "truncated").unwrap();
        assert!(p.b.get("keybad").is_none(), "size no longer matches the manifest");
        assert!(!p.b.contains("keybad"));
    }

    #[test]
    fn the_first_publisher_wins_and_entries_are_immutable() {
        let p = pair(true);
        let (_d1, one) = artifact("libfoo-abc.rlib", "first");
        let (_d2, two) = artifact("libfoo-abc.rlib", "second!");
        p.a.put("keyimm", "foo", "1.0.0", &[one]).unwrap();
        p.a.flush_remote();
        // Another writer for the same key (a racing CI job) does not replace it.
        let l = tempdir().unwrap();
        let other = CasStore::new(l.path().to_path_buf()).unwrap().with_remote(p.remote.clone(), true);
        other.put("keyimm", "foo", "1.0.0", &[two]).unwrap();
        other.flush_remote();
        let out = tempdir().unwrap();
        let linked = p.b.link_to("keyimm", out.path()).unwrap();
        assert_eq!(fs::read_to_string(&linked[0]).unwrap(), "first");
    }

    #[test]
    fn an_existing_local_store_seeds_the_remote() {
        let l = tempdir().unwrap();
        let (ra, lb) = (tempdir().unwrap(), tempdir().unwrap());
        let plain = CasStore::new(l.path().to_path_buf()).unwrap();
        let (_d, lib) = artifact("libfoo-abc.rlib", "seed");
        plain.put("keyseed", "foo", "1.0.0", std::slice::from_ref(&lib)).unwrap();
        // Same local store, now configured with a remote: storing again publishes.
        let with = CasStore::new(l.path().to_path_buf()).unwrap().with_remote(ra.path().to_path_buf(), true);
        with.put("keyseed", "foo", "1.0.0", &[lib]).unwrap();
        with.flush_remote();
        let reader = CasStore::new(lb.path().to_path_buf()).unwrap().with_remote(ra.path().to_path_buf(), false);
        assert!(reader.get("keyseed").is_some());
    }

    #[test]
    fn flushing_stats_waits_for_background_publishing() {
        let p = pair(true);
        for i in 0..20 {
            let (_d, lib) = artifact("libbulk-abc.rlib", &format!("payload {i}").repeat(1000));
            p.a.put(&format!("keybulk{i:02}"), "bulk", "1.0.0", &[lib]).unwrap();
        }
        p.a.flush_stats(); // what the executor calls at the end of a build
        for i in 0..20 {
            assert!(p.b.get(&format!("keybulk{i:02}")).is_some(), "entry {i} was not published");
        }
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::store::CasStore;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;
    use tempfile::{TempDir, tempdir};

    type Files = Arc<Mutex<HashMap<String, Vec<u8>>>>;
    type PutLog = Arc<Mutex<Vec<(String, Option<String>)>>>;
    type NameSet = Arc<Mutex<std::collections::HashSet<String>>>;

    /// A tiny in-memory stand-in for a Cairn server (GET/HEAD/PUT of
    /// `/v1/o/<key>/<file…>`, optional bearer token, immutable files).
    struct Fake {
        url: String,
        files: Files,
        put_headers: PutLog,
        exec: NameSet,
        requests: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn fake(token: Option<&'static str>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let files: Files = Default::default();
        let put_headers: PutLog = Default::default();
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let exec: NameSet = Default::default();
        let (f2, h2, r2, s2, e2) = (files.clone(), put_headers.clone(), requests.clone(), stop.clone(), exec.clone());
        std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (f, h, r, s, e) = (f2.clone(), h2.clone(), r2.clone(), s2.clone(), e2.clone());
                        std::thread::spawn(move || serve(stream, token, f, h, r, s, e));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Fake { url, files, put_headers, exec, requests, stop }
    }

    fn serve(
        stream: TcpStream,
        token: Option<&'static str>,
        files: Files,
        put_headers: PutLog,
        requests: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        exec: NameSet,
    ) {
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(200))).ok();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut out = stream;
        while !stop.load(Ordering::Relaxed) {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {}
                Err(_) => continue,
            }
            let mut parts = line.split_whitespace();
            let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
            let mut headers: HashMap<String, String> = HashMap::new();
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            requests.fetch_add(1, Ordering::Relaxed);

            let authorised = token.is_none_or(|t| headers.get("authorization").map(String::as_str) == Some(&format!("Bearer {t}")));
            let name = path.strip_prefix("/v1/o/").unwrap_or("").to_string();
            let (status, payload): (&str, Vec<u8>) = if !authorised {
                ("401 Unauthorized", Vec::new())
            } else {
                match method.as_str() {
                    "PUT" => {
                        put_headers.lock().unwrap().push((name.clone(), headers.get("x-cairn-blake3").cloned()));
                        let mut f = files.lock().unwrap();
                        if f.contains_key(&name) {
                            ("200 OK", Vec::new())
                        } else {
                            if headers.get("x-cairn-exec").is_some_and(|v| v == "1") {
                                exec.lock().unwrap().insert(name.clone());
                            }
                            f.insert(name.clone(), body);
                            ("201 Created", Vec::new())
                        }
                    }
                    _ => match files.lock().unwrap().get(&name) {
                        Some(b) => ("200 OK", if method == "HEAD" { Vec::new() } else { b.clone() }),
                        None => ("404 Not Found", Vec::new()),
                    },
                }
            };
            let claimed = if method == "HEAD" && status == "200 OK" {
                files.lock().unwrap().get(&name).map_or(0, |b| b.len())
            } else {
                payload.len()
            };
            let exec_line = if exec.lock().unwrap().contains(&name) { "x-cairn-exec: 1\r\n" } else { "" };
            write!(out, "HTTP/1.1 {status}\r\ncontent-length: {claimed}\r\n{exec_line}\r\n").unwrap();
            out.write_all(&payload).unwrap();
        }
    }

    fn machine(urls: &[&str], token: Option<&str>, replicas: usize, write: bool) -> (TempDir, CasStore) {
        let dir = tempdir().unwrap();
        let remote = Remote::http(
            &urls.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
            token.map(str::to_string),
            replicas,
            write,
        )
        .unwrap();
        let store = CasStore::new(dir.path().to_path_buf()).unwrap().with_remote_store(remote);
        (dir, store)
    }

    fn artifact(name: &str, content: &str) -> (TempDir, PathBuf) {
        let d = tempdir().unwrap();
        let p = d.path().join(name);
        fs::write(&p, content).unwrap();
        (d, p)
    }

    #[test]
    fn what_one_machine_builds_another_restores_over_http() {
        let server = fake(None);
        let (_da, a) = machine(&[&server.url], None, 1, true);
        let (_db, b) = machine(&[&server.url], None, 1, false);
        let (_d, lib) = artifact("libfoo-abc.rlib", "compiled bytes");
        a.put("keyfoo", "foo", "1.0.0", &[lib]).unwrap();
        a.put_value("keyval", &vec![1, 2, 3]).unwrap();
        a.flush_remote();

        let out = tempdir().unwrap();
        let linked = b.link_to("keyfoo", out.path()).unwrap();
        assert_eq!(fs::read_to_string(&linked[0]).unwrap(), "compiled bytes");
        assert_eq!(b.get_value::<Vec<u32>>("keyval"), Some(vec![1, 2, 3]));
        // Files first, marker last: the manifest is the final PUT for the key.
        assert!(server.requests.load(Ordering::Relaxed) > 0);
        let names: Vec<String> = server.put_headers.lock().unwrap().iter().map(|(n, _)| n.clone()).collect();
        let last = names.iter().rposition(|n| n.starts_with("keyfoo/")).unwrap();
        assert_eq!(names[last], "keyfoo/manifest.json", "{names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_compiled_binary_is_still_executable_after_a_round_trip_over_http() {
        use std::os::unix::fs::PermissionsExt;
        let server = fake(None);
        let (_da, a) = machine(&[&server.url], None, 1, true);
        let (_db, b) = machine(&[&server.url], None, 1, false);
        let (_d, bin) = artifact("mybin-abc123", "\x7fELF pretend");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let (_d2, rlib) = artifact("libmy-abc123.rlib", "rlib");
        a.put("keybin", "my", "1.0.0", &[bin, rlib]).unwrap();
        a.flush_remote();
        assert!(server.exec.lock().unwrap().contains("keybin/mybin-abc123"), "the client announced it");
        assert!(!server.exec.lock().unwrap().contains("keybin/libmy-abc123.rlib"));
        let out = tempdir().unwrap();
        let linked = b.link_to("keybin", out.path()).unwrap();
        let mode = |name: &str| {
            let f = linked.iter().find(|f| f.file_name().unwrap() == name).unwrap();
            fs::metadata(f).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode("mybin-abc123"), 0o555);
        assert_eq!(mode("libmy-abc123.rlib"), 0o444);
    }

    #[test]
    fn artifact_uploads_announce_their_blake3_hash() {
        let server = fake(None);
        let (_d, a) = machine(&[&server.url], None, 1, true);
        let (_f, lib) = artifact("libfoo-abc.rlib", "verify me");
        a.put("keyhash", "foo", "1.0.0", &[lib]).unwrap();
        a.flush_remote();
        let want = blake3::hash(b"verify me").to_hex().to_string();
        let sent = server.put_headers.lock().unwrap().iter().find(|(n, _)| n == "keyhash/libfoo-abc.rlib").cloned().unwrap();
        assert_eq!(sent.1.as_deref(), Some(want.as_str()));
    }

    #[test]
    fn the_bearer_token_is_sent_and_required() {
        let server = fake(Some("s3cret"));
        let (_f, lib) = artifact("libfoo-abc.rlib", "x");
        // Wrong or missing token: publishing silently fails, nothing is stored.
        let (_d1, nope) = machine(&[&server.url], None, 1, true);
        nope.put("keyauth", "foo", "1.0.0", std::slice::from_ref(&lib)).unwrap();
        nope.flush_remote();
        assert!(server.files.lock().unwrap().is_empty());
        // The right token works, and a reader without it simply misses.
        let (_d2, good) = machine(&[&server.url], Some("s3cret"), 1, true);
        good.put("keyauth", "foo", "1.0.0", &[lib]).unwrap();
        good.flush_remote();
        assert!(!server.files.lock().unwrap().is_empty());
        let (_d3, anon) = machine(&[&server.url], None, 1, false);
        assert!(anon.get("keyauth").is_none());
        let (_d4, reader) = machine(&[&server.url], Some("s3cret"), 1, false);
        assert!(reader.get("keyauth").is_some());
    }

    #[test]
    fn a_read_only_client_never_uploads() {
        let server = fake(None);
        let (_d, ro) = machine(&[&server.url], None, 1, false);
        let (_f, lib) = artifact("libfoo-abc.rlib", "x");
        ro.put("keyro", "foo", "1.0.0", &[lib]).unwrap();
        ro.flush_remote();
        assert!(server.files.lock().unwrap().is_empty());
    }

    #[test]
    fn sharding_places_each_entry_on_the_servers_its_key_hashes_to() {
        let servers = [fake(None), fake(None), fake(None)];
        let urls: Vec<&str> = servers.iter().map(|s| s.url.as_str()).collect();
        let (_d, a) = machine(&urls, None, 1, true);
        for i in 0..30 {
            a.put_value(&format!("keysh{i:02}"), &i).unwrap();
        }
        a.flush_remote();
        let counts: Vec<usize> = servers.iter().map(|s| s.files.lock().unwrap().len()).collect();
        assert_eq!(counts.iter().sum::<usize>(), 30, "each entry on exactly one server: {counts:?}");
        assert!(counts.iter().all(|&c| c >= 3), "the load is spread: {counts:?}");
        // A different machine with the same list finds every entry.
        let (_db, b) = machine(&urls, None, 1, false);
        for i in 0..30 {
            assert_eq!(b.get_value::<i32>(&format!("keysh{i:02}")), Some(i));
        }
    }

    #[test]
    fn replicas_keep_entries_on_several_servers_and_survive_losing_one() {
        let (s1, s2) = (fake(None), fake(None));
        let (u1, u2) = (s1.url.clone(), s2.url.clone());
        let urls = [u1.as_str(), u2.as_str()];
        let (_d, a) = machine(&urls, None, 2, true);
        for i in 0..10 {
            a.put_value(&format!("keyrep{i}"), &i).unwrap();
        }
        assert_eq!((s1.files.lock().unwrap().len(), s2.files.lock().unwrap().len()), (10, 10), "every entry on both");
        drop(s1); // one server goes away
        std::thread::sleep(Duration::from_millis(50));
        let (_db, b) = machine(&urls, None, 2, false);
        for i in 0..10 {
            assert_eq!(b.get_value::<i32>(&format!("keyrep{i}")), Some(i), "still served by the other");
        }
        // Publishing keeps working against the survivor.
        let (_dc, c) = machine(&urls, None, 2, true);
        c.put_value("keynew", &99).unwrap();
        assert!(s2.files.lock().unwrap().contains_key("keynew/value.json"));
    }

    #[test]
    fn a_dead_server_is_tried_once_and_then_left_alone() {
        let dead = "http://127.0.0.1:1"; // nothing listens on port 1
        let live = fake(None);
        let t = HttpTransport::new(&[dead.to_string(), live.url.clone()], None, 2).unwrap();
        let dead_idx = 0;
        // Ranking depends on the (random) port, so pick a key the dead server owns.
        let key = (0..1000)
            .map(|i| format!("keydead{i}"))
            .find(|k| t.order(k)[0] == dead_idx)
            .expect("some key ranks the dead server first");
        live.files.lock().unwrap().insert(format!("{key}/value.json"), b"1".to_vec());
        assert!(t.is_up(dead_idx));
        assert!(t.fetch(&key, "value.json").unwrap().is_some(), "served by the live one");
        assert!(!t.is_up(dead_idx), "the failed connection took it out of rotation");
        // Later requests do not even try it.
        let before = Instant::now();
        for _ in 0..20 {
            t.fetch(&key, "value.json").unwrap();
        }
        assert!(before.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn every_server_is_down_means_misses_not_errors_or_hangs() {
        let (_d, store) = machine(&["http://127.0.0.1:1"], None, 1, true);
        let start = Instant::now();
        assert!(store.get("keynone").is_none());
        assert!(store.get_value::<u8>("keynone").is_none());
        store.put_value("keynone", &1u8).unwrap(); // local write still succeeds
        assert_eq!(store.get_value::<u8>("keynone"), Some(1));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn placement_is_stable_balanced_and_moves_little_when_a_server_leaves() {
        let urls: Vec<String> = (1..=4).map(|i| format!("http://10.0.0.{i}:7070")).collect();
        let full = HttpTransport::new(&urls, None, 1).unwrap();
        let without_last = HttpTransport::new(&urls[..3], None, 1).unwrap();
        let keys: Vec<String> = (0..400).map(|i| format!("key{i:04}")).collect();
        let mut load = [0usize; 4];
        let mut moved = 0;
        for k in &keys {
            let top = full.order(k)[0];
            assert_eq!(top, full.order(k)[0], "deterministic");
            load[top] += 1;
            // Rendezvous hashing: only the leaver's keys change owner.
            if top != 3 && without_last.order(k)[0] != top {
                moved += 1;
            }
        }
        assert_eq!(moved, 0, "keys owned by remaining servers did not move");
        assert!(load.iter().all(|&n| (60..=140).contains(&n)), "balanced: {load:?}");
    }

    #[test]
    fn specs_are_parsed_into_the_right_kind_of_store() {
        let dir = Remote::from_spec("/mnt/team-cache", true, None, 1).unwrap();
        assert_eq!(dir.root(), Some(Path::new("/mnt/team-cache")));
        let http = Remote::from_spec("http://a:7070, https://b:7070 http://c:7070", true, None, 2).unwrap();
        assert!(http.root().is_none());
        let d = http.describe();
        assert!(d.contains("http://a:7070") && d.contains("https://b:7070") && d.contains("http://c:7070"), "{d}");
        assert!(Remote::from_spec("http://", true, None, 1).is_err());
    }

    #[test]
    fn urls_with_odd_characters_in_paths_are_encoded() {
        let t = HttpTransport::new(&["http://h:1/base/".into()], None, 1).unwrap();
        let u = t.url(0, "key1", "tree/sub dir/a b#c.rs");
        assert_eq!(u.path(), "/base/v1/o/key1/tree/sub%20dir/a%20b%23c.rs");
    }
}
