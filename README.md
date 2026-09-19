# Rune

**A fast, Cargo-compatible build system and package manager for Rust.**

Rune reads your existing `Cargo.toml` (including TOML 1.1: multi-line inline
tables, trailing commas), builds with `rustc` directly, and reuses compiled
artifacts through a content-addressable store (CAS). Your project keeps working
with plain `cargo`.

- **Content-hash fingerprinting** — rebuild when contents change, not mtimes
- **rmeta pipelining** — dependents start as soon as a dependency's metadata exists
- **Content-addressable store** — reuse artifacts across projects and profiles
- **Critical-path scheduling** — weighted by measured compile times from earlier builds
- **Shared jobserver** — every `rustc` and build script draws from one job budget
- **Pipelined test runner** — test binaries run the moment they link

## Usage

```bash
rune build                     # or: rune b
rune build --release -j 8
rune check
rune run -- --my-flag
rune test                      # unit, integration and doc tests
rune test my_filter -- --nocapture
rune bench
rune add serde -F derive
rune tree -i serde             # who depends on serde?
rune why serde
rune update                    # only re-downloads changed index entries
rune new my-app                # or: rune init
```

Cargo's selection flags work as expected: `-p/--package`, `--workspace`,
`--exclude`, `--bin`, `--lib`, `--example`, `--test`, `--bench`, `--all-targets`,
`-F/--features`, `--all-features`, `--no-default-features`, `--profile`,
`--target`, `--manifest-path`, `--locked`, `--offline`, `--frozen`.
`[profile.*]` (including `inherits`), `[lints]`, `[workspace.*]` inheritance and
target auto-discovery (`src/bin/*`, `examples/`, `tests/`, `benches/`) follow Cargo.

## Settings: `rune config`

Settings can be **per project** (`Rune.toml` next to `Cargo.toml`; Cargo ignores
it) or **global** (`~/.rune/config.toml`). Precedence, lowest to highest:
default → global → project → `RUNE_*` environment variable → command-line flag.

```bash
rune config list --show-origin
rune config set build.release-rebuild-all true            # this project
rune config set build.release-rebuild-all true --global   # every project
rune config get build.release-rebuild-all
rune config unset build.release-rebuild-all
```

| Key | Default | Meaning |
| --- | --- | --- |
| `build.release-rebuild-all` | `false` | **Off:** release builds reuse the CAS like dev builds. **On:** every crate, dependencies included, is recompiled for release profiles (results are still stored). `--rebuild-all[=true\|false]` overrides it for one run. |
| `build.jobs` | CPUs + ⅓ | Parallel compiler jobs. |
| `build.target-dir` | `.rune/target` | Build output directory. |
| `cas.enabled` | `true` | Use the CAS at all. |
| `cas.dir` | `~/.rune/cas` | CAS location. |
| `cas.max-size` | unlimited | Evict least-recently-used entries after a build when the CAS grows past this (`20GB`, `512MiB`). |
| `net.offline` | `false` | Never touch the network. |
| `test.doctests` | `true` | Run documentation tests in `rune test`. |

Each key maps to an environment variable, e.g. `RUNE_BUILD_RELEASE_REBUILD_ALL=1`.
`RUNE_HOME` moves everything Rune keeps outside your project (settings, CAS,
registry cache), which is also how the test suite isolates itself.

## How caching keys work

Every unit's key covers its identity, features, target, every codegen-relevant
profile setting (opt-level, LTO, panic, codegen-units, ...), the compiler
(including a `RUSTC` wrapper such as `clippy-driver`), `RUSTFLAGS`, lint flags,
and its dependencies' keys. Registry crates are immutable, so their key never
reads the disk. Local packages are hashed package-wide (all files except build
output, hidden and nested-package directories, lock files, and `Rune.toml`), with
a size+mtime cache so warm runs only `stat` the tree. Compiler warnings are
stored with the artifact and replayed on cache hits.

## Compatibility notes and known limits

- Not supported yet: git dependencies, alternative registries, `[patch]`
  (git/registry sources fail with a clear error rather than resolving wrongly).
- Cross-compiling (`--target`) builds build scripts and proc-macros for the host
  but does not yet build *their* dependencies separately for the host.
- Feature unification is workspace-wide: `-p a` builds dependencies with the
  union of features requested by *all* members, where Cargo (resolver 2) unifies
  only over the selected packages.
- `Cargo.lock` is read to seed versions but is never rewritten; Rune keeps its own
  `Rune.lock.bin` (`rune lockfile export` produces a `Cargo.lock`).
- Local-package cache keys use a directory-wide hash rather than rustc dep-info,
  so unrelated changed files inside a package directory cause a rebuild of that
  package (hidden files and editor swap files are ignored).

## Architecture

```
rune CLI
├── rune-manifest     (Cargo.toml: TOML 1.1, profiles, lints, target discovery)
├── rune-resolver     (registry resolution, feature unification, lockfiles, tree/why)
├── rune-registry     (sparse index with ETags, parallel index + crate downloads)
├── rune-graph        (dependency DAG, critical path, pruning)
├── rune-fingerprint  (content hashing, mtime+size cache)
├── rune-scheduler    (parallel executor, jobserver, pipelining)
├── rune-compiler     (direct rustc invocation, build scripts)
├── rune-cas          (content-addressable store, atomic writes, LRU GC)
└── rune-common       (shared types, settings, timing)
```

`rune build --timings` (or `RUNE_TIMINGS=1`) prints how long each phase takes.

## Building Rune

```bash
cargo build --release      # or: rune build --release
cargo test --workspace     # or: rune test
```

## License

MIT OR Apache-2.0
