# Dependencies and supply chain

## Confining build-time code

`build.sandbox = true` (or `RUNE_BUILD_SANDBOX=1`) runs every build script and
compiler — and with it every proc-macro — confined: no network, and a
read-only file system except the build's output (a build script: its
`OUT_DIR`) and a private temporary directory. A build script that tries to
plant something in `~/.bashrc` or another crate's sources, or to phone home,
fails instead. On Linux this uses unprivileged user, mount and network
namespaces (on Ubuntu 23.10+ AppArmor may have to allow them); on macOS,
`sandbox-exec`. `build.sandbox-network = "openssl-src, foo"` lets the build
scripts of those packages use the network. It costs nothing measurable: a full
sandboxed rebuild of Rune's own 194 units (ring, zstd-sys, proc-macros) takes
as long as an unconfined one.

Version resolution takes the newest release each requirement allows and
settles the rare conflict (two versions in one compatible range) by
preferring or backtracking; when that is not enough it solves the choice as a
whole with PubGrub, and when no choice exists it says why:

```
! no set of versions satisfies every requirement:
  Because b 1.0.0 depends on a 1.1.0 and the workspace depends on b 1.0.0, the workspace depends on a 1.1.0.
  And because the workspace depends on a 1.0.0, the workspace cannot be resolved.
```

A release that needs a version of something that was never published is
skipped for an older one, as Cargo does.

`rune outdated` lists the declared dependencies with newer releases (compatible,
and newest overall, honouring each package's `rust-version`); `rune upgrade`
raises the requirements in place, keeping comments and how each requirement is
written (`1` stays `1`, `0.7` becomes `0.9`). `rune unused` runs `rustc`'s own
`unused_crate_dependencies` lint over every target of the workspace's packages —
no text search, so proc-macros, renamed crates and build-dependencies are
judged correctly — and suggests moving a dependency only tests use to
`[dev-dependencies]`; `--fix` removes the unused ones.

`rune sbom` writes a CycloneDX 1.5 or SPDX 2.3 bill of materials with package
URLs, checksums, licences and the locked dependency edges (`SOURCE_DATE_EPOCH`
makes it reproducible). `rune tree --exec-surface` lists every crate that runs
code on your machine while building — build scripts, proc-macros, native
`links` crates — with the chain that brings each in. `resolve.min-release-age =
"14d"` keeps resolution away from releases younger than that.

## Security advisories: `rune audit`

```
▸ checked 236 packages against 1875 advisories, updated 2026-09-30
✗ smallvec 1.6.0 · RUSTSEC-2021-0003 · critical 9.8
  Buffer overflow in SmallVec::insert_many
  fix   >=1.6.1 — `rune update smallvec --precise 1.6.1`
  path  app 0.1.0 → middle 1.0.0 → smallvec 1.6.0
  info  https://rustsec.org/advisories/RUSTSEC-2021-0003
! bincode 1.3.3 · RUSTSEC-2025-0141 · unmaintained
✗ 1 vulnerability found · 1 warning
```

Every locked crates.io package is checked against the RustSec database as OSV
publishes it (also GitHub's and OSV's reports for crates.io, among them
malicious releases; copies of one advisory are reported once): known
vulnerabilities with their CVSS score, unmaintained and unsound crates, and
yanked releases. Each finding names the path that brings the crate in and the
fix — the `rune update … --precise` line when a compatible release fixes it.
The database is fetched when it changed (an ETag check) and kept in
`~/.rune/advisory-db`, so the audit also works offline; a cached audit takes
about 0.1 s. Vulnerabilities fail the command (exit 1), warnings only with
`--deny warnings` (or `--deny unmaintained`, `unsound`, `yanked`);
`--ignore RUSTSEC-…` or `audit.ignore` accepts a reviewed advisory; `--format
json` follows cargo-audit's shape; `--db` reads a zip or directory of OSV
files.
