# `rune-bench`: Build System Benchmarking Suite

`rune-bench` is an automated, rigorous benchmarking framework designed to compare Rune and Cargo across everyday developer workflows: cold builds, no-op checks, incremental code edits, branch switching, test execution, and project graph queries.

## Quickstart

Build the benchmark runner and launch a benchmark suite:

```bash
# Build release binaries
cargo build --release -p rune-cli -p rune-bench

# Run quick benchmark suite (~10 minutes: tiny and medium workloads)
./target/release/rune-bench run

# Run full comprehensive suite (includes large workloads and release profiles)
./target/release/rune-bench run --suite full

# Benchmark your own codebase
./target/release/rune-bench run --project ~/src/my-service --edit-core crates/core/src/lib.rs

# List all available workloads and scenarios
./target/release/rune-bench list
```

## Generated Reports

Benchmark results are saved automatically to `./rune-bench-results/<timestamp>/`:
- `results.json`: Full machine-readable dataset containing raw metrics for every run.
- `report.md`: GitHub-flavored Markdown summary tables and speedup factors.
- `report.html`: Interactive HTML report with charts, peak memory plots, and CPU consumption graphs.

Reports are saved progressively after each workload finishes, preserving completed data even if a benchmark is interrupted.

## What Scenarios Are Measured

Every scenario executes identical commands for both tools from identical initial states:

| Category | Scenarios | Workflow Evaluated |
| :--- | :--- | :--- |
| **Startup** | `startup` | CLI initialization overhead via `--version`. |
| **Cold Build** | `cold-build`, `cold-check`, `cold-release` | Compiling with an empty target directory and empty cache (fresh machines, uncached CI). |
| **Disk Footprint** | `disk` | Total disk bytes consumed after a clean build (including CAS artifacts). |
| **No-Op Latency** | `noop-build`, `noop-check`, `noop-test` | The overhead of determining that all targets are up to date. |
| **Incremental Edits** | `edit-leaf`, `edit-core`, `edit-leaf-check`, `edit-test`, `edit-unread` | The inner edit-compile-test loop at the leaf, core, and build-script levels of the dependency DAG. |
| **Cache Invalidation** | `fresh-checkout`, `branch-switch` | Checking out previous commits or switching branches back and forth. |
| **Test Runner** | `test-run` | Executing compiled test suites. |
| **Project Queries** | `metadata`, `tree`, `lockfile` | Language server queries and dependency tree generation. |

## Standard Workloads

`rune-bench` generates deterministic, standalone synthetic workspaces requiring zero network access:

- `tiny`: 2 crates (a library and binary application) measuring pure tool overhead.
- `medium`: 25 crates in 4 hierarchical layers, typical of mid-sized microservices.
- `large`: 73 crates across 6 layers, representative of enterprise multi-crate workspaces.

You can benchmark any real-world repository using `--project DIR`. The runner creates a temporary working copy (leaving out `.git` and `target/`), ensuring your original repository is never modified.

## Methodology & Statistical Rigor

Benchmarking compilers reliably requires eliminating external system noise:

- **Interleaved Execution**: Runs alternate strictly between tools (`rune`, `cargo`, `cargo`, `rune`...) to prevent thermal throttling or background system load from skewing comparisons.
- **Process Isolation**: Each tool operates within its own directory copy and isolated cache environments.
- **Warm-Up Cycles**: Inexpensive scenarios run unrecorded warm-up iterations to prime OS disk caches.
- **Precise Telemetry**: Measures wall-clock time, user+system CPU time across child processes via `wait4`, and peak resident memory (RSS).
- **Environment Sanitization**: Strips caller environment overrides (`CARGO_TARGET_DIR`, `RUSTFLAGS`, `RUSTC_WRAPPER`, `RUNE_*`) to guarantee parity.

## Detecting Performance Regressions in CI

Use `compare` to enforce performance budgets:

```bash
# Benchmark baseline commit
rune-bench run --out /tmp/bench-base

# Benchmark candidate commit
rune-bench run --out /tmp/bench-candidate

# Compare with automated statistical significance threshold
rune-bench compare /tmp/bench-base/results.json /tmp/bench-candidate/results.json --threshold 10
```

`rune-bench compare` checks for statistically significant regressions against Cargo as an internal control. If candidate performance degrades beyond the noise margin across all runs, the command exits with code 1, making it a reliable automated CI gate.
