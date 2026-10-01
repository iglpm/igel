# Architecture and contributing

## Crates

```
rune CLI
├── rune-manifest     (Cargo.toml: TOML 1.1, profiles + overrides, lints, targets, workspaces)
├── rune-resolver     (version resolution, lockfiles, per-build feature resolver, [patch], tree/why)
├── rune-registry     (sparse index, registries & source replacement, git dependencies, downloads)
├── rune-graph        (dependency DAG with normal/build edges, critical path, pruning)
├── rune-fingerprint  (content hashing, mtime+size cache)
├── rune-scheduler    (parallel executor, jobserver, pipelining, build-script steps, declared inputs, schedule analysis)
├── rune-compiler     (direct rustc invocation, build scripts)
├── rune-cas          (immutable content-addressable store, atomic writes, LRU GC)
└── rune-common       (shared types, settings, toolchain, .cargo/config.toml, cfg evaluation)
```

## Building and testing Rune

```bash
cargo build --release      # or: rune build --release
cargo test --workspace     # or: rune test
```

`cargo test --workspace` runs about 500 tests and needs no network: every
end-to-end test (`crates/rune-cli/tests/cli.rs`) builds throwaway projects
with their own `RUNE_HOME`. Before sending a change: `cargo fmt --all`,
`cargo clippy --workspace --all-targets` (no warnings) and the tests.
[ROADMAP.md](../ROADMAP.md) has the status against the compatibility
specification, measurements, and what comes next.

## Measuring a change

`rune-bench` (in [`bench/`](../bench/README.md)) runs Rune and Cargo through
the same scenarios on generated workspaces or a copy of any project. For a
change that might affect speed, measure before and after on a quiet machine
and compare:

```bash
cargo build --release -p rune-cli -p rune-bench
./target/release/rune-bench run --out /tmp/before          # on the base commit
./target/release/rune-bench run --out /tmp/after           # with the change
./target/release/rune-bench compare /tmp/before/results.json /tmp/after/results.json
```
