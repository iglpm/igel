# Troubleshooting Guide

When a build fails or behaves unexpectedly, this guide will help you quickly isolate the root cause and resolve it.

Most build issues fall into one of three categories:
1. **The project itself**: The error occurs under both `cargo` and `rune`.
2. **Environment & tools**: A missing C compiler, insufficient disk space, or low memory.
3. **Rune-specific behavior**: Invalidation rules, cache configuration, or concurrency edge cases.

## Initial Diagnostic Steps

Run these three commands to isolate the problem:

```bash
# 1. Inspect environment, linkers, disk space, and toolchain setup
rune doctor

# 2. Check if the project compiles under Cargo
cargo build

# 3. Inspect what Rune is scheduling to compile and why
rune explain
```

If `cargo build` produces the exact same failure, the issue lies within your project code or dependency requirements rather than Rune.

## Unexpected Recompilations

If Rune compiles crates that you expected to be restored from cache, use `rune explain`:

```bash
rune explain
```

```text
▸ 1 of 197 units would be compiled (0 come from the store)

  app v0.1.0 (bin `app`) [dev]
    · sources changed: src/main.rs (modified)
```

Common causes of unexpected cache invalidations:

- **Build scripts without `rerun-if-changed`**: If a `build.rs` script does not declare `println!("cargo::rerun-if-changed=...")`, Cargo and Rune assume that *any* file change in the package requires re-running the script and recompiling the crate. Always declare explicit change filters in your build scripts.
- **Environment variable drift**: If a build script accesses environment variables via `rerun-if-env-changed`, differing variables across terminals, IDEs, and CI environments will produce different cache keys.
- **Varying compiler flags**: Differences in `RUSTFLAGS`, active features, profiles, or wrappers (`RUSTC_WRAPPER`) generate distinct artifacts.
- **Toolchain or Rune updates**: Upgrading your Rust toolchain or Rune release updates compiler fingerprints, intentionally requiring a clean recompilation.

## Slow Builds

- **Critical Path Bottlenecks**: If a build takes over 10 seconds and leaves CPU cores underutilized, Rune names the bottleneck dependency chain in your terminal. Run `rune build --timings` to inspect the visual timeline in `.rune/timings/rune-timing.html`. Splitting the largest crate on the critical path often provides the greatest speedup.
- **Memory Throttling Notifications**: A notice stating `note: memory ran short, so fewer compilers ran at once` indicates that Rune paused spawning new compiler workers to prevent swap thrashing or OOM crashes. Close memory-intensive background apps or adjust `-j`.

## Compiler Errors About Dependencies

```text
error[E0463]: can't find crate for `tracing`
error[E0460]: found possibly newer version of crate `dep` which `mid` depends on
```

If these errors occur under Rune but not Cargo, rustc encountered mismatched dependency builds. Rebuild the workspace with clean artifacts:

```bash
rune build --rebuild-all
```

If the issue persists, please [report a bug](#reporting-a-bug).

## Lockfile and Resolution Errors

- **`the dependency resolution does not contain ...`**: A requested dependency is missing from the lockfile. Run `rune update -p <name>` to re-resolve the specific crate, and ensure that any `[patch]` directives in `Cargo.toml` or `.cargo/config.toml` point to valid sources.
- **`failed to select a version for ...`**: Dependency version requirements conflict across your workspace graph. Use `rune tree -i <name>` and `rune why <name>` to trace which crates are demanding conflicting versions.
- **`the lock file ... needs to be updated but --locked was passed`**: Manifest requirements were edited without updating `Cargo.lock`. Run `rune build` without `--locked` (or run `rune update`) and commit the updated lockfile.

## Native Dependencies & Build Scripts

Crates that compile native C/C++ libraries (such as `ring`, `openssl-sys`, `libz-sys`) require system build tools like `cc`, `cmake`, and `pkg-config`:
- **Linux**: Install your distribution's development meta-packages (e.g. `build-essential` on Debian/Ubuntu, `base-devel` on Arch, or `gcc` on Fedora).
- **Windows**: Install the *Desktop development with C++* workload via the Visual Studio Installer.
- **Build Sandboxing**: If `build.sandbox = true` is active, build scripts cannot access the internet to download prebuilt binaries. Add the package to the network allowlist in `Rune.toml`: `build.sandbox-network = "openssl-src"`.

## Storage and Cache Management

Rune automatically cleans up idle artifacts, but you can inspect and reclaim storage space on demand:

```bash
# View disk usage by package and age
rune cas du

# Clean local project build outputs (.rune/target)
rune clean

# Prune cache entries unused for more than 7 days
rune cas gc --max-age 7d

# Cap cache storage to 10 GB (removes least-recently-used artifacts)
rune cas gc --max-size 10GB
```

## Platform Notes (Windows)

- **"Access is denied" on `rune.exe`**: Occurs if an existing `rune` process is currently running when you try to replace it. Wait for the active process to finish or build into an alternate `--target-dir`.
- **Long Path Exceeded**: Very deeply nested directories can exceed the legacy Windows 260-character path limit. Enable `LongPathsEnabled` in the Windows Registry or clone repositories closer to your drive root.
- **Antivirus Interference**: Real-time scanners can introduce noticeable latency during compilation. Adding folder exclusions for `~/.rune` and project `.rune` directories significantly improves build speeds.

## Resetting to a Clean State

If you want to start fresh:

```bash
# Recompile everything without deleting existing cache entries
rune build --rebuild-all

# Remove workspace target outputs
rune clean

# Wipe the local content-addressable store completely
rm -rf ~/.rune/cas
```

## Verbose Telemetry and Debugging

Inspect Rune's internal operations with verbosity and tracing flags:

```bash
# Verbose command execution
rune build -v

# Detailed execution logs (cache hits, compiler invocations)
RUST_LOG=debug rune build

# Trace scheduler worker commands specifically
RUST_LOG=rune_scheduler=debug rune build

# Print stage duration breakdown
RUNE_TIMINGS=1 rune build
```

## Reporting a Bug

If you encounter unexpected behavior, please submit an issue at [https://github.com/runepm/rune/issues](https://github.com/runepm/rune/issues) including:
- Output of `rune version -v`
- Your operating system and architecture
- The exact command invoked and terminal output (ideally with `RUST_LOG=debug`)
- Whether the failure reproduces under standard `cargo build`
- A minimal reproduction project if possible
