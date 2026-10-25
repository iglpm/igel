# Getting Started with Igel

This guide walks you through setting up Igel, creating your first project, and adopting Igel in existing Cargo repositories without any friction.

Igel is designed as a drop-in replacement for Cargo. It uses the exact same manifests, understands the same flags, and runs the same commands. The difference lies entirely in execution: Igel persists every compilation unit into a cryptographic, content-addressable store shared across all your projects. Work compiled once is instantly available everywhere on your machine.

## Installation

Install the official pre-built binary for your platform:

```bash
# Linux and macOS
curl -fsSL https://runepm.com/install.sh | sh

# Windows (PowerShell)
irm https://runepm.com/install.ps1 | iex
```

Igel uses your existing Rust toolchain (managed via `rustup` or directly available on your `PATH`). Verify your setup and check system diagnostics:

```bash
igl version -v
igl doctor
```

For advanced options like custom installation directories, specific release tags, or compiling from source, see the [Installation Guide](installation.md).

## Creating a New Project

Creating and launching a project with Igel feels completely familiar:

```bash
igl new hello
cd hello
igl run
```

```text
✓ dev · 1 compiled · 0 cached · 0.55s
▸ running .igel/target/debug/hello
Hello, world!
```

`igl new` initializes a standard Rust workspace with a `.gitignore` configured to ignore both `target/` and `.igel/`. If you run `igl run` immediately again, nothing recompiles — Igel restores the output in milliseconds directly from the cache.

Add a dependency and compile:

```bash
igl add serde -F derive
igl build
igl test
```

## Adopting Igel in an Existing Project

Adopting Igel requires zero configuration files, zero migrations, and zero lock-in. Simply navigate to any directory containing a `Cargo.toml` and run:

```bash
igl build
```

Igel parses `Cargo.toml`, respects `Cargo.lock`, honors `.cargo/config.toml`, and builds with the versions you have already pinned. You can switch between `cargo` and `igl` at any time without conflict.

### Workspace Files Overview

When you run Igel in a workspace, it manages the following files:

| File | Purpose | Version Control |
| :--- | :--- | :--- |
| `Cargo.lock` | Standard Cargo lockfile. Created if absent and updated only when dependency requirements change. | Commit to Git (same as Cargo). |
| `Igel.lock.bin` | Fast binary cache of dependency resolution for instant command startup. Rebuilt automatically if manifests change. | Ignore (not required in Git). |
| `.igel/` | Build artifacts, compilation timings, and dependency fingerprints (Igel's target directory). | Add `.igel/` to `.gitignore`. |
| `Igel.toml` | Optional configuration file for project-specific [settings](configuration.md) or standalone manifests. | Commit to Git if used. |

Build binaries are placed in `.igel/target/<profile>/` following standard layout conventions: debug binaries appear in `.igel/target/debug/<name>` and release builds in `.igel/target/release/<name>`. You can customize this anytime using `--target-dir` or the `build.target-dir` setting.

## The Everyday Developer Workflow

Igel gives you instant, responsive feedback throughout your development cycle:

```bash
igl check                 # Fast type-checking (reuses existing build outputs)
igl build                 # Compile your active target (alias: igl b)
igl run -- --arg value    # Compile and execute with command-line arguments
igl test                  # Run unit, integration, and doc tests
igl test my_filter        # Filter tests by matching string
igl test --failed         # Re-run only previously failed tests
igl watch -x check        # Automatically re-run checks on file modifications
```

Every command finishes with a clear summary:

```text
✓ dev · 3 compiled · 194 cached · 2.71s
```

Units labeled *compiled* ran through `rustc`, while units marked *cached* were restored instantly from the store. When you start working on a new project, Igel only compiles dependencies that have never been built before on your system.

## Why Igel Is So Fast

Every compiled crate is indexed under a cryptographic key representing its complete input state: the compiler binary, compiler flags, target profile, active features, upstream dependencies, and the exact files read from disk.

Because identical inputs always produce the identical key:
- **Instant branch switching**: Checking out an older commit or switching branches restores previously compiled binaries in milliseconds.
- **Instant secondary worktrees**: Creating a new Git worktree or clone builds almost instantaneously by reusing existing artifacts.
- **Free `check` after `build`**: Running `igl check` after a build compiles nothing — it verifies your code using the already compiled artifacts.
- **Global dependency reuse**: Common crates across different projects (`serde`, `tokio`, `syn`, `regex`) are compiled once per machine rather than duplicated in every project's `target/` directory.

To learn more about cache resolution, visit [Caching Mechanics](caching.md). To set up shared caching across your team or CI runners, check out [CI & Shared Caches](ci.md).

## Next Steps

- [Command Reference](commands.md): Detailed reference for commands, flags, and options.
- [Testing & Coverage](testing.md): Retries, flaky test isolation, nextest-style scheduling, and code coverage.
- [Configuration Reference](configuration.md): Complete guide to workspace and global settings.
- [Troubleshooting](troubleshooting.md): Diagnosing unexpected behaviors and compiler warnings.
- [Cargo Compatibility](compatibility.md): Comprehensive compatibility matrix and supported ecosystem tools.
