# Rune

**A fast, Cargo-compatible build system and package manager for Rust.**

Rune reads your existing `Cargo.toml` (including TOML 1.1: multi-line inline
tables, trailing commas), builds with `rustc` directly, and reuses compiled
artifacts through a content-addressable store (CAS). Your project keeps working
with plain `cargo`: Rune writes a Cargo-format `Cargo.lock` and understands
`.cargo/config.toml`.

- **A key is an identity, not a hash of the source** — compiler, wrappers,
  target, flags, profile, features, dependencies' keys, build-script results and
  the outside world a unit read all go into what makes an artifact reusable
- **Content-hash fingerprinting** — rebuild when contents change, not mtimes
- **rmeta pipelining** — dependents start as soon as a dependency's metadata exists
- **Content-addressable store** — immutable, verified artifacts shared across
  projects, profiles and targets
- **Critical-path scheduling** — weighted by measured compile times from earlier builds
- **Shared jobserver** — every `rustc` and build script draws from one job budget
- **Pipelined test runner** — test binaries run the moment they link

## Usage

```bash
rune build                     # or: rune b
rune build --release -j 8
rune build --target aarch64-unknown-linux-gnu
rune check
rune run -- --my-flag
rune test                      # unit, integration and doc tests
rune test my_filter -- --nocapture
rune bench
rune doc --no-deps
rune add serde -F derive
rune tree -i serde             # who depends on serde?
rune why serde
rune update                    # only re-downloads changed index entries
rune new my-app                # or: rune init
```

Cargo's selection flags work as expected: `-p/--package`, `--workspace`,
`--exclude`, `--bin`, `--lib`, `--example`, `--test`, `--bench`, `--all-targets`,
`-F/--features`, `--all-features`, `--no-default-features`, `--profile`,
`--target`, `--manifest-path`, `--locked`, `--offline`, `--frozen`,
`--ignore-rust-version`.

## What is supported

**Manifests and workspaces.** Every compilation-relevant `[package]` field,
`[workspace]` (members, exclude, default-members, `resolver` 1/2/3),
`workspace = true` inheritance for packages, dependencies, lints and metadata —
including packages that inherit from a *foreign* workspace (a git repository
with its own `[workspace]`). `[lib]` `crate-type` (`rlib`, `dylib`, `cdylib`,
`staticlib`, `proc-macro`, several at once), `[[bin]]`/`[[example]]`/`[[test]]`/
`[[bench]]` with `required-features`, `harness`, `doctest`, target
auto-discovery, `autolib`/`autobins`/….

**Dependencies.** Registry, alternative registries, git (branch/tag/rev, pinned
to an exact commit, submodules included), path, several locations at once,
renamed (`package = "..."`), target-specific (`cfg(...)` evaluated against the
real target), build- and dev-dependencies, `[patch]` and `[replace]` (from the
manifest and from configuration), source replacement (`replace-with` a
mirror, a vendor `directory` or a `local-registry`).

**Features.** The full model — `default`, optional dependencies, `dep:`, weak
`dep?/feat`, `foo/bar`, `default-features = false`, `required-features` — resolved
**per selection and per role**: `-p a` no longer enables features only `b`
asked for, and (resolver 2/3) a build script's or proc-macro's dependencies get
their own feature set, separate from the same crate compiled for the target.

**Host vs. target.** Build scripts, proc-macros and everything only they use are
compiled for the host; the rest for `--target`. `CARGO_CFG_*` describes the
target as `rustc --print=cfg` reports it (custom target JSONs are keyed by
their normalized content).

**Build scripts.** The full `cargo::` contract: `rustc-link-lib/search/arg*`,
`rustc-cdylib-link-arg`, `rustc-flags`, `rustc-cfg`, `rustc-check-cfg`,
`rustc-env`, `metadata`/`DEP_*`, `warning`, `error`, `rerun-if-*`; the real
environment (`TARGET`, `HOST`, `OUT_DIR`, `CARGO_PKG_*`, `CARGO_FEATURE_*`,
`CARGO_ENCODED_RUSTFLAGS`, …); one run per package shared by its library,
binaries and tests; `links` conflicts detected.

**Toolchain and configuration.** `RUSTC`, `RUSTC_WRAPPER`,
`RUSTC_WORKSPACE_WRAPPER`, `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`,
`rust-toolchain(.toml)` (resolved through `rustup`), and the hierarchical
`.cargo/config.toml`: `[build]` (`rustc`, wrappers, `rustflags`, `target`,
`rustdocflags`), `[target.<triple|cfg>]` (`linker`, `runner`, `rustflags`),
`[env]`, `[registries]`, `[source]`, `[patch]`, tokens and
`credential-provider = "cargo:token-from-stdout …"`.

**Profiles.** `dev`/`release`/`test`/`bench`, custom profiles with `inherits`,
`[profile.*.package.<name|*>]` and `[profile.*.build-override]` (by default
build scripts and proc-macros compile unoptimized, as in Cargo). The key uses
the *effective* profile of each unit.

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
| `cas.verify` | `false` | Re-hash every artifact restored from the CAS (entries are always checked against their key and recorded sizes). |
| `resolve.write-cargo-lock` | `true` | Create/update `Cargo.lock` when the set of locked packages changes. An up-to-date lockfile is never touched. |
| `net.offline` | `false` | Never touch the network. |
| `test.doctests` | `true` | Run documentation tests in `rune test`. |

Each key maps to an environment variable, e.g. `RUNE_BUILD_RELEASE_REBUILD_ALL=1`.
`RUNE_HOME` moves everything Rune keeps outside your project (settings, CAS,
registry cache, git checkouts), which is also how the test suite isolates itself.

## How caching keys work

A unit's key is its complete identity:

```
UnitKey = hash(
    package + source identity           registry+checksum · git commit+submodules · local files
  + target kind, crate types, edition, features, host/target role
  + compiler (executable, version, driver) + RUSTC_WRAPPER + RUSTC_WORKSPACE_WRAPPER
  + effective rustflags, linker, lints, [env]
  + the unit's effective profile (after package/build overrides)
  + how each dependency is linked (alias, normal/build) and every dependency's own key
  + values of the external inputs it declared (below)
)
```

* **Registry and git packages are immutable**, so their key never reads the
  disk; registry packages are identified by *index + checksum*, git packages by
  the *resolved commit* (a branch name never enters a key).
* **Local packages** are hashed package-wide — every file except `.rune/`,
  `target/`, VCS and tool directories, editor droppings, nested packages, lock
  files, `Rune.toml`, and `Cargo.toml` itself (whose semantics are in the key
  already). Hidden files *are* inputs, since a crate may `include_str!` them. A
  size+mtime cache means warm runs only `stat` the tree.
* **The outside world.** After a unit is built, Rune records what it read beyond
  its source: a build script's `rerun-if-env-changed`/`rerun-if-changed`, the
  native libraries it linked, `env!`/`option_env!` variables (from rustc's
  dep-info), the manifest if a crate includes it, and — for units with a build
  script — the identity of the C toolchain. The key then follows the *current
  values* of those, so an artifact is only found again when everything it read is
  unchanged.
* **Correctness beats hit rate.** A false rebuild is acceptable; reusing a wrong
  artifact is not. Objects in the store are immutable (first writer wins),
  read-only, published atomically, and verified on every hit.
* Compiler warnings are stored with the artifact and replayed on cache hits.

## Known limits

Rune targets Cargo compatibility, so these are the things it does **not** do (yet):

- **Only sparse registries.** Git-based registry indexes (`registry+https://…`)
  and replacing a source *with a git source* are rejected with an explanation.
- **Credential providers** other than a plain token and
  `cargo:token-from-stdout` (e.g. OS keychains) are not implemented.
- **Undeclared external reads.** Inputs are learned from what a unit declares.
  A build script that reads the outside world without declaring it (no
  `rerun-if-*`) is treated like Cargo treats it, which cannot be detected
  without tracing or sandboxing — planned.
- **The resolver is greedy**, seeded from `Cargo.lock`: it picks the newest
  version that fits and does not backtrack to find a set Cargo's solver would
  find in unusual conflict situations.
- **Git dependencies need `git` on `PATH`** (Rune uses your own git, so your
  credential helpers and `insteadOf` rules just work).
- **Structured diagnostics.** Warnings are replayed as rendered text.
- Linux is the platform this was developed and tested on; Windows and macOS
  have not been exercised.
- `publish`, `audit`, `outdated` and `deny` are not implemented.

See [ROADMAP.md](ROADMAP.md) for the full status against the compatibility
specification and what comes next.

## Architecture

```
rune CLI
├── rune-manifest     (Cargo.toml: TOML 1.1, profiles + overrides, lints, targets, workspaces)
├── rune-resolver     (version resolution, lockfiles, per-build feature resolver, [patch], tree/why)
├── rune-registry     (sparse index, registries & source replacement, git dependencies, downloads)
├── rune-graph        (dependency DAG with normal/build edges, critical path, pruning)
├── rune-fingerprint  (content hashing, mtime+size cache)
├── rune-scheduler    (parallel executor, jobserver, pipelining, declared inputs)
├── rune-compiler     (direct rustc invocation, build scripts)
├── rune-cas          (immutable content-addressable store, atomic writes, LRU GC)
└── rune-common       (shared types, settings, toolchain, .cargo/config.toml, cfg evaluation)
```

`rune build --timings` (or `RUNE_TIMINGS=1`) prints how long each phase takes.

## Building Rune

```bash
cargo build --release      # or: rune build --release
cargo test --workspace     # or: rune test
```

## License

MIT OR Apache-2.0
