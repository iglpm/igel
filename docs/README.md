# Rune Documentation

Welcome to the Rune documentation. Whether you are installing Rune for the first time, integrating it into your team's CI pipeline, or configuring remote caching, you will find comprehensive guides and references below.

If you are new to Rune, start with the [Installation Guide](installation.md) and the [Getting Started Tutorial](getting-started.md).

## Guides and Reference

| Topic | Description |
| :--- | :--- |
| [Installation](installation.md) | Official install scripts, custom directories, shell completions, and system requirements |
| [Getting Started](getting-started.md) | Your first build, adding dependencies, and adopting Rune in existing Cargo projects |
| [CLI Commands](commands.md) | Comprehensive command reference, Cargo-compatible flags, interactive pager, and progress monitoring |
| [Cargo Compatibility](compatibility.md) | Manifest support, workspaces, build scripts, native dependencies, features, and platform targets |
| [Manifests](manifests.md) | Working with standard `Cargo.toml` or standalone `Rune.toml` configurations |
| [Configuration](configuration.md) | Complete settings reference, precedence hierarchy, and global/project options via `rune config` |
| [Testing & Coverage](testing.md) | Pipelined test execution, failure isolation, flaky-test retries, cached runs, and `rune coverage` |
| [Caching Deep Dive](caching.md) | Cryptographic unit keys, invalidation triggers, concurrent process safety, and store maintenance |
| [Performance Tuning](performance.md) | Scheduler mechanics, jobserver sharing, memory management, and bottleneck diagnosis |
| [CI & Remote Caching](ci.md) | Setting up remote stores, Cairn cache servers, portable cache bundles, and matrix sharding |
| [Supply Chain Security](supply-chain.md) | Build script sandboxing, dependency resolution rules, SBOM generation, and `rune audit` |
| [Cross-Compilation](cross-compilation.md) | `rune cross` and `--cross`: building, testing and running for other targets, here or in warm containers |
| [Single-File Scripts](scripts.md) | Writing and running standalone Rust scripts with embedded dependencies and shebang support |
| [Editor Integration](editor-integration.md) | Configuring rust-analyzer, JSON compiler messages, and editor build plans |
| [Architecture](architecture.md) | Internal crate structure, design principles, and building Rune from source |
| [Troubleshooting](troubleshooting.md) | Common errors, diagnostic commands, and debugging unexpected cache behavior |
| [Benchmarks](../bench/README.md) | Benchmarking methodology and measuring performance differences with `rune-bench` |

## Project Status and Updates

For upcoming milestones and compatibility progress, see the [Roadmap](../ROADMAP.md). For detailed release notes and recent updates, check the [Changelog](../CHANGELOG.md).
