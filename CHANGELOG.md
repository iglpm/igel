# Changelog

All notable changes to Igel are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versions are `YEAR.MINOR.PATCH`: the year of the release line, a minor
release for new features, a patch release for fixes. Within a year's line
releases are compatible (semver-style), so `igl self update` within it is
always safe; the first stable release is 2027.0.0.

## [2027.0.0] - Unreleased

The first stable release. Changes since the 0.1.0 preview:

### Added
- Cross-compilation, built in: `igl cross build|check|test|run|bench|clippy|doc
  -t <triple>`, and `--cross[=<triple>]` on those commands (`igl build --cross
  aarch64-unknown-linux-gnu -r`). hyprcross (the `hyprcross/` submodule) does
  it inside Igel: on this machine when it can build the target, with the
  target's C compiler as the linker's driver and QEMU, Wine or Wasmtime running
  the tests; else in a warm container from the target's image
  (`ghcr.io/runepm/hyprcross/<target>`, 30 targets), where this Igel builds
  with the host's toolchain and shared cache, mounted at their own paths. A
  cached build in a warm container takes about as long as on the host.
  `igl cross targets`, `doctor`, `init`, `daemon` and `images` manage it;
  `cross.*` settings (`default-target`, `engine`, `daemon`, `idle-timeout`,
  `container-engine`, `image-repository`, `image-tag`) and
  `[cross.target.<triple>]` tables (`pre-build`, `env`, `image`, `dockerfile`,
  `volumes`, `runner`, `sysroot`) configure it, and `Hyprcross.toml` and
  cross-rs' `Cross.toml` are read too. `igl cross` used to run an installed
  `cargo-cross`; the built-in command takes its place.
- hyprlnk (the `hyprlnk/` submodule, github.com/runepm/hyprlnk), a linker
  developed with Igel and built into it, and Igel's default linker
  (`build.linker = "hyprlnk"`; `"default"` keeps the one rustc and Cargo
  choose): it links binaries, tests, build scripts, proc-macros and dynamic
  libraries. It links Windows images for x64, ARM64 and x86 from
  `link.exe`-style command lines — PDBs, identical code folding and
  resources included — in about half the time lld takes on Igel's own build;
  macOS images for arm64, arm64e and x86_64 from ld64-style command lines
  — chained fixups, debug maps, code signatures and Objective-C included — in
  less than half the time of Apple's ld (a quarter of ld64.lld's); and Linux
  executables and shared objects for x86-64, x86, AArch64, ARM, RISC-V,
  POWER, s390x and LoongArch (glibc and musl) from GNU ld command lines, in
  45–65 % of lld's time and 55–70 % of mold's. On Linux it links through the
  target's C compiler (`target.<triple>.linker`, else `cc`), which finds the C
  runtime. `igl __hyprlnk <args>` runs it by hand; it also builds as a
  standalone `hyprlnk` executable.
- rustc-runified (in `rustc-runified/`): the toolchain's own compiler plus
  what a stable `rustc` lacks, which Igel uses whenever it is installed for
  the toolchain in use (`build.compiler = "auto"`; `"runified"` requires it,
  `"rustc"` never uses it; `cargo runified driver --igl` installs it).
  Executables, tests, benches and examples compile against their
  dependencies' metadata as soon as it exists, like libraries, and link once
  every rlib does — the link shows as its own step in `--timings`; and stable
  toolchains get the parallel front end (`build.frontend-threads`), with the
  same output whatever the thread count. Igel's own build from scratch:
  55.4 s → 47.1 s. With a runified toolchain (rustc-runified's patches),
  libraries also write their `.rmeta` before monomorphization, so dependents
  start sooner (a chain of libraries builds 3.7% faster), and derive macros
  known to be pure are cached in the workspace's incremental builds.
  `igl doctor` reports it.
- `igel-bench` (in `bench/`): benchmarks Igel against Cargo on generated
  workspaces or a copy of any project — startup, cold builds, no-op builds,
  rebuilds after edits, cache reuse, tests, inspection commands, disk and
  memory — with JSON, Markdown and HTML reports and a regression check.
- `igl doctor` checks free disk space and memory per parallel job.
- "Did you mean" for misspelled `-p` packages and commands (`igl buidl`); a
  test filter that matches nothing says so.
- `igl audit`: RustSec/OSV advisories (vulnerabilities with CVSS scores,
  unmaintained, unsound, malicious) and yanked releases in the lockfile, with
  the dependency path and the fix; cached database, `--deny`, `--ignore`,
  `audit.ignore`, JSON output.
- `igl coverage`: source-based coverage of the workspace's tests; summary,
  missing lines, LCOV, JSON, Cobertura and HTML reports, `--fail-under`.
- `igl metadata` lists every package with its dependencies, targets and
  features, and resolve nodes with `deps`/`dep_kinds`, matching
  `cargo metadata` (ids included).
- `igl test` shows a live line while tests run: binaries done, tests passed
  so far (counted as they finish), and what is running with its progress and
  time. `igl coverage` shows its merge and report steps.
- `igl test --failed`: builds and runs only the tests that failed last
  time, by exact name; repeated, it narrows down to what is still broken,
  and runs everything once nothing is. A failed run points to it.
- `igl test --test-jobs N` / `test.jobs`: how many test binaries run at once.
- `igl test --timeout 10m` / `test.timeout`: a test binary that runs longer
  is stopped and reported as failed, naming the tests that had not finished,
  so a hung test cannot hang a CI job.
- The live test line names tests libtest reports as running for over a
  minute.
- Tests get `CARGO`, `CARGO_MANIFEST_PATH` and every `CARGO_PKG_*` variable at
  run time, as Cargo sets them.

### Fixed
- After a toolchain update, rustc-runified quietly stopped compiling: a driver
  works only with the toolchain it was built for, so builds fell back to plain
  `rustc` and `igl doctor` said nothing. Doctor now warns when the driver is
  installed only for other releases, with the command that builds it for the
  one in use.
- `igl test --target <triple>` warned that test binaries run directly on this
  machine even with a runner configured (`target.<triple>.runner` or
  `CARGO_TARGET_<TRIPLE>_RUNNER`), which runs them; it now warns only without
  one.
- Where the store can't hard-link into the target directory (another file
  system, or another mount of the same one, as with a container's bind mounts
  of the project and `~/.igel`), every build copied each artifact out of the
  store again and hashed the copy: a build with nothing to do took 1.5 s
  instead of 30 ms on hyprcross itself. Copies now carry the stored file's
  modification time, and one of the same length and time counts as restored.
- An inherited `HYPRLNK_CC` chose the C compiler hyprlnk links through for
  targets that name none, host build scripts included: in a cross container
  exporting the target's compiler they failed with "unrecognized command-line
  option '-m64'". The variable is now always set for the links Igel runs
  (empty means `cc`), and so can't differ from what the cache key says.
- A missing C cross compiler (`failed to find tool "riscv64-linux-gnu-gcc"`)
  was reported as a missing host compiler, with `pacman -S base-devel` and the
  like as the fix. The hint now names the cross compiler, its package where
  distributions have one, and building in a container as the alternative.
- A build script's stored result could carry another project's absolute
  paths into a build: a project restored `ISLE_DIR`, link search paths or
  `DEP_*` values pointing into the target directory of whichever project ran
  the script first, failing with "couldn't read `…/out/isle_numerics.rs`"
  (cranelift-codegen) or "could not find native static library
  `WebView2LoaderStatic`" once that directory was gone. Paths into the
  script's `OUT_DIR` and package are now stored portably (Windows 8.3 short
  names included) and relocated on restore; results that cannot be relocated
  are kept per project. Results stored by earlier versions are not used.
- Crates built by different projects did not fit together: registry crates
  compiled in whatever directory `igl` was started from, which rustc hashes
  into a crate's identity, and a store entry could then name a build of its
  dependency the store did not hold — a later build failed with "can't find
  crate for `tracing`" or "found possibly newer version of crate". Crates
  outside the project compile in their package root, as with Cargo; every
  stored crate records the builds of its dependencies it was compiled
  against and is restored only next to them (otherwise compiled again, and
  its entry replaced); nothing compiled against a build that differs from the
  stored one is stored. Entries from earlier versions are not reused, so
  everything compiles once.
- A build failed with "the dependency resolution does not contain `combine` →
  `bytes 0.5.6`" when a dependency had an optional dependency nothing switches
  on (Cargo leaves those out of the lockfile, and so does Igel). The version
  resolver also no longer resolves a renamed optional dependency (`bytes_05 =
  { package = "bytes" }`) because another one on the same package is on.
- Windows: a crate whose earlier build was still in the target directory
  (back on a branch, an edit undone) compiled again instead of coming from
  the store — a rename cannot replace a read-only file there, and every
  restored output is one. It is now replaced atomically.
- Windows: `igl install` could not upgrade a program it had installed;
  `igl watch` ran again for changes to files git ignores; compiler messages
  named files with mixed separators (`app\src/main.rs`); package ids in
  `igl metadata`, JSON messages and `igl pkgid` were not URLs
  (`path+file://C:\…`, now `path+file:///C:/…` as from Cargo).
- Windows: memory is watched as on Linux and macOS (what is available, and
  each compiler's peak), so a big build holds back new compilers instead of
  running the machine out of memory.
- A feature a local dependency lacks is an error on every platform, as with
  Cargo — also when the dependency is for another platform only.
- `igel-bench` counted a file hard-linked into several directories once per
  link on Windows, overstating disk use.
- A lockfile pinning a release newer than the registry index cached on the
  machine (after `cargo update`, or a teammate's commit) was quietly
  re-resolved to an older release, rewriting `Cargo.lock`; Igel now fetches
  the index as Cargo does (offline, it says so instead).
- Clear messages instead of low-level errors: no Rust toolchain (or a
  `rustup` without a default toolchain), a full disk (rustc and linkers crash
  with a bus error there; Igel now names the cause, and warns before a build
  when under 1 GiB is free), network and TLS failures (with what to check).
- `edition = "2099"` and `version = "1.x"` are errors, as in Cargo.
- TOML errors in manifests print once, naming the file once.
- Doc tests run after the test binaries, as with Cargo: run first, a failing
  doc test could stop the run before a failing unit test was reported.
- `igl watch` no longer restarts forever when a run writes into the project
  (a JUnit report, logs, test snapshots): changes to files git ignores don't
  count. It says which file set off a run.
- A credential helper (`cargo:libsecret`, an external provider) that exits
  before reading its input is an error, instead of silently ending
  `igl login`/`publish` with SIGPIPE.
- Building Igel itself with Cargo recompiled `igel-cli` on every build in a
  clone without `.git/packed-refs` (its build script watched the missing
  file); in a worktree it watched the wrong refs.
- A failed test binary's output is followed by one blank line, not three.
- `igl test -q` prints the result line; a binary killed by a signal counts as
  a failed test; a failed download no longer reports "downloaded".
- A crash (a bug in Igel) prints what happened and where to report it.
- Igel compiles cleanly for Windows (two Unix-only assumptions); CI checks
  Windows and runs on pushes to `master`. The whole test suite passes on
  Windows and runs there in CI, registry tests included (they no longer need
  `tar` and `sha256sum`).
- The install scripts take `--version 2027.0` (not only the full tag), say
  what is wrong with a bad option, install the native build under Rosetta,
  refuse musl-based Linux (the builds need glibc) with how to build from
  source, show the `PATH` line for the user's shell, and replace a running
  `igl` safely. The Intel macOS release builds on a supported runner image.
- `igl self uninstall --purge` refuses to delete a `IGEL_HOME` that is the
  home directory, a file system root, or holds nothing of Igel's.
- `igl self update` uses Igel's network settings (CA bundles, proxy) and no
  longer gives up on a slow download after 30 seconds.
- A dependency cycle made the build hang; it is now an error naming the cycle.
- A build script could be served another project's script output when both
  had a package of the same name and version (script records named files by
  absolute path and were shared between different scripts).
- `[patch."<git url>"]` was ignored; `default-run` was ignored by `igl run`.
- `--target` outputs are in `target/<triple>/<profile>/`, as with Cargo.
- Manifests Cargo refuses are refused: no library or binary, a feature naming
  nothing, `dep:` on a non-optional dependency, a feature a local dependency
  lacks, two members with one name, packages named `test`/`std`/`core`/….
- A test binary killed by a signal says which signal.
- Tests failed now and then when run again unchanged: test binaries ran side
  by side, so tests in different binaries sharing a port, a file or a
  database collided. They now run one at a time by default, as with Cargo,
  while the build continues.
- `igl test` waited until a process a test had started (a server it did not
  stop) exited, as that process held the output pipe open.
- A freshly linked test binary could fail to start with "text file busy".
- A `igl build` or `igl test` sharing a target directory with a concurrent
  one could put a read-only store file where the other's compiler was
  writing, failing its build.
- `igl coverage` and ordinary builds replaced each other's files (they share
  file names); the instrumented build has its own directory now.
- A target directory or store inside a package (`--target-dir out`, a CI's
  `IGEL_HOME=$PWD/.cache`) counted as the package's source, so nothing was
  ever cached. Both are marked with `CACHEDIR.TAG` and left out.

### Changed
- A crate of the project is keyed by the files rustc read for it (its
  modules, `include_str!`/`include_bytes!` files), recorded after it
  compiles, instead of by every file of its package. Editing an integration
  test, a README or a doc, or a log file appearing in a package no longer
  recompiles the package's library and everything that depends on it (in a
  25-crate workspace, a new file in the bottom crate rebuilt 21 crates; now
  none). A crate with a build script is keyed by what it read and its
  script's result, so editing `tests/` of a package whose build script
  embeds the git commit (or generates code from a watched file) no longer
  rebuilds its library either.
- Quick commands on small projects no longer start a thread pool for two or
  three items of work: its spinning workers made their time erratic.
- Less work when nothing changed, measured on a 73-crate workspace (no-op
  `check`/`build`/`test --no-run`: 32/39/37 ms, Cargo 65/93/88 ms): unit keys
  are hashed once instead of twice (the per-part breakdown for `igl
  explain` is only worked out for units whose key changed), plain workspace
  member paths are not expanded as globs (which stat every directory from
  the file system root down), and canonical paths are remembered.
- The README is a short introduction; the reference documentation moved to
  `docs/` (installation, commands, compatibility, settings, testing, caching,
  CI, supply chain, …). Install with `curl -fsSL https://runepm.com/install.sh | sh`.
- Compiler probes are shared by every project and checkout that selects the
  same toolchain, and the host's cfg is asked for alongside them: a first
  command in a new project or on a fresh CI machine starts fewer compilers.
- `igl test` builds first the test binaries that ran longest last time (and
  the programs their tests start), so the long ones start earlier.
- Faster offline resolution (about 35 %): mimalloc as the allocator, and a
  binary copy of each parsed index file beside it.
- Test results are printed as each binary finishes rather than in a fixed
  order (reports keep the fixed order).
- `igl coverage` merges and exports at the same time, and its own test
  suite runs faster (Igel's: no fixed sleeps, a Nagle stall in a test server
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
  shared directories and Cairn servers as remote stores; `igl cas export/import`.
- Automatic size cap (a tenth of the disk, 5–50 GiB), removal after 30 days
  unused, zstd compression after 7 days unused.
- `igl cas pin` / `unpin` / `pins`: a workspace's entries survive cleanup.

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
- Compact grouped help with an interactive pager; `igl tree`, `why`,
  `features`, `graph`, `outdated`, `upgrade`, `unused`, `sbom`, `affected`,
  `watch`, `doctor`, `explain` and the Cargo commands.
- `igl version -v`, `igl completions <shell>`; library warnings print like
  Igel's own.

### Distribution
- `install.sh` (checksummed release download), CI and release workflows for
  Linux and macOS on x86_64 and aarch64.
