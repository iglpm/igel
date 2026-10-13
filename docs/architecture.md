# Internal Architecture & Contributing

Rune is designed as a modular suite of specialized Rust crates, each responsible for an isolated aspect of package management, dependency resolution, execution scheduling, and artifact caching.

## Workspace Crate Architecture

The Rune codebase is organized into clean, focused layers:

```text
rune CLI
├── rune-manifest     (Manifest parsing: Cargo.toml/Rune.toml, TOML 1.1, profiles, lints, workspaces)
├── rune-resolver     (Dependency resolution: PubGrub solver, lockfiles, per-target feature trees)
├── rune-registry     (Index client: HTTP sparse indexes, Git dependencies, package downloads, source replacement)
├── rune-graph        (Compilation DAG: normal/build edges, critical path analysis, cycle detection)
├── rune-fingerprint  (Cryptographic hashing: file content hashing, size+mtime acceleration caches)
├── rune-scheduler    (Execution engine: pipelining, jobserver integration, memory throttling, schedule telemetry)
├── rune-compiler     (Compilation runner: direct rustc invocations, build script execution, diagnostic capture)
├── rune-cas          (Artifact store: content-addressable store, atomic filesystem writes, LRU maintenance)
└── rune-common       (Core primitives: shared domain types, toolchain discovery, configuration loader)
```

## Building and Testing Rune

Rune can be compiled using either standard `cargo` or an existing `rune` installation:

```bash
# Compile release binaries
cargo build --release      # or: rune build --release

# Run comprehensive test suite (~500 automated tests)
cargo test --workspace     # or: rune test
```

The test suite requires no external network access: all end-to-end integration tests (`crates/rune-cli/tests/cli.rs`) create transient mock projects inside isolated temporary `RUNE_HOME` directories.

Before submitting changes, ensure all quality gates pass:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Benchmarking Performance Changes

When modifying scheduling logic, cache hashing, or compiler invocation pipelines, measure performance regressions using `rune-bench` (located in [`bench/`](../bench/README.md)):

```bash
# Build release binaries for the CLI and benchmark tool
cargo build --release -p rune-cli -p rune-bench

# Record baseline benchmark on base commit
./target/release/rune-bench run --out /tmp/bench-before

# Record benchmark with your changes applied
./target/release/rune-bench run --out /tmp/bench-after

# Compare results and check for statistically significant regressions
./target/release/rune-bench compare /tmp/bench-before/results.json /tmp/bench-after/results.json
```

For more details on upcoming development goals, review the [Roadmap](../ROADMAP.md).
