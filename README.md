<div align="center">

# Rune

**Rust builds that never do the same work twice.**

A blazing-fast, Cargo-compatible build system and package manager for Rust.  
Drop it into any existing Cargo workspace — zero migration, zero lock-in.

```bash
curl -fsSL https://runepm.com/install.sh | sh
```

</div>

Rune compiles your existing Rust projects directly with `rustc` while persisting every compiled crate into a shared, content-addressable store. Whether you switch branches, pull an older commit, spin up a secondary worktree, or spin up a fresh CI agent, artifacts built anywhere on your system are restored in milliseconds instead of compiled from scratch.

```text
$ rune build
▸ resolved 249 packages (0.21s)
⠹ building ━━━━━━━━━━━━━━━╾───────── 147/197 regex-syntax, tokio, serde_derive +3
✓ dev · 12 compiled · 185 cached · 4.81s
```

## Why Developers Choose Rune

### Blazing Speed Where It Counts

| Benchmark on Rune Workspace (249 packages) | Rune | Cargo | Speedup |
| :--- | :---: | :---: | :---: |
| No-op build (`build`) | **53 ms** | 225 ms | **4.2× faster** |
| No-op check (`check`) | **64 ms** | 234 ms | **3.6× faster** |
| No-op test prep (`test --no-run`) | **56 ms** | 272 ms | **4.8× faster** |
| Dependency tree inspection (`tree`) | **9 ms** | 184 ms | **20.4× faster** |
| Full clean build from scratch | **112.5 s** | 122.8 s | **Faster** |
| Fresh machine with shared cache | **0.65 s** | Full rebuild (84s+) | **129× faster** |

Across a generated 25-crate benchmark workspace, [`rune-bench`](bench/README.md) clocks fresh checkouts at **108× Cargo's speed**, branch switching at **34×**, and test executions at **9×**. Cold builds and single-file incremental edits remain on par because both run `rustc` — but everywhere else, Rune removes the waiting.

You can verify these figures on your own codebase anytime:
```bash
rune-bench run --project path/to/your/project
```

- **Compile once, share everywhere**: Every compilation unit is keyed by its exact cryptographic inputs — compiler binary, flags, enabled features, target architecture, dependencies, and environment variables. Same inputs yield the exact same artifact across every workspace on your machine.
- **Free `check` after `build`**: Type-checking seamlessly reuses the artifacts produced by full builds.
- **Whole-machine saturation**: Compilation units schedule as soon as upstream metadata exists. Build scripts execute concurrently with independent dependency compilations, critical paths take priority, and dynamic memory tracking prevents system thrashing.

### A Modern, Dependable Test Runner

- **Pipelined execution**: Test binaries run the moment their code links — you don't wait for the entire workspace to finish building before your first test executes.
- **Live progress**: Real-time terminal dashboard tracking active binaries, completed suites, pass/fail counters, and run durations.
- **Predictable concurrency**: Test binaries run sequentially by default to protect shared resources like ports and test databases, while individual tests run in parallel. Parallel binary execution is easily enabled with `--test-jobs`.
- **Laser-focused failure loops**: `rune test --failed` re-runs only the tests that failed previously until everything passes green.
- **Enterprise-ready features**: Native test timeouts, automatic retries with flaky-test detection, JUnit XML generation, cached test outputs (`--cached`: drops no-op runs from 8s to 0.26s), nextest-compatible test batching, and integrated code coverage via `rune coverage`.

### Built for Fast Teams and Clean CI

- **Global cache sharing**: Connect workstations and CI runners via shared network directories or dedicated Cairn cache servers. A developer checking out a fresh repo can restore an entire workspace in under a second.
- **Hermetic cache bundles**: Export and import self-contained, portable cache archives (`rune cas export` / `import`) that stay lean instead of ballooning like persistent `target/` directories.
- **Intelligent change detection**: `rune affected --since origin/main` calculates the exact set of affected packages across your workspace for efficient parallel CI matrix sharding.

### Supply Chain Security Out of the Box

- **Sandboxed build execution**: Constrain build scripts and procedural macros with strict filesystem and network isolation (`build.sandbox = true`).
- **Continuous vulnerability auditing**: `rune audit` scans your locked dependency graph against the RustSec Advisory Database with clear remediation guidance.
- **Complete visibility**: Generate standard CycloneDX and SPDX Software Bills of Materials (SBOMs), inspect execution attack surfaces with `rune tree --exec-surface`, and quarantine fresh packages using `resolve.min-release-age`.

### Everything in a Single Binary

No need to juggle a dozen external Cargo plugins. Rune includes built-in commands for `outdated`, `upgrade`, `unused`, `watch`, single-file `script` runners with embedded dependencies, `explain` (root-cause cache invalidation analysis), `doctor` (diagnostic recommendations), interactive timelines, dependency graphs, `install`, `publish`, and `vendor` — while maintaining full compatibility with all existing `cargo-*` subcommands.

## Installation

### Linux & macOS

```bash
curl -fsSL https://runepm.com/install.sh | sh
```

### Windows (PowerShell)

```powershell
irm https://runepm.com/install.ps1 | iex
```

The installer verifies SHA-256 release checksums and places `rune` into `~/.rune/bin`. Rune requires an active Rust toolchain (`rustup`). Keep your installation updated with `rune self update`.

For package managers, custom directories, and shell completions, check the [Installation Guide](docs/installation.md).

## Quickstart

Rune works immediately inside any existing Cargo project:

```bash
rune build                  # Compile the project (alias: rune b)
rune test                   # Run unit, integration, and doc tests
rune run -- --arg value     # Build and execute your binary
rune check                  # Fast type-checking with build-cache reuse
rune add serde -F derive    # Add a dependency with features
rune watch -x test          # Auto-retest on every file modification
rune doctor                 # Inspect your environment for performance bottlenecks
```

Because Rune mirrors Cargo's CLI surface and manifest format, your codebase stays 100% compatible with standard `cargo`. Rune writes standard `Cargo.lock` files, reads `.cargo/config.toml`, and never pollutes your repository.

## Complete Compatibility

Rune supports the full breadth of the Rust ecosystem:
- Multi-member workspaces and inheritance
- Target-specific dependencies and platform configurations
- Advanced build scripts and procedural macros
- Native C/C++ bindings (`cc`, `bindgen`, `cxx`, `pyo3`)
- Alternative registries, sparse indexes, and vendor directories
- `[patch]` and `[replace]` directives
- Cross-compilation targets and custom profiles
- Standard toolchain files (`rust-toolchain.toml`) and `rust-analyzer`

See the full [Compatibility Reference](docs/compatibility.md) for detailed platform coverage.

## Documentation

| Guide | Description |
| :--- | :--- |
| [Installation](docs/installation.md) | Install options, shell completions, and environment paths |
| [Getting Started](docs/getting-started.md) | First steps, workflow setup, and everyday commands |
| [CLI Commands](docs/commands.md) | Complete CLI reference, selection flags, and interactive pager |
| [Testing & Coverage](docs/testing.md) | Test runner, failure isolation, flaky retries, and coverage reports |
| [Caching Deep Dive](docs/caching.md) | Cache key mechanics, invalidation rules, and CAS management |
| [CI & Remote Caching](docs/ci.md) | Remote stores, Cairn servers, cache bundles, and matrix sharding |
| [Configuration](docs/configuration.md) | Workspace and global settings via `Rune.toml` and CLI flags |
| [Supply Chain Security](docs/supply-chain.md) | Sandboxing, advisory audits, and SBOM export |
| [Performance Guide](docs/performance.md) | Resource management, concurrency, and bottleneck diagnosis |
| [hyprlnk Linker](hyprlnk/README.md) | Integrated next-generation linker for Linux, macOS, and Windows |
| [rustc-runified](rustc-runified/README.md) | Advanced compiler driver for early linking and parallel frontends |
| [Complete Docs Index](docs/README.md) | Overview of all topics, manifests, scripts, and architecture |

## Project Status

**Rune 2027.0** is the first stable release line, tested against hundreds of real-world crates and validated across Linux, macOS, and Windows. Check out the [Roadmap](ROADMAP.md) and [Changelog](CHANGELOG.md) to see where we're headed.

## License

Licensed under either the [Apache License, Version 2.0](LICENSE-APACHE.md) or the [MIT License](LICENSE-MIT.md), at your option.
