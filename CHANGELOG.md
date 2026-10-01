# Changelog

All notable changes to Rune are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versions are `YEAR.MINOR.PATCH`: the year of the release line, a minor
release for new features, a patch release for fixes. Within a year's line
releases are compatible (semver-style), so `rune self update` within it is
always safe; the first stable release is 2027.0.0.

## [2027.0.0] - Unreleased

The first stable release. Changes since the 0.1.0 preview:

### Added
- `rune-bench` (in `bench/`): benchmarks Rune against Cargo on generated
  workspaces or a copy of any project — startup, cold builds, no-op builds,
  rebuilds after edits, cache reuse, tests, inspection commands, disk and
  memory — with JSON, Markdown and HTML reports and a regression check.
- `rune doctor` checks free disk space and memory per parallel job.
- "Did you mean" for misspelled `-p` packages and commands (`rune buidl`); a
  test filter that matches nothing says so.
- `rune audit`: RustSec/OSV advisories (vulnerabilities with CVSS scores,
  unmaintained, unsound, malicious) and yanked releases in the lockfile, with
  the dependency path and the fix; cached database, `--deny`, `--ignore`,
  `audit.ignore`, JSON output.
- `rune coverage`: source-based coverage of the workspace's tests; summary,
  missing lines, LCOV, JSON, Cobertura and HTML reports, `--fail-under`.
- `rune metadata` lists every package with its dependencies, targets and
  features, and resolve nodes with `deps`/`dep_kinds`, matching
  `cargo metadata` (ids included).
- `rune test` shows a live line while tests run: binaries done, tests passed
  so far (counted as they finish), and what is running with its progress and
  time. `rune coverage` shows its merge and report steps.
- `rune test --failed`: builds and runs only the tests that failed last
  time, by exact name; repeated, it narrows down to what is still broken,
  and runs everything once nothing is. A failed run points to it.
- `rune test --test-jobs N` / `test.jobs`: how many test binaries run at once.
- `rune test --timeout 10m` / `test.timeout`: a test binary that runs longer
  is stopped and reported as failed, naming the tests that had not finished,
  so a hung test cannot hang a CI job.
- The live test line names tests libtest reports as running for over a
  minute.
- Tests get `CARGO`, `CARGO_MANIFEST_PATH` and every `CARGO_PKG_*` variable at
  run time, as Cargo sets them.

### Fixed
- Clear messages instead of low-level errors: no Rust toolchain (or a
  `rustup` without a default toolchain), a full disk (rustc and linkers crash
  with a bus error there; Rune now names the cause, and warns before a build
  when under 1 GiB is free), network and TLS failures (with what to check).
- `edition = "2099"` and `version = "1.x"` are errors, as in Cargo.
- TOML errors in manifests print once, naming the file once.
- Doc tests run after the test binaries, as with Cargo: run first, a failing
  doc test could stop the run before a failing unit test was reported.
- `rune watch` no longer restarts forever when a run writes into the project
  (a JUnit report, logs, test snapshots): changes to files git ignores don't
  count. It says which file set off a run.
- A credential helper (`cargo:libsecret`, an external provider) that exits
  before reading its input is an error, instead of silently ending
  `rune login`/`publish` with SIGPIPE.
- A failed test binary's output is followed by one blank line, not three.
- `rune test -q` prints the result line; a binary killed by a signal counts as
  a failed test; a failed download no longer reports "downloaded".
- A crash (a bug in Rune) prints what happened and where to report it.
- Rune compiles cleanly for Windows (two Unix-only assumptions); CI checks
  Windows and runs on pushes to `master`.
- The install scripts take `--version 2027.0` (not only the full tag), say
  what is wrong with a bad option, install the native build under Rosetta,
  refuse musl-based Linux (the builds need glibc) with how to build from
  source, show the `PATH` line for the user's shell, and replace a running
  `rune` safely. The Intel macOS release builds on a supported runner image.
- `rune self uninstall --purge` refuses to delete a `RUNE_HOME` that is the
  home directory, a file system root, or holds nothing of Rune's.
- `rune self update` uses Rune's network settings (CA bundles, proxy) and no
  longer gives up on a slow download after 30 seconds.
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
- Tests failed now and then when run again unchanged: test binaries ran side
  by side, so tests in different binaries sharing a port, a file or a
  database collided. They now run one at a time by default, as with Cargo,
  while the build continues.
- `rune test` waited until a process a test had started (a server it did not
  stop) exited, as that process held the output pipe open.
- A freshly linked test binary could fail to start with "text file busy".
- A `rune build` or `rune test` sharing a target directory with a concurrent
  one could put a read-only store file where the other's compiler was
  writing, failing its build.
- `rune coverage` and ordinary builds replaced each other's files (they share
  file names); the instrumented build has its own directory now.
- A target directory or store inside a package (`--target-dir out`, a CI's
  `RUNE_HOME=$PWD/.cache`) counted as the package's source, so nothing was
  ever cached. Both are marked with `CACHEDIR.TAG` and left out.

### Changed
- A crate of the project is keyed by the files rustc read for it (its
  modules, `include_str!`/`include_bytes!` files), recorded after it
  compiles, instead of by every file of its package. Editing an integration
  test, a README or a doc, or a log file appearing in a package no longer
  recompiles the package's library and everything that depends on it (in a
  25-crate workspace, a new file in the bottom crate rebuilt 21 crates; now
  none). Crates with a build script keep the package-wide key.
- Quick commands on small projects no longer start a thread pool for two or
  three items of work: its spinning workers made their time erratic.
- The README is a short introduction; the reference documentation moved to
  `docs/` (installation, commands, compatibility, settings, testing, caching,
  CI, supply chain, …). Install with `curl -fsSL https://runepm.com/install.sh | sh`.
- Compiler probes are shared by every project and checkout that selects the
  same toolchain, and the host's cfg is asked for alongside them: a first
  command in a new project or on a fresh CI machine starts fewer compilers.
- `rune test` builds first the test binaries that ran longest last time (and
  the programs their tests start), so the long ones start earlier.
- Faster offline resolution (about 35 %): mimalloc as the allocator, and a
  binary copy of each parsed index file beside it.
- Test results are printed as each binary finishes rather than in a fixed
  order (reports keep the fixed order).
- `rune coverage` merges and exports at the same time, and its own test
  suite runs faster (Rune's: no fixed sleeps, a Nagle stall in a test server
  gone).

## [0.1.0] - 2026-09-29

The preview release: a Cargo-compatible build system and package manager for
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
