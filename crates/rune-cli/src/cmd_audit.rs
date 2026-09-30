//! `rune audit`: known vulnerabilities, and warnings (unmaintained, unsound,
//! yanked), in the packages the workspace locks.
//!
//! The advisories are the RustSec database (plus GitHub's and OSV's own
//! reports for crates.io, among them malicious packages) as published by
//! OSV: one zip of JSON documents, fetched when it changed (an `ETag` check)
//! and kept under `$RUNE_HOME/advisory-db`, so an audit also works offline.
//! `--db` reads a zip or a directory of OSV JSON files instead.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use rayon::prelude::*;
use rune_resolver::{PackageSource, ResolvedGraph};
use semver::Version;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::args::AuditArgs;
use crate::ui;
use crate::workspace::Workspace;

/// Where the database comes from (`RUNE_ADVISORY_DB_URL` overrides it).
const DEFAULT_DB_URL: &str = "https://osv-vulnerabilities.storage.googleapis.com/crates.io/all.zip";

/// A copy younger than this is used without asking whether it changed.
const FRESH_FOR: Duration = Duration::from_secs(3600);

// ---------------------------------------------------------------------------
// The OSV format (the parts an audit needs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Osv {
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub details: String,
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub withdrawn: Option<String>,
    #[serde(default)]
    pub affected: Vec<OsvAffected>,
    #[serde(default)]
    pub references: Vec<OsvReference>,
    #[serde(default)]
    pub severity: Vec<OsvSeverity>,
    #[serde(default)]
    pub database_specific: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OsvAffected {
    pub package: OsvPackage,
    #[serde(default)]
    pub ranges: Vec<OsvRange>,
    #[serde(default)]
    pub versions: Vec<String>,
    #[serde(default)]
    pub ecosystem_specific: serde_json::Value,
    #[serde(default)]
    pub database_specific: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OsvPackage {
    pub name: String,
    #[serde(default)]
    pub ecosystem: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OsvRange {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub events: Vec<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OsvReference {
    #[serde(rename = "type", default)]
    pub kind: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OsvSeverity {
    #[serde(rename = "type")]
    pub kind: String,
    pub score: String,
}

/// What an advisory says about a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Kind {
    /// A malicious release (`MAL-…`).
    Malicious,
    Vulnerability,
    Unsound,
    Unmaintained,
    Notice,
}

impl Kind {
    fn is_warning(self) -> bool {
        matches!(self, Kind::Unsound | Kind::Unmaintained | Kind::Notice)
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Malicious => "malicious",
            Kind::Vulnerability => "vulnerability",
            Kind::Unsound => "unsound",
            Kind::Unmaintained => "unmaintained",
            Kind::Notice => "notice",
        }
    }
}

impl Osv {
    fn kind(&self, affected: &OsvAffected) -> Kind {
        if self.id.starts_with("MAL-") {
            return Kind::Malicious;
        }
        match affected
            .database_specific
            .get("informational")
            .and_then(|v| v.as_str())
        {
            Some("unmaintained") => Kind::Unmaintained,
            Some("unsound") => Kind::Unsound,
            Some(_) => Kind::Notice,
            None => Kind::Vulnerability,
        }
    }

    /// The advisory's own page.
    fn url(&self) -> String {
        if self.id.starts_with("RUSTSEC-") {
            return format!("https://rustsec.org/advisories/{}", self.id);
        }
        self.references
            .iter()
            .find(|r| r.kind == "ADVISORY")
            .map(|r| r.url.clone())
            .unwrap_or_else(|| format!("https://osv.dev/vulnerability/{}", self.id))
    }

    /// GitHub's rating (`HIGH`), for advisories without a CVSS 3 score.
    fn rated(&self) -> Option<String> {
        self.database_specific
            .get("severity")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase())
            .filter(|s| s != "unknown")
    }

    fn cvss(&self) -> Option<&str> {
        self.severity
            .iter()
            .find(|s| s.kind == "CVSS_V3")
            .or_else(|| self.severity.first())
            .map(|s| s.score.as_str())
    }
}

// ---------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------

/// `1`, `1.2`, `1.2.3-rc.1` — the index and advisories are not always strict.
pub(crate) fn lenient_version(s: &str) -> Option<Version> {
    let s = s.trim();
    if s == "0" {
        // OSV's "since the beginning".
        return Some(Version::parse("0.0.0-0").unwrap());
    }
    if let Ok(v) = Version::parse(s) {
        return Some(v);
    }
    let (core, rest) = match s.find(['-', '+']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let parts: Vec<&str> = core.split('.').collect();
    let padded = match parts.len() {
        1 => format!("{}.0.0{rest}", parts[0]),
        2 => format!("{}.{}.0{rest}", parts[0], parts[1]),
        _ => return None,
    };
    Version::parse(&padded).ok()
}

/// Whether `v` falls in an OSV range: events in version order switch it on
/// (`introduced`) and off (`fixed`, or past `last_affected`).
pub(crate) fn in_range(range: &OsvRange, v: &Version) -> bool {
    let mut events: Vec<(Version, &str)> = range
        .events
        .iter()
        .flat_map(|e| e.iter())
        .filter_map(|(k, ver)| Some((lenient_version(ver)?, k.as_str())))
        .collect();
    events.sort_by(|a, b| a.0.cmp(&b.0));
    let mut affected = false;
    for (at, kind) in events {
        match kind {
            "introduced" if at <= *v => affected = true,
            "fixed" if at <= *v => affected = false,
            "last_affected" if at < *v => affected = false,
            _ => {}
        }
    }
    affected
}

fn affects(a: &OsvAffected, v: &Version) -> bool {
    a.versions
        .iter()
        .filter_map(|s| lenient_version(s))
        .any(|x| x == *v)
        || a.ranges
            .iter()
            .filter(|r| r.kind == "SEMVER" || r.kind == "ECOSYSTEM")
            .any(|r| in_range(r, v))
}

/// The releases that fixed it, oldest first.
fn fixed_versions(a: &OsvAffected) -> Vec<Version> {
    let mut v: Vec<Version> = a
        .ranges
        .iter()
        .flat_map(|r| r.events.iter())
        .filter_map(|e| e.get("fixed"))
        .filter_map(|s| lenient_version(s))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Cargo's compatible range of a version: the same major, the same minor for
/// `0.x`, the same patch for `0.0.x`.
fn compatible(a: &Version, b: &Version) -> bool {
    match (a.major, a.minor) {
        (0, 0) => b.major == 0 && b.minor == 0 && b.patch == a.patch,
        (0, m) => b.major == 0 && b.minor == m,
        (m, _) => b.major == m,
    }
}

// ---------------------------------------------------------------------------
// CVSS
// ---------------------------------------------------------------------------

/// The CVSS 3.x base score of a vector
/// (`CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H` → 9.8).
pub(crate) fn cvss3_score(vector: &str) -> Option<f64> {
    if !vector.starts_with("CVSS:3") {
        return None;
    }
    let m: HashMap<&str, &str> = vector
        .split('/')
        .skip(1)
        .filter_map(|p| p.split_once(':'))
        .collect();
    let changed = *m.get("S")? == "C";
    let av = match *m.get("AV")? {
        "N" => 0.85,
        "A" => 0.62,
        "L" => 0.55,
        "P" => 0.2,
        _ => return None,
    };
    let ac = match *m.get("AC")? {
        "L" => 0.77,
        "H" => 0.44,
        _ => return None,
    };
    let pr = match (*m.get("PR")?, changed) {
        ("N", _) => 0.85,
        ("L", false) => 0.62,
        ("L", true) => 0.68,
        ("H", false) => 0.27,
        ("H", true) => 0.5,
        _ => return None,
    };
    let ui = match *m.get("UI")? {
        "N" => 0.85,
        "R" => 0.62,
        _ => return None,
    };
    let cia = |k: &str| -> Option<f64> {
        Some(match *m.get(k)? {
            "H" => 0.56,
            "L" => 0.22,
            "N" => 0.0,
            _ => return None,
        })
    };
    let iss = 1.0 - (1.0 - cia("C")?) * (1.0 - cia("I")?) * (1.0 - cia("A")?);
    let impact = if changed {
        7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15)
    } else {
        6.42 * iss
    };
    if impact <= 0.0 {
        return Some(0.0);
    }
    let exploitability = 8.22 * av * ac * pr * ui;
    let raw = if changed {
        (1.08 * (impact + exploitability)).min(10.0)
    } else {
        (impact + exploitability).min(10.0)
    };
    // The specification's "round up" to one decimal.
    let int = (raw * 100_000.0).round() as i64;
    Some(if int % 10_000 == 0 {
        int as f64 / 100_000.0
    } else {
        ((int / 10_000) + 1) as f64 / 10.0
    })
}

fn severity_of(score: f64) -> &'static str {
    match score {
        0.0 => "none",
        s if s < 4.0 => "low",
        s if s < 7.0 => "medium",
        s if s < 9.0 => "high",
        _ => "critical",
    }
}

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

/// The advisories, and where they came from.
pub(crate) struct Database {
    pub advisories: Vec<Osv>,
    pub source: String,
    /// When the copy used was fetched (seconds since the epoch).
    pub fetched: Option<u64>,
}

fn cache_dir() -> PathBuf {
    rune_common::settings::rune_home().join("advisory-db")
}

/// Reads OSV documents from a zip file or a directory of `.json` files.
pub(crate) fn read_advisories(path: &Path) -> Result<Vec<Osv>> {
    if path.is_dir() {
        let files: Vec<PathBuf> = walkdir::WalkDir::new(path)
            .into_iter()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .map(|e| e.into_path())
            .collect();
        return Ok(files
            .par_iter()
            .filter_map(|f| serde_json::from_slice(&std::fs::read(f).ok()?).ok())
            .collect());
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse_zip(&bytes).with_context(|| format!("reading {}", path.display()))
}

fn parse_zip(bytes: &[u8]) -> Result<Vec<Osv>> {
    let entries = zip_entries(bytes)?;
    // Decompressing and parsing some thousands of documents: in parallel.
    Ok(entries
        .par_iter()
        .filter(|e| e.name.ends_with(".json"))
        .filter_map(|e| {
            let data = e.read(bytes).ok()?;
            serde_json::from_slice::<Osv>(&data).ok()
        })
        .collect())
}

/// One member of a zip archive.
struct ZipEntry {
    name: String,
    method: u16,
    compressed: usize,
    header: usize,
}

impl ZipEntry {
    fn read(&self, zip: &[u8]) -> Result<Vec<u8>> {
        let h = self.header;
        if zip.get(h..h + 4) != Some(b"PK\x03\x04".as_slice()) {
            bail!("bad local header for {}", self.name);
        }
        let name_len = u16le(zip, h + 26)? as usize;
        let extra_len = u16le(zip, h + 28)? as usize;
        let start = h + 30 + name_len + extra_len;
        let data = zip
            .get(start..start + self.compressed)
            .context("zip entry out of bounds")?;
        match self.method {
            0 => Ok(data.to_vec()),
            8 => {
                let mut out = Vec::new();
                std::io::Read::read_to_end(&mut flate2::read::DeflateDecoder::new(data), &mut out)?;
                Ok(out)
            }
            m => bail!("unsupported zip compression method {m}"),
        }
    }
}

fn u16le(b: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        b.get(at..at + 2).context("truncated zip")?.try_into()?,
    ))
}

fn u32le(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(at..at + 4).context("truncated zip")?.try_into()?,
    ))
}

/// The members of a zip archive, from its central directory.
fn zip_entries(zip: &[u8]) -> Result<Vec<ZipEntry>> {
    // The end-of-central-directory record: in the last 64 KiB + 22 bytes.
    let from = zip.len().saturating_sub(65_535 + 22);
    let eocd = (from..zip.len().saturating_sub(21))
        .rev()
        .find(|&i| &zip[i..i + 4] == b"PK\x05\x06")
        .context("not a zip file")?;
    let count = u16le(zip, eocd + 10)? as usize;
    let mut at = u32le(zip, eocd + 16)? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if zip.get(at..at + 4) != Some(b"PK\x01\x02".as_slice()) {
            bail!("corrupt zip central directory");
        }
        let method = u16le(zip, at + 10)?;
        let compressed = u32le(zip, at + 20)? as usize;
        let name_len = u16le(zip, at + 28)? as usize;
        let extra_len = u16le(zip, at + 30)? as usize;
        let comment_len = u16le(zip, at + 32)? as usize;
        let header = u32le(zip, at + 42)? as usize;
        let name = String::from_utf8_lossy(
            zip.get(at + 46..at + 46 + name_len)
                .context("truncated zip")?,
        )
        .into_owned();
        entries.push(ZipEntry {
            name,
            method,
            compressed,
            header,
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// The cached database, refreshed first unless offline or fresh.
fn load_database(args: &AuditArgs, offline: bool) -> Result<Database> {
    if let Some(path) = &args.db {
        return Ok(Database {
            advisories: read_advisories(path)?,
            source: path.display().to_string(),
            fetched: None,
        });
    }
    let dir = cache_dir();
    let zip_path = dir.join("crates.io.zip");
    let etag_path = dir.join("crates.io.etag");
    let url = std::env::var("RUNE_ADVISORY_DB_URL").unwrap_or_else(|_| DEFAULT_DB_URL.into());
    let age = std::fs::metadata(&etag_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok());
    let have = zip_path.is_file();
    let want_fetch = !offline && !args.no_fetch && (!have || age.is_none_or(|a| a > FRESH_FOR));
    if want_fetch {
        let etag = have
            .then(|| std::fs::read_to_string(&etag_path).ok())
            .flatten();
        let fetching = ui::Activity::start("fetching", "the advisory database");
        let result = rune_registry::http::fetch_if_changed(&url, etag.as_deref().map(str::trim));
        fetching.clear();
        match result {
            Ok(Some(f)) => {
                std::fs::create_dir_all(&dir)?;
                rune_common::fsutil::write_atomic(&zip_path, &f.body)?;
                std::fs::write(&etag_path, f.etag.unwrap_or_default())?;
            }
            // Unchanged: remember that it was checked.
            Ok(None) => {
                let _ = filetime::set_file_mtime(&etag_path, filetime::FileTime::now());
            }
            Err(e) if have => ui::warn(format!(
                "could not update the advisory database ({e}); using the copy from {}",
                describe_age(&etag_path)
            )),
            Err(e) => bail!("could not download the advisory database: {e}"),
        }
    } else if !have {
        bail!(
            "no advisory database yet, and {}: run `rune audit` online once",
            if offline {
                "working offline"
            } else {
                "--no-fetch was given"
            }
        );
    }
    let fetched = std::fs::metadata(&etag_path)
        .or_else(|_| std::fs::metadata(&zip_path))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    Ok(Database {
        advisories: read_advisories(&zip_path)?,
        source: url,
        fetched,
    })
}

fn describe_age(path: &Path) -> String {
    let secs = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map_or(0, |d| d.as_secs());
    match secs {
        0..=3_599 => format!("{} min ago", secs / 60),
        3_600..=86_399 => format!("{} h ago", secs / 3_600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// Drops withdrawn advisories and the GitHub/OSV copies of RustSec ones.
fn deduplicate(all: Vec<Osv>) -> Vec<Osv> {
    let rustsec: HashSet<String> = all
        .iter()
        .filter(|a| a.id.starts_with("RUSTSEC-"))
        .flat_map(|a| std::iter::once(a.id.clone()).chain(a.aliases.iter().cloned()))
        .collect();
    all.into_iter()
        .filter(|a| a.withdrawn.is_none())
        .filter(|a| {
            a.id.starts_with("RUSTSEC-")
                || !(rustsec.contains(&a.id) || a.aliases.iter().any(|x| rustsec.contains(x)))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

/// An advisory that applies to a locked package.
struct Finding<'a> {
    advisory: &'a Osv,
    kind: Kind,
    name: String,
    version: Version,
    fixed: Vec<Version>,
    functions: Vec<String>,
    /// Chains from a workspace member to the package.
    paths: Vec<Vec<String>>,
    score: Option<f64>,
}

impl Finding<'_> {
    /// What to do about it.
    fn fix(&self) -> String {
        let newer: Vec<&Version> = self.fixed.iter().filter(|f| **f > self.version).collect();
        if let Some(v) = newer.iter().find(|f| compatible(&self.version, f)) {
            return format!(">={v} — `rune update {} --precise {v}`", self.name);
        }
        match newer.first() {
            Some(v) => format!(">={v} (a new major version: raise the requirement)"),
            None if self.kind == Kind::Unmaintained => "none — look for an alternative".into(),
            None => "no fixed release".into(),
        }
    }
}

fn locked_crates_io(resolved: &ResolvedGraph) -> Vec<(String, Version, String)> {
    let mut v: Vec<(String, Version, String)> = resolved
        .packages
        .iter()
        .filter(|(_, p)| {
            matches!(&p.source, PackageSource::Registry(i) if i == rune_common::CRATES_IO_INDEX)
        })
        .map(|(id, p)| (p.name.clone(), p.version.clone(), id.clone()))
        .collect();
    v.sort();
    v
}

pub fn audit(args: AuditArgs) -> Result<()> {
    let ws = Workspace::load(args.manifest.manifest_path.as_deref())?;
    let offline = args.manifest.offline || ws.config.settings.offline();
    let existing = rune_resolver::LockfileManager::load_fast(ws.root());
    let (resolved, _) = crate::graph::resolve_fresh(&ws, offline, existing.as_ref(), false)?;
    let packages = locked_crates_io(&resolved);

    let mut db = load_database(&args, offline)?;
    let advisories = deduplicate(std::mem::take(&mut db.advisories));
    let mut by_name: HashMap<&str, Vec<(&Osv, &OsvAffected)>> = HashMap::new();
    for a in &advisories {
        for aff in &a.affected {
            if aff.package.ecosystem.is_empty() || aff.package.ecosystem == "crates.io" {
                by_name
                    .entry(aff.package.name.as_str())
                    .or_default()
                    .push((a, aff));
            }
        }
    }

    let mut ignore: HashSet<String> = args.ignore.iter().cloned().collect();
    ignore.extend(ws.config.settings.audit_ignore());
    let roots: Vec<String> = ws.members.iter().map(|m| m.id()).collect();
    let mut findings: Vec<Finding> = Vec::new();
    let mut ignored = 0usize;
    for (name, version, id) in &packages {
        for (a, aff) in by_name.get(name.as_str()).into_iter().flatten() {
            if !affects(aff, version) {
                continue;
            }
            if ignore.contains(&a.id) || a.aliases.iter().any(|x| ignore.contains(x)) {
                ignored += 1;
                continue;
            }
            let paths: Vec<Vec<String>> = rune_resolver::trace_why(&resolved, &roots, name)
                .into_iter()
                .filter(|p| p.last() == Some(id))
                .collect();
            let functions = aff
                .ecosystem_specific
                .pointer("/affects/functions")
                .and_then(|f| f.as_array())
                .map(|f| {
                    f.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            findings.push(Finding {
                advisory: a,
                kind: a.kind(aff),
                name: name.clone(),
                version: version.clone(),
                fixed: fixed_versions(aff),
                functions,
                paths,
                score: a.cvss().and_then(cvss3_score),
            });
        }
    }
    findings.sort_by(|a, b| {
        (
            a.kind,
            std::cmp::Reverse((a.score.unwrap_or(0.0) * 10.0) as i64),
            &a.name,
        )
            .cmp(&(
                b.kind,
                std::cmp::Reverse((b.score.unwrap_or(0.0) * 10.0) as i64),
                &b.name,
            ))
    });

    // Yanked releases, from the index copies resolution keeps.
    let mut yanked: Vec<(String, Version)> = Vec::new();
    if !args.no_yanked {
        let names: Vec<String> = {
            let mut n: Vec<String> = packages.iter().map(|(n, _, _)| n.clone()).collect();
            n.dedup();
            n
        };
        if let Ok(client) =
            rune_registry::source::IndexClient::for_registry(rune_common::CRATES_IO_INDEX, true)
        {
            let yanked_by_name: HashMap<&String, Vec<String>> = names
                .par_iter()
                .map(|n| (n, client.yanked_versions(n)))
                .filter(|(_, v)| !v.is_empty())
                .collect();
            for (name, version, _) in &packages {
                if yanked_by_name.get(name).is_some_and(|vs| {
                    vs.iter()
                        .any(|v| lenient_version(v).as_ref() == Some(version))
                }) {
                    yanked.push((name.clone(), version.clone()));
                }
            }
        }
    }

    let deny: HashSet<&str> = args.deny.iter().map(String::as_str).collect();
    let denied =
        |kind: Kind| !kind.is_warning() || deny.contains("warnings") || deny.contains(kind.label());
    let failing = findings.iter().filter(|f| denied(f.kind)).count()
        + if deny.contains("warnings") || deny.contains("yanked") {
            yanked.len()
        } else {
            0
        };

    if args.format == "json" {
        print_json(&db.source, db.fetched, packages.len(), &findings, &yanked);
    } else {
        print_human(
            &advisories,
            &db,
            packages.len(),
            &findings,
            &yanked,
            ignored,
            args.verbose,
        );
    }
    if failing > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn print_human(
    advisories: &[Osv],
    db: &Database,
    scanned: usize,
    findings: &[Finding],
    yanked: &[(String, Version)],
    ignored: usize,
    verbose: bool,
) {
    let updated = db
        .fetched
        .map(|s| format!(", updated {}", &rune_common::time::format_rfc3339(s)[..10]))
        .unwrap_or_default();
    ui::action(
        "checked",
        format!(
            "{} against {} advisories{updated}",
            ui::plural(scanned, "package"),
            advisories.len()
        ),
    );
    for f in findings {
        let a = f.advisory;
        let rating = match (f.kind, f.score) {
            (Kind::Vulnerability, Some(s)) => format!("{} {s:.1}", severity_of(s)),
            (Kind::Vulnerability, None) => a.rated().unwrap_or_else(|| "vulnerability".into()),
            (k, _) => k.label().to_string(),
        };
        let head = format!("{} {} · {} · {}", f.name.bold(), f.version, a.id, rating);
        if f.kind.is_warning() {
            ui::warn(head);
        } else {
            ui::err(format!("{} {head}", "✗".red().bold()));
        }
        let title = if a.summary.is_empty() {
            a.details.lines().next().unwrap_or("").to_string()
        } else {
            a.summary.clone()
        };
        ui::err(format!("  {title}"));
        let mut rows: Vec<(&str, String)> = vec![("fix", f.fix())];
        if !f.functions.is_empty() {
            rows.push(("affects", f.functions.join(", ")));
        }
        let arrow = " → ";
        for (i, p) in f
            .paths
            .iter()
            .take(if verbose { usize::MAX } else { 2 })
            .enumerate()
        {
            rows.push((if i == 0 { "path" } else { "" }, p.join(arrow)));
        }
        if f.paths.len() > 2 && !verbose {
            rows.push(("", format!("and {} more (--verbose)", f.paths.len() - 2)));
        }
        if verbose && !a.details.is_empty() {
            for line in a.details.lines().take(12) {
                rows.push(("", line.to_string()));
            }
        }
        rows.push(("info", a.url()));
        for l in ui::kv_lines(&rows, 2) {
            ui::err(l);
        }
    }
    for (name, version) in yanked {
        ui::warn(format!(
            "{} {version} · yanked · `rune update {name}` moves to a release that is not",
            name.bold()
        ));
    }
    let vulns = findings.iter().filter(|f| !f.kind.is_warning()).count();
    let warnings = findings.len() - vulns + yanked.len();
    let mut parts = Vec::new();
    if warnings > 0 {
        parts.push(ui::plural(warnings, "warning"));
    }
    if ignored > 0 {
        parts.push(format!("{ignored} ignored"));
    }
    let tail = if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(" · "))
    };
    if vulns > 0 {
        ui::error(format!(
            "{} found{tail}",
            ui::plural(vulns, "vulnerability").replace("vulnerabilitys", "vulnerabilities")
        ));
    } else {
        ui::summary(format!("no known vulnerabilities{tail}"));
    }
}

fn print_json(
    source: &str,
    fetched: Option<u64>,
    scanned: usize,
    findings: &[Finding],
    yanked: &[(String, Version)],
) {
    let item = |f: &Finding| {
        let a = f.advisory;
        serde_json::json!({
            "advisory": {
                "id": a.id,
                "aliases": a.aliases,
                "package": f.name,
                "title": a.summary,
                "description": a.details,
                "date": a.published,
                "url": a.url(),
                "cvss": a.cvss(),
                "score": f.score,
                "severity": f.score.map(|s| severity_of(s).to_string()).or_else(|| a.rated()),
                "informational": f.kind.is_warning().then(|| f.kind.label()),
                "kind": f.kind.label(),
            },
            "package": { "name": f.name, "version": f.version.to_string() },
            "versions": { "patched": f.fixed.iter().map(|v| format!(">={v}")).collect::<Vec<_>>() },
            "affected": { "functions": f.functions },
            "paths": f.paths,
        })
    };
    let vulns: Vec<_> = findings
        .iter()
        .filter(|f| !f.kind.is_warning())
        .map(item)
        .collect();
    let mut warnings: BTreeMap<&str, Vec<serde_json::Value>> = BTreeMap::new();
    for f in findings.iter().filter(|f| f.kind.is_warning()) {
        warnings.entry(f.kind.label()).or_default().push(item(f));
    }
    for (name, version) in yanked {
        warnings
            .entry("yanked")
            .or_default()
            .push(serde_json::json!({
                "kind": "yanked",
                "package": { "name": name, "version": version.to_string() },
            }));
    }
    let doc = serde_json::json!({
        "database": {
            "source": source,
            "last-updated": fetched.map(rune_common::time::format_rfc3339),
        },
        "lockfile": { "dependency-count": scanned },
        "vulnerabilities": {
            "found": !vulns.is_empty(),
            "count": vulns.len(),
            "list": vulns,
        },
        "warnings": warnings,
    });
    ui::out(doc.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(events: &[(&str, &str)]) -> OsvRange {
        OsvRange {
            kind: "SEMVER".into(),
            events: events
                .iter()
                .map(|(k, v)| BTreeMap::from([(k.to_string(), v.to_string())]))
                .collect(),
        }
    }

    #[test]
    fn ranges_follow_osv_semantics() {
        let r = range(&[
            ("introduced", "0.6.3"),
            ("fixed", "0.6.14"),
            ("introduced", "1.0.0"),
            ("fixed", "1.6.1"),
        ]);
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(!in_range(&r, &v("0.6.2")));
        assert!(in_range(&r, &v("0.6.3")));
        assert!(!in_range(&r, &v("0.6.14")));
        assert!(in_range(&r, &v("1.6.0")));
        assert!(!in_range(&r, &v("1.6.1")));
        let all = range(&[("introduced", "0")]);
        assert!(in_range(&all, &v("0.0.1")));
        let last = range(&[("introduced", "0"), ("last_affected", "2.1")]);
        assert!(in_range(&last, &v("2.1.0")));
        assert!(!in_range(&last, &v("2.1.1")));
    }

    #[test]
    fn cvss_scores_match_the_specification() {
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"),
            Some(9.8)
        );
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N"),
            Some(6.1)
        );
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:L/AC:H/PR:H/UI:R/S:U/C:N/I:N/A:L"),
            Some(1.8)
        );
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:N"),
            Some(0.0)
        );
        assert_eq!(cvss3_score("CVSS:4.0/AV:N"), None);
    }

    #[test]
    fn github_copies_of_rustsec_advisories_are_dropped() {
        let osv = |id: &str, aliases: &[&str]| Osv {
            id: id.into(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            summary: String::new(),
            details: String::new(),
            published: None,
            withdrawn: None,
            affected: vec![],
            references: vec![],
            severity: vec![],
            database_specific: serde_json::Value::Null,
        };
        let kept = deduplicate(vec![
            osv("RUSTSEC-2021-0003", &["GHSA-43w2-9j62-hq99"]),
            osv("GHSA-43w2-9j62-hq99", &["RUSTSEC-2021-0003"]),
            osv("GHSA-only-github", &[]),
        ]);
        let ids: Vec<&str> = kept.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["RUSTSEC-2021-0003", "GHSA-only-github"]);
    }
}
