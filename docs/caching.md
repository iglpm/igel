# Caching Architecture

Rune's performance rests on a fundamental principle: **every compilation unit's cache key is the complete cryptographic identity of its inputs**. 

Instead of relying on fragile file modification timestamps or ad-hoc target directories, Rune verifies the exact state of what goes into every compilation. If the inputs match an existing build, Rune restores the artifact instantly. The Content-Addressable Store (CAS) is shared across every project, branch, and profile on your machine — and can even be distributed across your entire team.

## How Cache Keys Are Constructed

A compilation unit's key is computed from all parameters that influence its machine code:

```text
UnitKey = hash(
    Package Source Identity (Registry checksum, Git commit + submodules, or local source files)
  + Target Metadata (Crate types, edition, active features, host/target role)
  + Compiler Identity (Binary hash, version, driver, wrappers like RUSTC_WRAPPER)
  + Compiler Flags (Effective rustflags, target linker, lint rules, [env] configuration)
  + Effective Profile (Optimization level, debuginfo settings, codegen units)
  + Merkle Dependency Tree (Direct dependency keys, linkage kinds, aliases)
  + Declared External Inputs (Environment variables, watched files, toolchain headers)
)
```

### Immutable Registry & Git Crates

Packages downloaded from crates.io or Git repositories are immutable:
- **Registry packages** are keyed purely by their sparse index hash, package name, version, and checksum. Rune never needs to stat individual source files on disk to compute their key.
- **Git dependencies** are identified by their resolved commit SHA and submodule configuration. Branch names never appear in keys.

### Local Workspace Crates

Local packages receive precise invalidation tracking:
- When a local crate compiles, Rune inspects `rustc`'s dependency graph (including modules, `include_str!`, and `include_bytes!`).
- The crate's cache key is derived from the contents of *only the files rustc actually accessed*.
- Modifying non-compilation files — such as integration test fixtures, `README.md`, or documentation — will never trigger a recompilation of the main library.
- When an earlier commit or branch is checked out, Rune matches the exact file hashes of that commit and immediately restores its previously compiled artifact.

### Build Scripts & Dynamic Environments

Build scripts (`build.rs`) receive dedicated caching logic:
- A build script unit has its own independent cache key covering its sources, configuration, and build-dependencies.
- If a build script specifies `rerun-if-changed`, only modifications to those watched files will invalidate the script's execution.
- If a script watches environment variables (`rerun-if-env-changed`), only changes to those specific variables invalidate the output.
- **Relocatable Outputs**: Script outputs frequently contain absolute paths (such as `OUT_DIR`). Rune normalizes these paths into portable placeholders so that artifacts can be safely shared across different directories and workstations.

### Correctness Always Comes First

In Rune, correctness is non-negotiable. Reusing an invalid artifact can cause silent bugs, so Rune enforces strict invariants:
- **Immutable Objects**: Stored artifacts are marked read-only and committed via atomic rename operations.
- **Verification on Restore**: Artifact sizes and metadata headers are verified on every cache hit.
- **Diagnostic Replay**: Compiler warnings emitted during initial compilation are persisted with the artifact and replayed seamlessly on cache hits.

## Instant `check` After `build`

A common frustration with Cargo is that running `cargo check` right after `cargo build` recompiles crates because check and build use different internal targets.

Rune eliminates this redundant work. When you run `rune check` after a build:
1. Rune identifies that the complete library artifacts already exist in the store.
2. The type-checker reuses the compiled `.rmeta` metadata directly.
3. Cached diagnostics are replayed immediately.

On Rune's own 181-unit workspace, running `check` after `build` completes in **0.3 seconds** with 0 compiled units (compared to Cargo recompiling 148 units over 6 seconds).

## Safe Multi-Process Concurrency

Whether you are running a terminal build while your IDE performs background checks, or multiple CI jobs run on the same machine, Rune ensures complete process safety:
- **No Global Target Locks**: Rune does not lock the entire target directory. Independent compilation units execute concurrently without blocking.
- **Per-Unit Advisory Locks**: When two processes attempt to build the exact same compilation unit simultaneously, one process compiles while the other waits and retrieves the completed result from the store.
- **Atomic Materialization**: Every output file is written to a unique temporary path and atomically moved into position, preventing half-written binaries.

## Workspace Feature Unification

By default, compiling a specific package (`rune build -p core`) only enables the features required by that package. However, across large workspaces with many binaries, this can produce duplicate compilations of shared dependencies with slightly different feature flags.

Setting `build.feature-unification = "workspace"` (or Cargo's `resolver.feature-unification = "workspace"`) unifies feature requirements across all workspace members in memory. Every sub-build shares the exact same dependency compilation without requiring synthetic "workspace-hack" helper crates.

## Managing Store Size

Rune maintains its global cache automatically:
- **Disk Cap**: Automatically caps store size to 10% of available disk space (between 5 GiB and 50 GiB by default, configurable via `cas.max-size`).
- **LRU Eviction**: Evicts least-recently-used artifacts that haven't been accessed for 30 days (`cas.max-age`).
- **Compression**: Compresses idle artifacts using zstd if unaccessed for 7 days (`cas.compress-after`).
- **Workspace Pinning**: Protect your critical projects from eviction using `rune cas pin` (view active pins with `rune cas pins`).

Inspect your storage usage anytime:

```bash
rune cas du                 # View disk usage by package and age
rune cas gc --max-age 14d   # Remove artifacts unused for two weeks
rune cas gc --max-size 15GB # Prune cache down to 15 GB
```
