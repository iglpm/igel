# Testing & Code Coverage

Testing in Rune is built directly into the core build scheduler. Instead of waiting for every library, executable, and benchmark in the workspace to finish compiling before launching your test suite, Rune runs tests the moment the first test binary links.

## Pipelined Test Execution

When you run `rune test`, testing runs concurrently with ongoing compilations:

```text
▸ ok app (unit tests: lib) · 41 passed · 0.12s
▸ ok app (test api) · 12 passed · 1.83s
⠹ building ━━━━━━━━━━━━━━━╾───────── 61/97 server, cli
⠼ testing  ━━━━━━━━╾──────────────── 2/6 · 53 passed · app (test integration) 7/30 4s
```

The terminal display reports completed suites, total passed tests, and active test runs in real time. If a test fails, the failure details print immediately while the remainder of the suite continues running.

### Key Architectural Behaviors

- **Isolated Binary Execution**: By default, test binaries execute sequentially (`test.jobs = 1`) while the build continues in parallel. Because tests in separate binaries often share resources like network ports, temporary files, or databases, sequential execution eliminates flaky concurrency failures. If your test binaries are fully isolated, run them concurrently with `rune test --test-jobs 4`.
- **Intelligent Prioritization**: Rune tracks historical test run durations across runs. Binaries that historically take the longest to finish are scheduled and compiled first, minimizing total wall-clock time.
- **Process Cleanup**: If a test spawns background worker processes or mock servers and fails to terminate them, Rune guarantees they will not hang the test runner when the parent binary exits.
- **Strict Timeouts**: Stop hanging tests from blocking CI runners. Pass `--timeout 10m` (or configure `test.timeout = "10m"`) to terminate runs that exceed your threshold and report which individual tests stalled:

```text
✗ FAILED app (test api) · 600.00s
stopped after 600.00s: test binary exceeded timeout (--timeout, test.timeout) (41 of 42 tests finished)
stalled test: upload_resumes_after_disconnect
```

## Everyday Testing Commands

```bash
# Basic Execution
rune test                           # Run unit, integration, and doc tests
rune test parse -- --nocapture      # Filter test names and pass args to test binaries
rune test --no-fail-fast            # Execute all test binaries even after failures
rune bench                          # Run benchmarks

# Targeted Workflows
rune test --failed                  # Re-run only the tests that failed previously
rune test --retries 2               # Automatically retry failures and report flaky tests
rune test --schedule tests          # Distribute tests across all cores (nextest-style)
rune test --cached                  # Skip test binaries whose inputs haven't changed
rune test --junit report.xml        # Export JUnit XML test report for CI
```

## Fast Failure Iteration: `rune test --failed`

Fixing broken tests shouldn't require re-running your entire test suite.

Rune persists the exact set of failing test names per project and profile:
- Running `rune test --failed` compiles and executes only the binaries containing failing tests, running only those specific test names.
- When an individual test passes, Rune clears it from the failure cache. Repeating `rune test --failed` progressively narrows execution until the entire suite is green.
- Combine with filters (`rune test --failed parse`) to target specific subsets of broken tests.

## Flaky Test Detection: `--retries`

Flaky tests degrade CI reliability. The `--retries N` flag automatically re-runs failing tests by exact name up to `N` times:
- If a test passes on a subsequent attempt, Rune marks it as **flaky** in the terminal summary and JUnit XML report without failing the CI pipeline.
- Tests that consistently fail across all attempts are marked as failed, halting the build.

## Instant Cached Test Runs: `--cached`

When code and test inputs haven't changed, re-running hundreds of unit tests wastes time:

```bash
rune test --cached
```

Rune hashes the test binary executable, package fixture files, command-line arguments, and relevant environment variables (`RUST_*`, `CARGO_*`, `RUNE_*`). If all inputs match a previous successful run, Rune skips execution and reports cached results instantly.

On Rune's own 285 tests, running `rune test --cached` drops execution time from **8.0s down to 0.26s**. Failed tests and documentation tests are never cached.

> **Caution**: Enable test caching only for hermetic tests. Tests that depend on external live databases, external HTTP services, or system clocks should run without caching.

## Parallel Test Scheduling: `--schedule tests`

For large integration test suites dominated by a single heavy binary, enable `--schedule tests` (or `test.schedule = "tests"`). 

Rune queries the binary's test list and partitions tests into parallel batches distributed across every available core — similar to `cargo-nextest`. This cuts wall-clock time dramatically on multi-core systems.

## Native Code Coverage: `rune coverage`

Rune includes built-in source-based code coverage powered by LLVM profiling:

```text
file                       lines             functions               regions
src/lib.rs         8/17  47.06 %          2/3  66.67 %         9/19  47.37 %
  not run: 5, 11-17
total              8/17  47.06 %          2/3  66.67 %         9/19  47.37 %
✓ 47.06 % of lines covered (8 of 17) · 1 files
```

Generate comprehensive coverage reports without installing external tools:

```bash
# Terminal summary with missing line annotations
rune coverage --show-missing-lines

# Export in standard formats for CI services
rune coverage --lcov -o lcov.info
rune coverage --cobertura -o coverage.xml
rune coverage --html -o coverage_html/
rune coverage --fail-under 80       # Enforce minimum coverage threshold
```

Coverage builds are isolated inside `.rune/target/coverage/build`, preventing instrumented binaries from invalidating your normal development cache.
