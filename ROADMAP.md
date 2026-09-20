# Rune roadmap

Rune is a Cargo-compatible build system whose central idea is that **a cache
key is the complete identity of a compilation unit**, not a hash of its source
(see `new-spec.md`). This document records what Rune can do **today**, how that
maps onto every section of the specification, what is still missing, and the
order in which to close the gaps.

Legend: ✅ done and verified (by automated tests unless a note says otherwise) · 🟡 partial (what exists and what is
missing is stated) · ⬜ not started.

_Status as of the spec-conformance update (Sep 2026). Everything below was
developed and verified on Linux x86_64; Windows and macOS have not been run._

---

## 1. What Rune can do today

**Build orchestration**
- Direct `rustc` invocation, parallel critical-path scheduling weighted by
  measured compile times, `.rmeta` pipelining, a shared GNU jobserver for
  `rustc`, build scripts and the C compilers they drive.
- `build`, `check`, `test` (unit, integration, bin, example, doc tests; test
  binaries run the moment they link), `run`, `bench`, `doc`, `clippy`, `fmt`,
  `install`, `clean`, `tree`, `why`, `add`, `remove`, `update`, `fetch`,
  `generate-lockfile`, `metadata`, `locate-project`, `config`, `cas`, `lockfile`.
- Cargo's selection flags, `--locked/--offline/--frozen`, `--target`,
  `--ignore-rust-version`, `--timings`.

**Manifests, workspaces, targets** — the full compilation-relevant manifest;
workspace inheritance (also from *foreign* workspaces); all target kinds and
options; `[lib] crate-type` with several crate types per unit; target
auto-discovery incl. `autolib`; `[lints]`; profiles with `inherits`,
`[profile.*.package.*]`, `[profile.*.build-override]`, `split-debuginfo`.

**Dependencies** — registry, alternative sparse registries (with tokens), git
(pinned to a commit, submodules), path, renamed, target-specific, build/dev;
`[patch]`/`[replace]` from manifest and config; source replacement
(`replace-with` a mirror, vendor directory or local registry); MSRV-aware
resolution (resolver 3) and `rust-version` enforcement.

**Features & roles** — Cargo's feature model resolved per selection and per
role (target/host, resolver 1/2/3), dev-dependencies only when dev targets are
built, target-gated dependencies evaluated against the exact `cfg` set of the
platform being built.

**Cross-compilation** — separate host units for build scripts, proc-macros and
their dependencies; `CARGO_CFG_*` from the real target; per-target output
directories; `target.<t>.rustflags/linker/runner`; custom target JSON keyed by
normalized content.

**Build scripts** — the complete `cargo::` directive set and environment;
captured stderr; `cargo::error`; one run per package shared by all its units;
`links` conflict detection; immutable-source enforcement.

**Cache correctness** — complete unit identity (see the README), external
inputs learned from build scripts and `env!`, native toolchain identity,
compiler + wrappers identity, immutable read-only atomic CAS objects verified on
every hit, warnings replayed on hits.

**Lockfile** — a Cargo-format `Cargo.lock` maintained like Cargo does (created
if absent, updated when the package set changes, never touched when current).
For Rune's own 229-package workspace it matches Cargo 1.98's output in package
set, dependency edges, ordering and format (a regenerated lock differs only
where crates.io has published newer releases than the ones pinned).

**Proof points**
- ~210 automated tests (`cargo test --workspace` and `rune test` both pass),
  including end-to-end tests for build scripts, host/target roles, features per
  selection, git (tag/branch/rev/submodules/foreign workspaces), patches,
  local/vendored/authenticated registries, wrappers, runners, config, CAS
  immutability and external-input invalidation.
- Rune builds and tests **itself** (182 units, 208 tests) from an empty CAS
  against live crates.io, restores everything from the CAS after `rm -rf
  .rune/target` (0 compiled), and a no-op build takes ~0.02 s.

---

## 2. Conformance matrix (`new-spec.md`)

| § | Topic | Status | Notes |
| --- | --- | --- | --- |
| 1 | Compatibility goal | 🟡 | Broad and verified on Rune's own graph; an ecosystem-wide sweep (top-N crates) is milestone M2. |
| 2 | Manifest fields | ✅ | Compilation-relevant fields honoured; packaging-only fields (`include/exclude/publish/keywords/…`) are parsed and ignored. |
| 3 | Workspaces | ✅ | members/exclude/default-members/resolver, `[workspace.package/dependencies/lints/metadata]`, `[patch]`, `[replace]`, `[profile]`. |
| 4 | Package targets | ✅ | All target kinds/options; `doc = false` honoured by `rune doc`. |
| 5 | Crate types | ✅ | Several per unit; part of identity. |
| 6 | Dependency types | ✅ | Normal/build/dev are distinct contexts; build deps compile for the host. |
| 7 | Dependency sources | ✅ | Registry, alternative registries, git, path, multi-location, renamed, target-specific (+build). |
| 8 | Git dependencies | ✅ | Resolved to a commit, submodule state in the identity, `rune update` moves the pin. Needs `git` on `PATH`. |
| 9 | Registry package identity | ✅ | `index + name + version + checksum`; never reads the disk to key. |
| 10 | Local package identity | ✅ | Package-wide, hidden files included; exclusions are the known non-inputs only. |
| 11 | Local hash optimisation | ✅ | size+mtime cache: warm = walk+stat. |
| 12 | Manifest semantics | ✅ | `Cargo.toml` is not hashed; aliases, kinds, features, profile, lints, meta are in the key; a manifest read via `include_str!`/build script becomes a declared input. |
| 13 | Features | ✅ | Complete model; resolver 1/2/3 (3 = 2 plus MSRV-aware version choice). |
| 14 | Resolution | 🟡 | Greedy, seeded by the lock; patches, MSRV, multiple versions, renames, platform gating done. **No backtracking** (M2). |
| 15 | Merkle dependency keys | ✅ | Dependency *keys* (incl. external inputs) feed dependents. |
| 16 | Host vs target | ✅ | Distinct host units (build scripts, proc-macros, their deps) with their own features/profile/flags. |
| 17 | Target support | 🟡 | Any rustc target (delegated to rustc), exact `cfg`, target rustflags/linker/runner ✅. Custom JSON targets: identity implemented, end-to-end unverified (needs nightly `build-std`). |
| 18 | `cfg` configuration | ✅ | From `rustc --print=cfg` for the target and flags; `rustc-cfg`/`rustc-check-cfg` supported. |
| 19 | Build scripts | ✅ | All directives, environment, captured output, shared run, cached results. |
| 20 | Build script outputs | ✅ | Generated files restored from the CAS; result identity via declared inputs. |
| 21 | Input tracking | 🟡 | `rerun-if-*`, native libs, tools ✅. **Undeclared reads are not detected** — needs tracing/sandbox (M1). |
| 22 | Non-hermetic builds | 🟡 | Declared external state is keyed; there is no explicit "cannot prove ⇒ never share" class yet (M1). |
| 23 | Proc macros | ✅ | Host artifacts with their own key. |
| 24 | Native code | 🟡 | Works through build scripts (`cc`/`cmake`/`bindgen`/`pkg-config`). Identity covers `$CC` and `$AR` only. |
| 25 | Native libraries | ✅ | `links`, `rustc-link-*`, `DEP_*`; system library files fingerprinted. |
| 26 | `DEP_*` metadata | ✅ | Reproduced; flows into dependents through the dependency's key. |
| 27 | Compiler identity | ✅ | Path, `-vV`, `librustc_driver` fingerprint. |
| 28 | `RUSTC` | ✅ | Also `build.rustc`, `rust-toolchain(.toml)` via `rustup` (the latter verified by hand, not yet in the test suite). |
| 29 | `RUSTC_WRAPPER` | ✅ | Also `build.rustc-wrapper`; part of identity. |
| 30 | `RUSTC_WORKSPACE_WRAPPER` | ✅ | Members only; dependencies' artifacts survive changing it. |
| 31 | Effective rustc flags | ✅ | `CARGO_ENCODED_RUSTFLAGS` > `RUSTFLAGS` > `target.*.rustflags` > `build.rustflags`; host units ignore the first/second/last when cross-compiling. |
| 32 | Lints | ✅ | `[lints]`, workspace lints, in the key. |
| 33 | Diagnostics | 🟡 | Rendered warnings stored with the artifact and replayed. Not structured (level/location/spans) yet (M4). |
| 34–35 | Profiles & overrides | ✅ | Package/`"*"`/`name@ver`/build-override with Cargo's precedence; `split-debuginfo`. Newer profile keys (`trim-paths`, …) ⬜. |
| 36 | Incremental | ✅ | Local-only, never in the CAS. |
| 37 | Environment | ✅ | Standard `CARGO_*`, `TARGET`, `HOST`, `OUT_DIR`, `PROFILE`, `OPT_LEVEL`, `DEBUG`, `DEP_*`, `CARGO_BIN_EXE_*`. (`DEBUG_ASSERTIONS` is not a Cargo variable; `CARGO_CFG_DEBUG_ASSERTIONS` is set.) |
| 38 | `env!`/`option_env!` | ✅ | Read from rustc's dep-info; values are part of the key. |
| 39 | Linker configuration | ✅ | `target.<t>.linker`, `runner` (used by `run` and `test`). |
| 40 | Custom targets | 🟡 | See §17. |
| 41 | Cargo/Rune configuration | 🟡 | `[build] [target] [env] [registries] [source] [patch]` ✅. `[profile]` in config, config `include`, OS credential providers ⬜. |
| 42 | Source replacement | 🟡 | registry/directory/local-registry ✅; replacement by a **git source** ⬜. |
| 43 | Alternative registries | 🟡 | Sparse index, `config.json` download templates, tokens (env/config/`token-from-stdout`) ✅. Git-based indexes and OS keychain providers ⬜. |
| 44 | Dependency overrides | ✅ | `[patch]`, `[replace]`, config patches. "Unused patch" warnings ⬜. |
| 45 | Lockfile semantics | ✅ | Lockfile is not hashed; the resolved graph is. |
| 46 | `links` constraints | ✅ | Conflicts reported like Cargo. |
| 47 | Jobserver | ✅ | GNU jobserver shared with rustc and scripts. |
| 48 | Build output isolation | ✅ | `OUT_DIR` wiped before a script re-runs; a script that modifies an immutable source fails the build. |
| 49 | CAS immutability | ✅ | First writer wins; objects read-only. |
| 50 | Hardlink materialisation | ✅ | Never written through; stale outputs unlinked before recompiling; script `OUT_DIR` unlinked before re-running. |
| 51 | Atomic writes | ✅ | Files, manifests and values are temp+rename; entry becomes visible when its manifest lands. |
| 52 | Verification | ✅ | Key + recorded sizes on every hit; `cas.verify` re-hashes. |
| 53 | Object layout | ✅ | `objects/<xx>/<key>/{manifest.json,artifacts,tree/,value.json}` (sharded; not literally `sha256/`). |
| 54 | Build result structure | 🟡 | Manifest stores key, package, artifacts; the identity *breakdown* (compiler/target/profile/…) is not recorded for inspection (M4). |
| 55 | Complete unit key | ✅ | See the README; every listed component is present. |
| 56 | Recursive keys | ✅ | |
| 57 | Diagnostics as part of result | 🟡 | See §33. |
| 58 | Rustdoc | 🟡 | `rune doc` uses the toolchain's rustdoc/flags/target and skips crates whose identity (unit key + rustdoc + flags) is unchanged. Docs are not stored in the shared CAS. |
| 59 | Documentation tests | ✅ | Separate units via `rustdoc --test`. |
| 60 | Tests | ✅ | |
| 61 | Benchmarks | ✅ | `rune bench` (libtest benches need nightly, as in Cargo). |
| 62 | Examples | ✅ | |
| 63 | Effective profile | ✅ | |
| 64 | MSRV | ✅ | `rust-version` enforced (`--ignore-rust-version` overrides); resolver 3 prefers compatible versions. |
| 65 | Toolchain selection | ✅ | `rust-toolchain(.toml)` through `rustup which rustc`; toolchain identity in the key. |
| 66 | Environment-dependent builds | ✅ | Only declared/read variables are keyed; the whole environment is not. |
| 67 | Native tool identity | 🟡 | `cc`/`ar` (path, size, mtime, `--version`); `cmake`, `nasm`, `pkg-config`, `ld`/`lld`, SDKs ⬜ (M1). |
| 68 | Environment vs identity | ✅ | |
| 69 | Lookup algorithm | ✅ | A hit is an identity match. |
| 70 | Correctness rule | ✅ | False rebuilds are accepted, false hits are not. |
| 71 | What is not globally shared | ✅ | Incremental state and mutable directories stay local. |
| 72–73 | Architecture & principle | ✅ | |

---

## 3. Known gaps, and how to close them

**G1 — Undeclared external reads (§21, §22, §67).** A build script that reads
`/etc/…`, a system header or a tool without saying so is treated as Cargo treats
it. *Plan:* an opt-in `--sandbox` mode running scripts under `bwrap`/`unshare`
(Linux) or an `LD_PRELOAD`/`ptrace` file-access recorder, feeding observed reads
into `DeclaredInputs`; classify units whose reads cannot be determined as
non-hermetic (never stored globally). Extend native tool identity to `cmake`,
`nasm`, `pkg-config`, the linker and platform SDKs.

**G2 — Resolver fidelity (§14).** Greedy resolution picks the newest fitting
version and only errors when nothing fits. *Plan:* PubGrub-style backtracking,
and a differential test that resolves the top-N crates on crates.io against
`cargo generate-lockfile`. (Today's evidence: Rune's own 229-package lock matches
Cargo's.)

**G3 — Registry breadth (§41–43).** Git-based indexes, replacement by a git
source, OS keychain credential providers, config `include`, `[profile]` in
config. `rune publish`, `yank`, `owner`, `login`.

**G4 — Structured diagnostics and build-result records (§33, §54, §57).** Store
the JSON diagnostics (level, code, spans) in the CAS object, replay them
faithfully (incl. `--message-format`), and record the identity breakdown for
`rune cas inspect <key>` ("why did this miss?").

**G5 — Documentation in the CAS (§58).** Per-crate documentation trees with
`--merge=none`/`--parts-out-dir` (nightly today) so docs can be shared across
projects like compiled artifacts.

**G6 — Platforms.** Windows/macOS: path handling, `.exe`/`.dll`/`.dylib`
artifact names, `rpath`, SDK identity, CI matrix.

**G7 — Custom targets end to end (§17, §40).** A nightly CI job building a
`no_std` project for a JSON target with `-Zbuild-std`.

**G8 — Newer Cargo surface.** `trim-paths`, `hint-mostly-unused`, `-Z`
unit-graph output, `--message-format`, `--future-incompat-report`, `cargo
vendor`/`rune vendor`, `[hints]`.

---

## 4. Milestones

| | Goal | Delivers |
| --- | --- | --- |
| **M1** | Hermeticity | G1: sandboxed/recorded build-script reads, non-hermetic class, full native tool identity. |
| **M2** | Ecosystem confidence | G2 + a compatibility corpus: build the top ~500 crates (and their tests) with Rune vs Cargo in CI; fix every divergence; backtracking resolver. |
| **M3** | Registry & tooling | G3 + `publish`/`audit`/`outdated`/`deny`/`coverage` (the tooling promised in `spec.md`). |
| **M4** | Observability | G4 + `rune cas inspect`, `rune explain-miss`, structured diagnostics. |
| **M5** | Sharing | Remote/read-through CAS for CI and teams; G5 shared documentation. |
| **M6** | Platforms | G6 + G7 + CI on Windows/macOS. |

---

## 5. How to verify the current state

```bash
cargo test --workspace                       # ~210 tests, no network needed
cargo clippy --workspace --all-targets

# Rune builds and tests itself against live crates.io (use a scratch copy
# and a scratch RUNE_HOME on a disk-backed path; /tmp may be RAM):
export RUNE_HOME=~/.cache/rune-scratch/home
cp -r Cargo.toml Cargo.lock crates ~/.cache/rune-scratch/selfhost && cd ~/.cache/rune-scratch/selfhost
rune build && rm -rf .rune/target && rune build   # second run: "0 compiled"
rune test --workspace
diff Cargo.lock /path/to/rune/Cargo.lock          # must be identical

# Lockfile fidelity against Cargo (differences should only be newer releases):
cargo generate-lockfile && cp Cargo.lock /tmp/cargo.lock
rm Cargo.lock && rune generate-lockfile && diff /tmp/cargo.lock Cargo.lock
```

Environment variables that isolate experiments: `RUNE_HOME` (settings, CAS,
registry cache, git checkouts).

---

## 6. What changed in the spec-conformance update

Fixed along the way (all covered by tests): `rune new` scaffolded an unbuildable
project (wrote the manifest to `Rune.toml`); `[lib] crate-type` was silently
ignored (`cdylib`/`staticlib` never built); `autolib` was misspelt; build
scripts saw the *host's* `CARGO_CFG_*` and empty `CARGO_PKG_*`; a re-run build
script could write through hard links into the shared CAS; resolution depended
on the command line, and `-p a` unified features requested only by `b`.
