//! `rune test --cached`: remember passing runs keyed by the binary, package files, arguments, cwd and
//! `RUST_*`/`CARGO_*`/`RUNE_*` env. Opt-in: tests depending on anything else must not be cached.

use rune_common::hash::HashBuilder;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub passed: u64,
    pub ignored: u64,
    pub filtered: u64,
    pub output: String,
}

const MAX_OUTPUT: usize = 256 * 1024;

impl Record {
    pub fn new(passed: u64, ignored: u64, filtered: u64, output: &str) -> Self {
        let mut output = output.to_string();
        if output.len() > MAX_OUTPUT {
            let mut cut = MAX_OUTPUT;
            while !output.is_char_boundary(cut) {
                cut -= 1;
            }
            output.truncate(cut);
        }
        Self {
            passed,
            ignored,
            filtered,
            output,
        }
    }
}

fn hash_file(h: &mut HashBuilder, path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        h.update(&buf[..n]);
    }
}

fn relevant_env(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    let mut v: Vec<_> = vars
        .filter(|(k, _)| {
            (k.starts_with("RUST_") || k.starts_with("CARGO_") || k.starts_with("RUNE_"))
                && k != "CARGO_TARGET_TMPDIR"
        })
        .collect();
    v.sort();
    v
}

pub fn key(
    exe: &Path,
    package_hash: &str,
    args: &[String],
    env: impl Iterator<Item = (String, String)>,
) -> std::io::Result<String> {
    let mut h = HashBuilder::new();
    h.update(b"rune-test-run-v1\0");
    hash_file(&mut h, exe)?;
    h.update(b"\0package\0");
    h.update(package_hash.as_bytes());
    h.update(b"\0args\0");
    for a in args {
        h.update(a.as_bytes());
        h.update(b"\0");
    }
    h.update(b"env\0");
    for (k, v) in relevant_env(env) {
        h.update(k.as_bytes());
        h.update(b"=");
        h.update(v.as_bytes());
        h.update(b"\0");
    }
    Ok(format!("test-run-{}", h.finish_blake3()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> std::vec::IntoIter<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn exe(content: &str) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), content).unwrap();
        f
    }

    fn k(exe: &Path, pkg: &str, args: &[&str], e: &[(&str, &str)]) -> String {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        key(exe, pkg, &args, env(e)).unwrap()
    }

    #[test]
    fn the_same_inputs_give_the_same_key() {
        let f = exe("binary");
        assert_eq!(
            k(f.path(), "p1", &["--x"], &[]),
            k(f.path(), "p1", &["--x"], &[])
        );
    }

    #[test]
    fn every_input_that_can_change_the_outcome_changes_the_key() {
        let a = exe("binary A");
        let b = exe("binary B");
        let base = k(a.path(), "p1", &["--x"], &[("RUST_LOG", "info")]);
        assert_ne!(
            base,
            k(b.path(), "p1", &["--x"], &[("RUST_LOG", "info")]),
            "the binary"
        );
        assert_ne!(
            base,
            k(a.path(), "p2", &["--x"], &[("RUST_LOG", "info")]),
            "the package's files"
        );
        assert_ne!(
            base,
            k(a.path(), "p1", &["--y"], &[("RUST_LOG", "info")]),
            "the arguments"
        );
        assert_ne!(
            base,
            k(a.path(), "p1", &["--x"], &[("RUST_LOG", "debug")]),
            "RUST_*"
        );
        assert_ne!(
            base,
            k(
                a.path(),
                "p1",
                &["--x"],
                &[("RUST_LOG", "info"), ("CARGO_PKG_NAME", "x")]
            ),
            "CARGO_*"
        );
    }

    #[test]
    fn unrelated_environment_and_scratch_paths_do_not_matter() {
        let f = exe("binary");
        let base = k(f.path(), "p", &[], &[]);
        assert_eq!(
            base,
            k(
                f.path(),
                "p",
                &[],
                &[("PATH", "/usr/bin"), ("HOME", "/root")]
            )
        );
        assert_eq!(
            base,
            k(
                f.path(),
                "p",
                &[],
                &[("CARGO_TARGET_TMPDIR", "/somewhere/else")]
            )
        );
    }

    #[test]
    fn environment_order_is_irrelevant() {
        let f = exe("binary");
        assert_eq!(
            k(f.path(), "p", &[], &[("RUST_A", "1"), ("RUST_B", "2")]),
            k(f.path(), "p", &[], &[("RUST_B", "2"), ("RUST_A", "1")])
        );
    }

    #[test]
    fn a_missing_binary_is_an_error_not_a_key() {
        assert!(key(Path::new("/definitely/not/here"), "p", &[], env(&[])).is_err());
    }

    #[test]
    fn records_round_trip_and_cap_their_output() {
        let r = Record::new(3, 1, 0, "hello");
        let back: Record = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);
        let big = "é".repeat(MAX_OUTPUT);
        let capped = Record::new(1, 0, 0, &big);
        assert!(capped.output.len() <= MAX_OUTPUT);
        assert!(capped.output.is_char_boundary(capped.output.len()));
    }
}
