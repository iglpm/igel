# Testing: `rune test`, `rune bench`, `rune coverage`

## How a test run works

Test binaries are part of the same build graph as everything else. Testing
starts the moment the first test binary links, while the rest of the
workspace is still compiling, instead of after a full build:

```text
▸ ok app (unit tests: lib) · 41 passed · 0.12s
▸ ok app (test api) · 12 passed · 1.83s
⠹ building ━━━━━━━━━━━━━━━╾───────── 61/97 server, cli
⠼ testing  ━━━━━━━━╾──────────────── 2/6 · 53 passed · app (test integration) 7/30 4s
```

The *testing* line shows how many binaries are done, how many tests have
passed so far (counted from each binary's output as its tests finish), and
what runs now with its own progress and time. Each binary's result is printed
as soon as it is in; a failure shows while the rest still runs.

- **One test binary at a time**, as with Cargo, while the build goes on.
  Tests in different binaries often share a port, a file or a database;
  running two binaries at once makes such suites fail now and then.
  `rune test --test-jobs 4` (or `test.jobs = 4`) runs binaries side by side
  when your tests don't share anything. Each binary still runs its own tests
  in parallel, as libtest does.
- **The slowest binaries are built first.** Rune remembers how long each test
  binary ran; the scheduler builds the binaries a run will wait longest on
  (and the programs their tests start) before the quick ones.
- **Programs a test starts are ready.** A package's binaries and examples are
  built before its tests run (`CARGO_BIN_EXE_<name>` works), as with Cargo.
- **Leftover processes don't hang the run.** A test that starts a server and
  forgets it no longer keeps `rune test` waiting once the test binary exits.
- **The environment is Cargo's**: `CARGO`, `CARGO_MANIFEST_DIR`,
  `CARGO_MANIFEST_PATH`, every `CARGO_PKG_*`, `CARGO_TARGET_TMPDIR`, and the
  library paths of build scripts' native libraries.

```bash
rune test                          # unit, integration and doc tests
rune test parse -- --nocapture     # a filter, and arguments for the test binaries
rune test --no-fail-fast           # run every binary even after a failure
rune test --retries 2              # re-run failed tests; ones that pass are reported as flaky
rune test --junit report.xml       # a JUnit report for CI
rune test --message-format json    # one `test-suite-finished` line per binary
rune test --schedule tests         # split binaries into batches spread over every core
rune bench                         # benchmarks, one binary at a time
```

At the end, a run longer than a few seconds names its slowest binaries.

## Scheduling tests across binaries

`--schedule tests` (or `test.schedule = "tests"`) lists each binary's tests
and splits them into batches, each its own process, scheduled across all
binaries at once — like cargo-nextest. A suite dominated by one large binary
finishes sooner; the price is that tests no longer share a process (a test
that relies on another's global state in the same binary may notice).

## Flaky tests

`--retries N` runs a binary's failed tests again, by exact name, up to N
times. A test that passes on a later attempt is reported as flaky (in the
summary and in the JUnit report) instead of failing the run; one that keeps
failing fails it. A binary that crashed without a failing test is not retried.

## Cached test results

`rune test --cached` (or `test.cache = true`) remembers a passing test binary under
a key made of the binary's bytes (which cover code, dependencies and compiler),
every file in the package directory (fixtures), the arguments, and the
`RUST_*`/`CARGO_*`/`RUNE_*` environment. Change any of them and the binary runs
again; a failing run is never remembered; `--cached=false` forces a run; doc-tests
always run. On Rune's own 285 tests: about 8 s → 0.26 s when nothing changed.

It is opt-in because Rune can only key what it can see. **Do not enable it** for
tests that depend on the network, the clock, randomness, other environment
variables or files outside the package — they would keep "passing" from the cache
while the world they test has changed.

## Code coverage: `rune coverage`

```
file                       lines             functions               regions
src/lib.rs         8/17  47.06 %          2/3  66.67 %         9/19  47.37 %
  not run: 5, 11-17
total              8/17  47.06 %          2/3  66.67 %         9/19  47.37 %
✓ 47.06 % of lines covered (8 of 17) · 1 files
```

Source-based coverage like `cargo llvm-cov`: only the workspace's own crates
are compiled with `-C instrument-coverage` (dependencies stay shared with
ordinary builds), the tests run — programs they start through
`CARGO_BIN_EXE_*` count too — and the profiles are merged and reported with the
toolchain's `llvm-profdata`/`llvm-cov` (the `llvm-tools` component is added on
first use). `--show-missing-lines`, `--lcov` (demangled names),
`--json`, `--cobertura`, `--html`, `--fail-under 80`,
`--ignore-filename-regex`; a failing test ends the run unless
`--no-fail-fast`, which reports what ran. `cfg(coverage)` is set, as with
cargo-llvm-cov. The instrumented build lives in `<target>/coverage/build`, so
`rune coverage` and `rune build`/`rune test` never replace each other's files
and each stays warm; a profile cut short (a test binary killed mid-write) is
left out with a warning rather than failing the report.
