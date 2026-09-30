# Changelog

All notable changes to Rune are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- `rune audit`: RustSec/OSV advisories (vulnerabilities with CVSS scores,
  unmaintained, unsound, malicious) and yanked releases in the lockfile, with
  the dependency path and the fix; cached database, `--deny`, `--ignore`,
  `audit.ignore`, JSON output.
- `rune coverage`: source-based coverage of the workspace's tests; summary,
  missing lines, LCOV, JSON, Cobertura and HTML reports, `--fail-under`.
- `rune metadata` lists every package with its dependencies, targets and
  features, and resolve nodes with `deps`/`dep_kinds`, matching
  `cargo metadata` (ids included).

### Fixed
- A dependency cycle made the build hang; it is now an error naming the cycle.
- A build script could be served another project's script output when both
  had a package of the same name and version (script records named files by
  absolute path and were shared between different scripts).
- `[patch."<git url>"]` was ignored; `default-run` was ignored by `rune run`.
- `--target` outputs are in `target/<triple>/<profile>/`, as with Cargo.
- Manifests Cargo refuses are refused: no library or binary, a feature naming
  nothing, `dep:` on a non-optional dependency, a feature a local dependency
  lacks, two members with one name, packages named `test`/`std`/`core`/….
- A test binary killed by a signal says which signal.

### Changed
- Faster offline resolution (about 35 %): mimalloc as the allocator, and a
  binary copy of each parsed index file beside it.

## [0.1.0] - 2026-09-29

The first release: a Cargo-compatible build system and package manager for
Rust that builds with `rustc` directly and reuses compiled artifacts through
a content-addressable store.

### Building
- Content-hash fingerprinting, `.rmeta` pipelining, critical-path scheduling
  weighted by measured compile times; build scripts as their own step.
- About 1.3 compilers per core sharing one GNU jobserver; starting a job never
  waits behind other crates' codegen threads.
- Memory-aware admission: each compiler's peak memory is measured and a unit
  waits until what it needs is free. `build.low-priority` runs compilers niced.
- `build.feature-unification = "workspace"` (and Cargo's
  `resolver.feature-unification`): any subset of a workspace shares dependency
  builds.
- `build.sandbox`: build scripts and proc-macros run with no network and
  nothing writable outside the build (Linux namespaces, macOS `sandbox-exec`);
  `build.sandbox-network` allow-lists packages.

### Store
- Immutable, verified artifacts shared across projects, profiles and targets;
  shared directories and Cairn servers as remote stores; `rune cas export/import`.
- Automatic size cap (a tenth of the disk, 5–50 GiB), removal after 30 days
  unused, zstd compression after 7 days unused.
- `rune cas pin` / `unpin` / `pins`: a workspace's entries survive cleanup.

### Resolution
- Byte-identical `Cargo.lock` with Cargo on a 469-package corpus.
- PubGrub for version conflicts the walk cannot settle, with an explanation
  when none can be; releases whose dependencies do not exist are skipped.
- The binary lockfile carries the fingerprint of the inputs it was resolved
  from and is written atomically, so a lockfile from another resolution is
  never trusted (fixes a spurious "undeclared crate").

### Testing
- Pipelined test runner: retries with flaky-test detection, JUnit and JSON
  reports, `--cached` results, and `--schedule tests` to schedule batches of
  tests across every binary at once.

### Commands and UX
- Compact grouped help with an interactive pager; `rune tree`, `why`,
  `features`, `graph`, `outdated`, `upgrade`, `unused`, `sbom`, `affected`,
  `watch`, `doctor`, `explain` and the Cargo commands.
- `rune version -v`, `rune completions <shell>`; library warnings print like
  Rune's own.

### Distribution
- `install.sh` (checksummed release download), CI and release workflows for
  Linux and macOS on x86_64 and aarch64.
