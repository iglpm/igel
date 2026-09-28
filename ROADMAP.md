# Rune roadmap

Rune is a Cargo-compatible build system whose central idea is that **a cache
key is the complete identity of a compilation unit**, not a hash of its source
(see `new-spec.md`). This document records what Rune can do **today**, how that
maps onto every section of the specification, what is still missing, and the
order in which to close the gaps.

Legend: ✅ done and verified (by automated tests unless a note says otherwise) · 🟡 partial (what exists and what is
missing is stated) · ⬜ not started.

_Status as of the build-speed and tooling update (§12), which follows the
daily-driver update (§11). §1–§11 were developed and verified on Linux x86_64
and macOS aarch64 (Apple M5); §12 on Linux x86_64 (4 cores) — its notes say what
to expect on a larger Mac. Windows has not been run._

---

## 1. What Rune can do today

**Build orchestration**
- Direct `rustc` invocation, parallel critical-path scheduling weighted by
  measured compile times, `.rmeta` pipelining, a shared GNU jobserver for
  `rustc`, build scripts and the C compilers they drive. Build scripts run as
  their own step, overlapping their package's dependencies, and rerun only when
  what they watch changes; no new compiler starts while memory is short; a build
  that left cores idle names the chain of crates responsible (§12).
- `build`, `check`, `test` (unit, integration, bin, example, doc tests; test
  binaries run the moment they link), `run`, `bench`, `doc`, `clippy`, `fmt`,
  `install`, `clean`, `tree`, `why`, `add`, `remove`, `update`, `fetch`,
  `generate-lockfile`, `metadata`, `locate-project`, `config`, `cas`, `lockfile`,
  `explain`, `graph`, `outdated`, `upgrade`, `unused`, `sbom` (§12).
- Cargo's selection flags, `--locked/--offline/--frozen`, `--target`,
  `--ignore-rust-version`, `--timings` (with a Chrome trace of the schedule),
  `--deps-only`, `--message-format json`.
- Several `rune` processes on one project at once: no directory lock, atomic file
  replacement, per-unit locks, results shared through the store (§8).
- `rune doctor` (toolchain/linker/profile/machine diagnosis), `rune cas du`,
  `rune watch`, `rune affected --since [--partition N/M]`, `rune script`
  (single-file scripts), and install advice when a build script fails on a
  missing system package, opt-in cached test results, test retries with flaky
  detection and JUnit/JSON reports, `rune cas export/import` bundles for CI
  caches, and a shared store (`cas.remote`, a directory or **Cairn** servers —
  the companion project in `../cairn`) for CI and teams.

**Manifests, workspaces, targets** — the full compilation-relevant manifest, from
`Cargo.toml` or from a `Rune.toml` (a Rune-only project needs no `Cargo.toml`);
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
- 444 automated tests (`cargo test --workspace` and `rune test` both pass),
  including end-to-end tests for build scripts, host/target roles, features per
  selection, git (tag/branch/rev/submodules/foreign workspaces), patches,
  local/vendored/authenticated registries, wrappers, runners, config, CAS
  immutability and external-input invalidation, native libraries across
  dependency levels, installing, packaging and publishing against a mock
  registry API, and version unification with backtracking.
- A fresh `rune generate-lockfile` for Rune's own 229-package workspace is
  byte-identical to `cargo generate-lockfile`'s.
- Eleven binding scenarios build and run as with Cargo (§11): `cc` C/C++,
  `bindgen`, `cxx`, bundled SQLite, `libz-sys` + CoreFoundation, `pyo3`
  extension module and embedded Python, `cbindgen` + staticlib/cdylib used from C,
  `libloading` plugins.
- Rune builds and tests **itself** (183 units, 444 tests) from an empty CAS
  against live crates.io, restores everything from the CAS after `rm -rf
  .rune/target` (0 compiled, 76 ms), and a no-op build takes ~50 ms of wall
  time (Cargo: 180–230 ms) — §12.

---

## 2. Conformance matrix (`new-spec.md`)

| § | Topic | Status | Notes |
| --- | --- | --- | --- |
| 1 | Compatibility goal | 🟡 | Verified on Rune's own graph, a 469-package corpus of popular crates, and 72 crates' own test suites, all ending as under Cargo (§12); a top-500 sweep in CI is milestone M2. |
| 2 | Manifest fields | ✅ | Compilation-relevant fields honoured; packaging-only fields (`include/exclude/publish/keywords/…`) are parsed and ignored. The manifest may be `Cargo.toml` **or** `Rune.toml` (§8). |
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
| 14 | Resolution | 🟡 | Seeded by the lock (which stays the source of truth); patches, MSRV, multiple versions, renames, platform gating; one version per semver-compatible range with preference and single-step backtracking (§11). Not a full PubGrub solver (M2). |
| 15 | Merkle dependency keys | ✅ | Dependency *keys* (incl. external inputs) feed dependents. |
| 16 | Host vs target | ✅ | Distinct host units (build scripts, proc-macros, their deps) with their own features/profile/flags. |
| 17 | Target support | 🟡 | Any rustc target (delegated to rustc), exact `cfg`, target rustflags/linker/runner ✅. Custom JSON targets: identity implemented, end-to-end unverified (needs nightly `build-std`). |
| 18 | `cfg` configuration | ✅ | From `rustc --print=cfg` for the target and flags; `rustc-cfg`/`rustc-check-cfg` supported. |
| 19 | Build scripts | ✅ | All directives, environment, captured output, shared run, cached results; a separate step as in Cargo (§12). |
| 20 | Build script outputs | ✅ | Generated files restored from the CAS; a result is keyed by the script's configuration, build-dependencies, `DEP_*` values and — with `rerun-if-changed` — exactly the watched files and its own sources (§12). |
| 21 | Input tracking | 🟡 | `rerun-if-*`, native libs, tools ✅. **Undeclared reads are not detected** — needs tracing/sandbox (M1). |
| 22 | Non-hermetic builds | 🟡 | Declared external state is keyed; there is no explicit "cannot prove ⇒ never share" class yet (M1). |
| 23 | Proc macros | ✅ | Host artifacts with their own key. |
| 24 | Native code | 🟡 | Works through build scripts (`cc`/`cmake`/`bindgen`/`pkg-config`). Identity covers `$CC`, `$AR` and on macOS the developer directory, its clang and the SDK (§12). |
| 25 | Native libraries | ✅ | `links`, `rustc-link-*`, `DEP_*`; system library files fingerprinted. |
| 26 | `DEP_*` metadata | ✅ | Reproduced; flows into dependents through the dependency's key. |
| 27 | Compiler identity | ✅ | Path, `-vV`, `librustc_driver` fingerprint. |
| 28 | `RUSTC` | ✅ | Also `build.rustc`, `rust-toolchain(.toml)` via `rustup` (the latter verified by hand, not yet in the test suite). |
| 29 | `RUSTC_WRAPPER` | ✅ | Also `build.rustc-wrapper`; part of identity. |
| 30 | `RUSTC_WORKSPACE_WRAPPER` | ✅ | Members only; dependencies' artifacts survive changing it. |
| 31 | Effective rustc flags | ✅ | `CARGO_ENCODED_RUSTFLAGS` > `RUSTFLAGS` > `target.*.rustflags` > `build.rustflags`; host units ignore the first/second/last when cross-compiling. |
| 32 | Lints | ✅ | `[lints]`, workspace lints, in the key. |
| 33 | Diagnostics | 🟡 | Rendered warnings stored with the artifact and replayed. Not structured (level/location/spans) yet (M4). |
| 34–35 | Profiles & overrides | ✅ | Package/`"*"`/`name@ver`/build-override with Cargo's precedence; `split-debuginfo`, with Cargo's `unpacked` default on Apple targets (§10). Newer profile keys (`trim-paths`, …) ⬜. |
| 36 | Incremental | ✅ | Local-only, never in the CAS. |
| 37 | Environment | ✅ | Standard `CARGO_*`, `TARGET`, `HOST`, `OUT_DIR`, `PROFILE`, `OPT_LEVEL`, `DEBUG`, `DEP_*`, `CARGO_BIN_EXE_*`. (`DEBUG_ASSERTIONS` is not a Cargo variable; `CARGO_CFG_DEBUG_ASSERTIONS` is set.) |
| 38 | `env!`/`option_env!` | ✅ | Read from rustc's dep-info; values are part of the key. |
| 39 | Linker configuration | ✅ | `target.<t>.linker`, `runner` (used by `run` and `test`). |
| 40 | Custom targets | 🟡 | See §17. |
| 41 | Cargo/Rune configuration | 🟡 | `[build] [target] [env] [registries] [source] [patch] [profile] [alias]` ✅, `CARGO_PROFILE_*`, `--config KEY=VALUE`, malformed files are errors (§11). Config `include`, OS credential providers ⬜. |
| 42 | Source replacement | 🟡 | registry/directory/local-registry ✅; git dependencies replaced by a vendor directory ✅ (§11); replacement *by* a git source ⬜. |
| 43 | Alternative registries | 🟡 | Sparse index, `config.json` download templates, tokens (env/config/`token-from-stdout`) ✅. Git-based indexes and OS keychain providers ⬜. |
| 44 | Dependency overrides | ✅ | `[patch]`, `[replace]`, config patches, "unused patch" warnings (§11). |
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
| 67 | Native tool identity | 🟡 | `cc`/`ar` (path, size, mtime, `--version`); macOS developer directory, clang and SDK; Apple deployment targets and `SDKROOT` for Apple targets (§12). `cmake`, `nasm`, `pkg-config`, `ld`/`lld` ⬜ (M1). |
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

**G2 — Resolver fidelity (§14).** *Done (§11):* one version per compatible range,
settled by preference or by backtracking one requirer at a time; a fresh lock of
Rune's own graph is byte-identical to Cargo's. *Still to do:* a full PubGrub
solver for conflicts that need several crates moved at once (today: a warning and
two versions side by side), and a differential test over the top-N crates.

**G3 — Registry breadth (§41–43).** *Done (§11):* `[profile]` in config,
`publish`, `yank`, `owner`, `login`/`logout`, `search`, `info`, `install` from a
registry or git. *Still to do:* git-based indexes, replacement by a git source,
OS keychain credential providers, config `include`.

**G4 — Structured diagnostics and build-result records (§33, §54, §57).** Store
the JSON diagnostics (level, code, spans) in the CAS object, replay them
faithfully (incl. `--message-format`), and record the identity breakdown for
`rune cas inspect <key>` ("why did this miss?").

**G5 — Documentation in the CAS (§58).** Per-crate documentation trees with
`--merge=none`/`--parts-out-dir` (nightly today) so docs can be shared across
projects like compiled artifacts.

**G6 — Platforms.** *macOS (aarch64):* the full test suite passes, Rune builds
and restores itself, and Cargo's `split-debuginfo = "unpacked"` default is applied
(§10); the deployment target, SDK and developer tools are part of the identity
(§12). Still to do: `rpath`/`.dylib` end-to-end tests, a CI job. *Windows:* path handling,
`.exe`/`.dll` artifact names, CI matrix — not started.

**G7 — Custom targets end to end (§17, §40).** A nightly CI job building a
`no_std` project for a JSON target with `-Zbuild-std`.

**G8 — Newer Cargo surface.** `rune vendor` ✅ (§11); the unit graph as JSON ✅
(`rune graph --units --format json`, Rune's own schema, §12). Still: `trim-paths`
(unstable in Cargo too), `hint-mostly-unused`, Cargo's `--unit-graph` schema,
`--future-incompat-report`, `[hints]`.

---

## 4. Cargo pain points, and what Rune should do about them

Rune only earns its place if it is *noticeably* better than Cargo where Cargo
hurts. This section collects what Rust users actually complain about, checks it
against what Rune does today, and says what to build. Evidence comes from the
Rust project's own surveys and Cargo's design discussions (each entry names its source);
entries without a source are my own suggestions and are marked *(idea)*.

"Rune today" describes the code **as of the start of the 2026-09-20 work**;
the table below says what has been done since. Items marked *verify* still need
a test or benchmark before they are claimed in the README.

| Item | Status |
| --- | --- |
| P2 check ⇄ build sharing | ✅ `check` after `build` compiles nothing (Rune itself: 148 → 0 units, 6.1 s → 0.3 s); tested end to end (§10). `build` after `check` still compiles, as it must. |
| P3 concurrent processes | ✅ fixed and tested (§8): it was a real bug — 3 of 4 concurrent builds failed. |
| P4 debuginfo | ✅ dependencies default to `line-tables-only` (−10 % cold build); `split-debuginfo = "unpacked"` on macOS (−11 %, §10); `rune doctor` covers linkers. Link-step caching ⬜. |
| P5 self-diagnosis | ✅ `rune doctor`; a build that left cores idle names the chain of crates responsible; `--timings` writes an HTML timeline with that chain highlighted; `rune explain` (§12). |
| P6 disk | 🟡 `rune cas du` ✅; automatic cleanup of entries unused for 30 days (`cas.max-age`, §12) ✅; pinning, zstd ⬜. |
| P7 CI/Docker | ✅ `rune build --deps-only`; `rune cas export/import` bundles (§12). |
| P8 path in keys | 🟡 verified: keys do **not** contain the checkout path (a shared store works across paths). Artifacts still embed absolute paths in debuginfo/panics; remapping is only needed for byte-identical builds. |
| P17 machine interface | 🟡 `--message-format json` for build/check/test ✅, `rune graph --units --format json` ✅ (§12); `rune-api` ⬜. |
| P1 early cutoff | ⛔ not possible on stable (see P1): `.rmeta` changes on any edit. |
| P9 cached tests | 🟡 opt-in `--cached` ✅ (285 tests: 8 s → 0.26 s); default-on needs G1. |
| P11 unused dependencies | ✅ `rune unused [--fix]` on rustc's own lint (§12). |
| P12 upgrading | ✅ `rune outdated`, `rune upgrade [--incompatible]`; `rune add` lists the release's features (enabled ones marked, as Cargo does), its `rust-version` and publication date (§12). Licence and download trend ⬜ (not in the index). |
| P15 build-time code | 🟡 `rune tree --exec-surface`, `resolve.min-release-age` ✅ (§12); the sandbox ⬜. |
| P16 provenance | 🟡 `rune sbom` (CycloneDX/SPDX) ✅ (§12); attestation, reproducibility check ⬜. |
| P10 feature questions | 🟡 `rune tree -e features -i <crate>` says which package or feature turned each feature on (§12). Workspace-wide unification ⬜. |
| P13, P14 | ⬜ |

### 4.1 Build speed and feedback loop

**P1 — Waiting for the incremental rebuild after a small edit.** By far the most
common complaint in the 2025 compiler-performance survey (3,700+ responses; 55 %
wait more than 10 s after a change). Three causes were named: a change deep in a
workspace recompiles everything above it, linking always starts from scratch, and
derive-macro expansion is not cached.
- *Rune today:* content hashing, a ~0.02 s no-op build, `.rmeta` pipelining,
  critical-path scheduling; switching branches or reverting an edit is a CAS hit.
  But a dependent's key includes the dependency's *key*, so any edit inside a
  crate (even a private function body) changes the keys of everything above it.
- *Finding (2026-09-20): the planned fix is not possible on stable Rust.* Early
  cutoff needs a hash of a crate's *interface* that ignores body edits. The obvious
  candidate, the `.rmeta`, is not one: compiling a lib, then changing only a private
  function body — or only whitespace, or appending a comment — changes the `.rmeta`
  every time, because it embeds the source map (file hashes, spans). So a
  "dependents keep their result if the `.rmeta` is unchanged" rule can never fire.
  What would work needs compiler support (a stable, span-free interface hash, or
  `-Zbinary-dep-depinfo`-style tracking on nightly); until then dependents of an
  edited crate are rebuilt, exactly as with Cargo. What Rune *can* do: keep the
  rebuild small — measured on Rune itself, an edit to a leaf crate rebuilds in
  0.8 s and an edit deep in the graph (10 dependent units) in 1.3 s, with ~60 ms
  of Rune overhead; a no-op build takes 59 ms.

**P2 — `cargo check` and `cargo build` do not share work.** Named explicitly in
the survey ("requiring redundant compilation when developers run check multiple
times before building").
- *Rune today:* measured on a small project (9 units): `build` after `check`
  compiled 3 and restored 6; `check` after `build` likewise. The shared ones are
  the registry crates; local crates and the proc-macro are built twice.
- *Done (§10):* a `check` unit takes the stored result of the equivalent
  `build` unit when its whole dependency closure can, so `check` after `build`
  compiles nothing. The reverse is impossible: a check produces no code.

**P3 — The IDE and the terminal block each other.** 35 %+ of survey respondents
say IDE and Cargo "block one another"; the rust-analyzer issue tracker is full of
"Blocking waiting for file lock on build directory / package cache" reports, and
the standard workaround is a *second* target directory (double the disk, double
the CPU). Cargo's answer is `-Zfine-grain-locking` (nightly, still being
split into separate locks).
- *Rune today:* it was **not** safe: a probe of 4 concurrent `rune build`s on one
  project failed 3 of 4 (files deleted under a running compiler, corrupted
  build-script binaries). Fixed — see §8.
- *Done:* (a) concurrency tests (6 parallel builds; interleaved check/build);
  (b) no global lock — per-unit locks so two processes wanting the same unit wait
  for one compile and the others proceed; (c) `--message-format json` and the
  rust-analyzer setting in the README (format tested, live rust-analyzer not yet).

**P4 — Linking is slow and always from scratch; debuginfo is expensive.** The
survey measured 2–30 % faster edit cycles from `debug = "line-tables-only"`, yet
25 % of those who kept full debuginfo never use a debugger. Faster linkers
(`mold`, `lld`, `wild`) give several-fold gains on big binaries but need manual
setup in `.cargo/config.toml`.
- *Rune today:* `target.<t>.linker` and rustflags are honoured and part of the
  identity, so a user *can* opt in. **Done:** registry/git dependencies default to
  `line-tables-only` in dev (`build.dependency-debuginfo` turns it off): a cold
  build of Rune itself went from 36.7 s to 33.1 s (median of 4 interleaved runs),
  and 29.7 s → 23.9 s when the workspace crates were trimmed too. `rune doctor`
  (F1) now detects an installed `mold`/`wild`/`lld` and prints the exact config.
- *Still to do:* Optionally a
  `build.linker = "auto"` setting that picks the fastest available linker and
  keys its identity. Cache the *link step* in the CAS (key = all object keys +
  linker identity + link args) so relinking an unchanged set of objects is free.

**P5 — Nobody knows the knobs exist.** 42 % of survey respondents had tried no
build-performance mechanism at all.
- *Plan:* make the slow case self-diagnosing: after a build slower than a
  threshold, print one line ("3 units = 70 % of the time; run `rune doctor`"),
  and turn `--timings` into an HTML/Chrome-trace report with the critical path
  highlighted and a "split this crate?" hint for a single dominant unit *(idea)*.

### 4.2 Disk, CI and containers

**P6 — Target directories eat the disk.** High disk usage was the *second* most
reported problem in the 2025 State of Rust survey (22 % of respondents; slow
compilation 27 %). The Cargo team is experimenting with `-Zembed-metadata=no` to
shrink `rlib`s.
- *Rune today:* CAS objects are hardlinked into `target/`, so a project's
  `target/` is nearly free — but the CAS itself is unbounded (~16 GB on the
  author's machine); `rune cas gc --max-size` exists.
- *Plan:* `rune cas du` (by package / profile / age), gc by *last use* rather
  than size alone, `rune clean --unused-for 30d`, pinning of the current
  project's keys so gc never evicts what `Cargo.lock` needs, zstd for cold
  objects, and adopting `embed-metadata=no` once it is stable.

**P7 — CI rebuilds the world; Docker layers cannot cache dependencies.** 25 % of
CI users call performance a major problem and ~36 % of those use no cache at all.
Cargo has no `--only-deps`, and its mtime-based fingerprints break Docker layer
reuse, which is why `cargo-chef` exists (5× faster Docker builds in its author's
measurements).
- *Done:* `rune build --deps-only` (all registry/git units, no workspace
  members) — replaces `cargo-chef` and needs no recipe file; content-hash keys are
  independent of mtimes, so the layer is stable. *Still to do:* `rune cas
  export/import <bundle>` for exactly the keys a lockfile needs (CI cache
  save/restore as one file). Remote CAS is M5.

**P8 — Cache keys that contain the checkout path.** `sccache` documents that the
working directory is part of its key, so copies of a project or several `git
worktree`s (very common with parallel coding agents) never share results; and a
single shared `CARGO_TARGET_DIR` serialises them behind Cargo's lock.
- *Rune today:* keys are content-based, but *checked*: Rune passes no
  `--remap-path-prefix` / `trim-paths`, so absolute paths are baked into debuginfo
  and panic messages. Registry units live under `RUNE_HOME`, so machines with a
  different home or `RUNE_HOME` produce different bytes for the same key.
  That does not break a local CAS but **would break a remote one (M5)**.
- *Plan:* remap the workspace, `RUNE_HOME` registry/git roots and the sysroot to
  fixed prefixes (and implement `trim-paths`, G8); add a test that the same
  project at two paths yields byte-identical artifacts and one shared key.
  Worktrees then share automatically.

**P9 — Test results are never cached.** Listed in the Cargo vision post; every
`cargo test` re-runs every test even when nothing that could affect it changed.
- *Done (opt-in):* `rune test --cached` / `test.cache`. A passing libtest binary is
  remembered under blake3(binary bytes, package-directory hash, arguments,
  `RUST_*`/`CARGO_*`/`RUNE_*` environment); failures are never stored; doc-tests
  are not cached. Measured on Rune's own suite: 285 tests, ~8 s → 0.26 s when
  nothing changed. Tested end to end (code change, fixture change, arguments,
  `--cached=false`, failures).
- *Still to do:* it is only sound for tests whose inputs Rune can see, so the flag
  stays opt-in. Making it default needs G1 (recording what a test actually reads,
  via the sandbox/tracer) to classify a test binary as hermetic; then
  `[package.metadata.rune] cache-tests = true` per package could replace the flag,
  and the key could use the *recorded* environment instead of a prefix list.

### 4.3 Dependencies and resolution

**P10 — Feature unification surprises and the `workspace-hack` workaround.**
Cargo unifies features across a workspace build, so the same crate is built
several times with different features; `cargo-hakari` exists to force a
"workspace-hack" crate that unifies them (reported 50 %+ faster consecutive
builds in big workspaces), at the cost of a generated crate in every manifest.
Cargo's RFC 3692 (feature unification) is still evolving.
- *Rune today:* features are resolved per selection, so `-p a` does not pick up
  features only `b` wants; different feature sets are different units and all
  live in the CAS, so alternating between them is cheap.
- *Plan:* `rune build --unify-features=workspace` computes a hakari-style
  unification *in memory* (no extra crate, no manifest changes), and `rune
  features <crate>` explains why a feature is on ("enabled by X via Y"). Reuse
  the machinery behind `rune why`.

**P11 — Unused dependencies.** `cargo-machete` is fast but text-based (false
positives with proc-macros and build scripts); `cargo-udeps` is accurate but
needs nightly and a full compile.
- *Plan:* `rune unused` built on stable rustc's `unused-crate-dependencies` lint
  plus Rune's own dependency graph, run as part of a normal build/check so it is
  nearly free on a warm CAS. Also flags unused *features* and dev/build
  dependencies used by nothing *(idea)*.

**P12 — Upgrading is costly and finding good crates takes insider knowledge**
(both named in the Cargo vision post).
- *Plan:* `rune outdated` and `rune upgrade [--incompatible]` (with MSRV
  awareness, as resolver 3 already has), `rune add` printing licence, MSRV, last
  release, download trend and enabled features before writing the manifest, and
  an optional `rune semver-check` wrapping `cargo-semver-checks` for libraries.

**P13 — Resolver: no backtracking, error messages.** Already G2. Cargo itself is
migrating to PubGrub; adopt it for the same reason (readable "no version of X
satisfies A and B" explanations).

**P14 — Public/private dependencies and artifact dependencies.** Both are
long-running Cargo efforts (RFC 1977 public-dependency; RFC 3028 binary/artifact
dependencies).
- *Plan:* parse and preserve `public = true` and lint leaks of private types in
  public API once rustc supports it; artifact dependencies (`artifact = "bin"`)
  as a second-tier feature — Rune's unit model (host/target roles) already
  matches what they need (G8).

### 4.4 Security and trust

**P15 — Build scripts and proc-macros run with the developer's full
privileges.** Stable Cargo has no way to turn either off or confine them; the
2026 `arrayref`/`internment` incident shows the attack is real (a poisoned
proc-macro dependency ran at build time). `cargo audit` also misses *deleted*
malicious versions and has no exploitability data.
- *Rune today:* G1's sandbox is planned for **hermeticity**; declared-input
  enforcement is there for immutable sources only.
- *Plan:* promote the sandbox to a **security feature** as well as a
  correctness one: `--sandbox` runs build scripts and proc-macros with no network
  and a read-only filesystem except `OUT_DIR`, with an allow-list in
  `[workspace.metadata.rune.sandbox]` for crates that legitimately need more
  (`openssl-sys`, `bindgen`). `rune tree --exec-surface` lists every crate that
  runs code at build time (build script, proc-macro, native `-sys`). Add a
  `resolve.min-release-age` setting (do not pick a version published in the last
  N days) and a warning when a locked version no longer exists in the index.

**P16 — Provenance.** *(idea)* Because keys already record everything that went
into an artifact, Rune is well placed to emit an SBOM (`rune sbom`, CycloneDX
and SPDX) and a build attestation (in-toto/SLSA) straight from the graph,
plus `rune build --verify-reproducible` (build twice, compare bytes, report the
non-deterministic crates).

### 4.5 Extensibility

**P17 — Cargo is almost all porcelain.** The Cargo vision post says the lack of
programmatic interfaces blocks Docker, corporate and tooling integrations.
- *Done:* `--message-format json` for build/check (compiler messages replayed
  from the store on cache hits, artifact paths, `fresh`, `build-finished`).
- *Still to do:* the same for `test`, unit keys in the output, a versioned schema,
  `rune unit-graph` (units, keys, edges, cache status), `rune build-plan`, and a
  library crate (`rune-api`) with the same data. This unblocks rust-analyzer
  (P3), CI dashboards and `rune affected` (F6).

### 4.6 Priority

Cheapest and most valuable first:

1. ✅ **Concurrency tests + per-unit locks** (P3) — was a correctness bug.
2. **Path remapping / `trim-paths`** (P8) — precondition for the remote CAS and
   for worktree sharing.
3. ✅ **`rune build --deps-only`** (P7) — small, and beats `cargo-chef`.
4. ✅ **`--message-format=json`** (P3, P17); rust-analyzer recipe written, unverified live.
5. ✅ **check ⇄ build metadata sharing** (P2).
6. ✅ **`rune doctor` and `rune cas du`** (P4–P6).
7. Bigger bets: interface early-cutoff (P1), sandbox-as-security (P15), cached
   tests (P9), remote CAS (M5).

---

## 5. Feature ideas

Beyond fixing Cargo's pain points: things a Rune-shaped tool can do *because*
every unit has a complete identity. Ordered roughly by value/cost.

**F1 — `rune doctor`.** ✅ *(first version)* Inspects toolchain, linker,
debuginfo, jobs, store size/idleness, RAM-backed `RUNE_HOME`, `RUSTC_WRAPPER` and
the `git`/`cc`/`pkg-config` binaries Rune shells out to, and prints concrete
fixes (P4–P6). Every rule is unit-tested. To add: profile/opt-level advice, a
"this build was slow because…" hint after a slow build.

**F2 — `rune explain-miss <unit>` / `rune why-rebuilt`.** Diff the identity
breakdown of the last stored key against the current one: "rebuilt because
`RUSTFLAGS` changed", "…because `build.rs` read `/usr/include/foo.h`, size
differs". Needs G4's recorded breakdown. This is the single feature that makes
the cache trustworthy to users.

**F3 — Remote / read-through CAS.** ✅ *(shared directory and HTTP; see §8 and the companion project **Cairn**, `../cairn`)* Done: a shared directory; an HTTP backend with bearer tokens (read/write roles), client-side sharding by rendezvous hashing with replicas, failover and a circuit breaker; and the Cairn server (integrity checks, LRU eviction, peers with read-through and replication, metrics). Still to do: TLS in Cairn itself (today: a reverse proxy), signed manifests, S3-compatible backends, cluster-wide eviction, `rune cas export/import` ✅ (§12). A "trusted peer" mode lets a team share proc-macro and `-sys` builds, the most expensive units.

**F4 — `rune watch`.** ✅ *(polling; `-x` commands, restart on change, `--ignore`)* Rebuild/check/test on save using the same CAS, so
reverting an edit or switching branches is instantaneous (a `bacon`/`cargo-watch`
replacement that understands keys instead of polling timestamps).

**F5 — Single-file scripts.** ✅ `rune script file.rs` with a `---cargo` front-matter
block for dependencies (the format Cargo is stabilising), compiled through the
normal pipeline and cached in the CAS; `#!/usr/bin/env rune` shebang. Runs in
milliseconds after the first time.

**F6 — `rune affected --since <git-ref>`.** ✅ *(package level: owner + transitive dependents; `--partition N/M` ✅ §12)* Given a diff, print the packages
whose unit keys change — exactly, not by path heuristics — so CI can build and
test only what a PR can influence, and `--partition N/M` to shard tests over CI
machines.

**F7 — A better test runner.** 🟡 *(retries with flaky marking, JUnit XML, JSON results, slowest binaries ✅ §12; scheduling single tests across binaries ⬜)* Per-test parallelism, retries and flaky-test
marking, JUnit/JSON output, slowest-tests summary, `--fail-fast` iteration mode
(all named in the Cargo vision post or done today by `cargo-nextest`). Runs
inside the pipelined runner already in `rune test`.

**F8 — `-Zbuild-std` in the CAS.** Cargo rebuilds `core`/`alloc`/`std` per
project; keyed by compiler + target + flags they are perfect CAS objects. Makes
embedded, WASM and custom-target work (G7) fast.

**F9 — Friendlier build-script failures.** ✅ *(C compiler, `pkg-config`, one missing `.pc` library, OpenSSL, `cmake`, `libclang`, `protoc`; apt/dnf/pacman/apk/zypper/brew)* When `pkg-config`, `cmake` or
a C compiler is missing, recognise the failure and say which package to install
for the detected distro ("`openssl-sys` needs `libssl-dev` / `openssl-devel`").
Among the commonest first-run failures for newcomers.

**F10 — Timings and regression tracking.** 🟡 *(Chrome trace, HTML timeline with the critical chain ✅ §12; `--compare` ⬜)* Export Chrome-trace / OpenTelemetry
spans; `rune timings --compare <ref>` to fail CI when the build gets slower by a
threshold; feed the same data into the scheduler (already weighted by measured
compile times).

**F11 — Packaging tooling.** ✅ *(§11)* `rune package --list`, `rune publish
--dry-run` that builds the packaged tarball, `rune vendor`, `rune
yank`/`owner`/`login`. `audit`, `deny`, `outdated`, `coverage` run as the usual
Cargo plugins through external subcommands; built-in versions are not planned.

**F12 — Workspace ergonomics.** 🟡 *(`rune graph` ✅ §12)* `rune graph` (DOT/Mermaid of packages and
units), `rune run -p x` from anywhere, `[workspace.metadata.rune]` for
Rune-only settings so plain `cargo` users are unaffected, `rune new --template`
(including the "mod-level templates" idea from the vision post).

**F13 — Toolchain handling.** Offer to install a missing `rust-toolchain.toml`
toolchain or target (`rustup target add`) with a clear message instead of a raw
rustc error; verify the `rust-toolchain` path in the test suite (§28, currently
checked by hand).

**F14 — Programmatic API.** `rune-api` crate exposing planning, the unit graph
and cache lookups for IDEs, Bazel-style wrappers and corporate tooling (see P17).

---

## 6. Milestones

| | Goal | Delivers |
| --- | --- | --- |
| **M1** | Hermeticity | G1: sandboxed/recorded build-script reads, non-hermetic class, full native tool identity. |
| **M2** | Ecosystem confidence | *Started (§12):* a 469-package corpus builds (faster than Cargo) with a byte-identical lockfile; 72 crates' own test suites end as under Cargo after nine fixes. *Open:* the top ~500 crates in CI; G2's full solver; two packages with the same name and version from different sources. |
| **M3** | Registry & tooling | *Done:* `install`/`uninstall`, `package`, `publish`, `login`/`logout`, `yank`, `owner`, `search`, `info`, `vendor`, `pkgid`, `verify-project`, external subcommands and aliases (§11). *Open:* the rest of G3. |
| **M4** | Observability | G4 + `rune cas inspect`, `rune explain-miss` (F2), structured diagnostics. |
| **M5** | Sharing | *Done:* shared-directory store ✅, HTTP backend ✅, **Cairn** server with peers/replication/sharding ✅, `--deps-only` ✅. `cas export/import` ✅ (§12). *Open:* signed manifests, TLS in Cairn, S3 backend; G5 shared documentation. |
| **M6** | Platforms | *Started:* macOS aarch64 test suite passes, Apple debuginfo default ✅, Apple deployment target/SDK identity ✅ (§12). *Open:* G6 (Windows, macOS SDK identity) + G7 + CI on Windows/macOS; `-Zbuild-std` in the CAS (F8). |
| **M7** | Daily driver | *Started:* P3 concurrency ✅, P4–P6 `rune doctor`/`cas du`/dependency debuginfo ✅, `--message-format=json` ✅, `--deps-only` ✅. `rune watch` ✅, scripts ✅, `rune affected` ✅, build-script hints ✅, P2 check ⇄ build ✅, `fix`/`clippy --fix`/`clippy -- flags`/`fmt -- --check`/`rustc`/`rustdoc`, `update --precise`, `add --git/--rename/--target`, status on stderr (§11). Test runner retries/flaky/JUnit/JSON, `outdated`/`upgrade`, `unused`, `graph`, `explain` ✅ (§12). *Open:* per-test scheduling, `rune features` (P10). |
| **M8** | Supply chain | *Done (§12):* exec-surface report, `min-release-age`, SBOM. *Open:* sandbox as a security boundary, attestation, reproducibility check. Builds on M1. |
| **M9** | Faster than Cargo on edits | P9 cached tests: opt-in ✅, default-on after M1. P1 early cutoff is blocked on compiler support, see P1. |

---

## 7. How to verify the current state

```bash
cargo test --workspace                       # 444 tests, no network needed
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

## 8. What changed in the performance-and-reliability update

**Reliability — concurrent processes (P3), a real bug.** Four `rune build`s
started at once on one project: three failed (`failed to open object file`,
`memory map must have a non-zero length`, `undefined hidden symbol` from a
half-written build-script binary). Causes: every process wrote the same
hash-named files into the shared `deps/` directory; "remove stale outputs, then
recompile" and "remove, then hard-link a CAS hit" both leave a window in which
the file does not exist for a compiler in another process. Fixed by (1) replacing
every materialised file atomically (link/copy to a temp name, `rename`), (2) a
per-unit advisory `flock` on the unit's output identity — only processes building
the *same unit* wait, everything else overlaps — and (3) after waiting, taking the
finished result from the store instead of compiling again. Regression tests: six
concurrent builds of an 8-crate chain with build scripts, and interleaved
`check`/`build`; both failed before the change. Measured on `serde`: 4 concurrent
builds compiled 10 units in total instead of 36 and all succeeded.

**Cold builds — what was measured (the 0.1 % CPU / 80 MB observation).** That
figure is Rune's *own* process; the compilers are children. Measured properly, a
cold build of Rune's 182 units on a 12-thread machine (i5-10400F: 6 cores + SMT)
kept the CPU at ~99 % for the first 13 s and ~80 % overall. The wall time (28.7 s)
was **equal to the length of one dependency chain** — `unicode-ident →
proc-macro2 → quote → syn → tracing-attributes → tracing → h2 → hyper →
hyper-util → reqwest → rune-registry → rune-resolver → rune` — so more jobs
cannot help once the machine is full; only shortening that chain can. Run alone
(`-j 1`) the same units take 2–3× less time than inside the parallel build (`syn`
3.6 s vs 8.8 s), which is SMT and shared-cache contention.

What was tried:

| Change | Result |
| --- | --- |
| Critical-path priority was inverted (it ranked a unit by the chain *before* it); now the longest chain *after* it, with pipelined edges costed at 40 % of the dependency. Unseen units estimated from source size instead of a flat 1 s; timings shared across projects in `<cas>/timings.json`. | **Correct, tested, no measurable cold-build gain** on this graph (median 29.6 s vs 29.6 s over 6 interleaved runs). Kept: it is what the scheduler claimed to do, and matters for graphs with a long tail. |
| Lowering the CPU priority (`nice`) of units off the critical chain. | **No gain** (SMT siblings share execution units regardless of priority). Removed again. |
| Dependencies compile with `line-tables-only` instead of full debuginfo in dev. | **−10 %** cold build (36.7 s → 33.1 s, median of 4 interleaved runs). Kept, as the default for registry/git dependencies only; `build.dependency-debuginfo = true` restores Cargo's behaviour. |
| Same, for *all* crates including the workspace's own. | −20 % (29.7 s → 23.9 s) — not done by default: it changes what you see when debugging your own code. |

Benchmark noise on this machine was ±10 % between identical runs, so every number
above is a median of interleaved runs, not a single measurement.

**New surface:** `rune build --deps-only`; `--message-format json` (Cargo-shaped
`compiler-message`/`compiler-artifact`/`build-finished`, diagnostics stored in the
CAS and replayed on hits); `rune doctor`; `rune cas du`; `--timings` writes
`.rune/fingerprints/trace.json` (Chrome trace of the schedule); tests
(332 in total).

**Second batch (same day):** `rune watch`, `rune affected`, `rune script`, install
hints for failed build scripts (see F4–F6, F9), opt-in cached test results (P9), a shared store (F3), and a fix so a `KeyUpdate` recomputes
only the units that depend on the changed one instead of every pending unit.
Measured along the way: no-op build 59 ms; edit-and-rebuild 0.8 s (leaf) / 1.3 s
(deep, 10 units); a script whose dependencies are in the store runs in 0.14 s; a
first-ever fetch of 219 crates takes ~5 s (≈2 s index, ≈3 s downloads, network
bound) — overlapping downloads with compilation could hide most of that on a truly
cold first build (~10 %), but needs the planner to work from partial information,
so it is a larger change than it looks. Early cutoff (P1) turned out to be
impossible on stable Rust (see P1).

**Shared store (F3, M5).** `cas.remote` / `cas.remote-write`: a directory (network
share, CI volume) consulted on a local miss and published to after a store, slot by
slot with the marker file last, so readers never see half an entry and any number
of writers can race (first publisher wins). Measured on Rune's own 182 units
(770 MB): a developer with an empty local store and a read-only remote restored
everything in 0.72 s (a cold build takes ~30 s), and the restored binary ran.
Publishing initially cost the CI build ~10 % (synchronous copies inside a compiler
slot); moved to a two-thread background pool flushed at the end of the build, it is
~1 % (3 runs each, inside the noise). Verified end to end that a different checkout
path and a separate `RUNE_HOME` still hit: keys contain no checkout path. The HTTP
backend and the Cairn server (next paragraphs) followed; not done: signing, S3,
eviction of a shared *directory*.

**`Rune.toml` as a manifest.** Rune's project settings file can now also be the
manifest, so a project needs no `Cargo.toml` at all. One function
(`rune_common::manifest_file::find_manifest`) decides what a directory's manifest
is — `Rune.toml` if it has a `[package]`/`[workspace]` table, else `Cargo.toml`,
`Rune.toml` winning if both are manifests — and every place that used to say
`dir.join("Cargo.toml")` (workspace roots and members, path and git dependencies,
`[patch]`, package hashing, declared inputs, `CARGO_MANIFEST_PATH`, `add`/`remove`,
`metadata`, `locate-project`, JSON messages, scaffolding) asks it, so they cannot
disagree. A settings-only `Rune.toml` beside a `Cargo.toml` is unchanged. Design
points: a broken `Rune.toml` that *looks* like a manifest is reported, not skipped
for the `Cargo.toml` beside it; a `[test]` settings table is tolerated in a
`Rune.toml` manifest (as a list, `[[test]]`, it is the test targets and
`rune config set test.*` refuses with the way out); `rune doctor` warns when two
manifests compete; `rune new/init --manifest rune`, and `init` keeps an existing
settings-only file's settings. 24 new tests (unit + end to end, including a
`Rune.toml`-only git dependency and a mixed workspace). Real check: Rune's own
workspace with all 11 `Cargo.toml` renamed — 182 units, `0 compiled` after wiping
`target`, 332 tests pass, `Cargo.lock` identical, and the renamed copy restores every
unit from the `Cargo.toml` original's cache (the file name is not in any key). Not
done: Windows path handling (as everywhere), a `Rune.toml`-aware `rust-analyzer`
recipe (rust-analyzer itself only looks for `Cargo.toml`), and registry crates
(always `Cargo.toml`, as `cargo publish` writes them).

**Cairn — the remote cache server (M5).** A new project next to Rune (`../cairn`, its
own README): one Rust binary over a directory of plain files, speaking Rune's own
layout (`GET`/`PUT /v1/o/{key}/{file}`). It refuses a marker file unless the slot it
names is complete, verifies an announced BLAKE3 hash while an upload streams, is
immutable (first writer wins, `409` otherwise), has `read`/`write`/`admin` tokens
(only hashes stored), LRU eviction, Prometheus metrics, and `verify`/`gc`/`stats`
commands. **Multi-server:** nodes are peers (`read_through` on a miss with one fetch
per key however many clients wait; `replicate` in the background with retries;
`max_hops` so mutual peers terminate), and Rune shards across servers itself
(`cas.remote = http://a,http://b`, rendezvous hashing, `cas.remote-replicas`,
failover, a 30 s circuit breaker). 46 tests, including a contract suite that drives
Rune's real client against real servers; Cairn builds and tests with Rune (146 units,
24 s, 46 tests). Measured on Rune's 182 units (811 MB): CI publishes at no visible
cost; a fresh developer restores everything in **0.65 s**; with two replicated
nodes and one killed, still 0.65 s.

Three findings from getting there — each would have shipped otherwise:
- **The restored binary was not executable.** Rewriting the shared-store code around
  a transport abstraction wrote pulled files through a fresh file, dropping the
  execute bit, so `rune run` said "Permission denied" for anything from a shared
  store (directory *or* HTTP). Fixed end to end (an `X-Cairn-Exec` header; `0555`
  vs `0444`; carried through peers) with regression tests in both projects. It was
  caught only by running the real acceptance, not by the unit tests.
- **Nagle's algorithm made restores 10× slower.** Restoring 182 units over HTTP took
  7.3 s while CPU sat idle on both sides. Cause: the server wrote response headers
  and body separately without `TCP_NODELAY`, so each small request waited ~40 ms for
  the client's delayed ACK (a big `curl` download ran at 4.4 GB/s, which hid it).
  With `TCP_NODELAY` on both ends: **0.65 s**, the same as a local directory.
- **`reqwest::blocking` is single-threaded underneath** (one I/O thread for every
  connection). A pool of blocking clients helped (7.3 → 3.4 s) but the real fix for
  scaling was a private multi-thread runtime driven with `block_on`.

**`rustc_driver` / `rustc_interface` (evaluated, not adopted).** Rune runs `rustc`
as a child process. Linking `rustc_driver` and compiling in-process was evaluated
and rejected for now:
- *Gain:* a `rustc` process costs ~9 ms to start (~15 ms for a trivial crate). Over
  Rune's 182 units that is ~1.6 s of ~231 s CPU (0.7 %), about 0.1 s of a 28 s cold
  build's critical chain, and nothing on warm builds (nothing is spawned).
- *Cost:* both crates are compiler internals (`#![feature(rustc_private)]`): they
  need a nightly and the `rustc-dev` component and change with every nightly, so
  Rune could no longer build itself or serve projects on the stable toolchain.
- *Hazards:* `env!` and proc-macros read the **process** environment, so parallel
  in-process compiles with different `CARGO_*` values could produce wrong output —
  a correctness violation (§70); diagnostics go to the process's stderr unless a
  custom `Emitter` is written against unstable internals; an ICE or an LLVM abort
  takes Rune down with it; sessions leak memory.
- *If this is ever revisited:* the only sound shape is a pool of worker processes
  each hosting `rustc_driver` behind an opt-in cargo feature, so env and stderr
  stay per-process. That saves the same ~9 ms and adds all the maintenance above.

**Not done, and why:** default-on cached tests (P9) need
the sandbox (G1) to know what a test reads; path remapping (P8)
changes what users see in backtraces and only matters once a remote CAS exists;
the sandbox (P15) is a large, platform-specific piece of work. Each is in §4 with
its plan.

---

## 9. What changed in the spec-conformance update

Fixed along the way (all covered by tests): `rune new` scaffolded an unbuildable
project (wrote the manifest to `Rune.toml`); `[lib] crate-type` was silently
ignored (`cdylib`/`staticlib` never built); `autolib` was misspelt; build
scripts saw the *host's* `CARGO_CFG_*` and empty `CARGO_PKG_*`; a re-run build
script could write through hard links into the shared CAS; resolution depended
on the command line, and `-p a` unified features requested only by `b`.

---

## 10. What changed in the macOS and check-reuse update (2026-09-24)

First run on macOS (aarch64-apple-darwin, Apple M5, 10 cores, rustc 1.98.1):
all 332 existing tests passed unchanged. A cold build of Rune itself took 13.7 s
with Rune and 11.5 s with Cargo, so Rune was ~20 % *slower* on macOS despite
cheaper dependency debuginfo.

**macOS debuginfo default (P4, G6).** Cargo compiles with
`split-debuginfo = "unpacked"` on Apple targets whenever there is debuginfo;
rustc's own default there is `packed`, which runs `dsymutil` over every binary it
links. Rune passed nothing, so it paid for `dsymutil` on every binary and test.
Now `Profile::split_debuginfo_for(target)` applies Cargo's rule (an explicit
setting always wins), the value goes to rustc for both units and build scripts,
and it is in the key, so old `packed` results are not reused. Measured: cold build
13.9 s → 12.4 s (−11 %, 3 interleaved pairs). Against Cargo, single runs after the
change gave 12.8 s and 11.8 s (Cargo 11.7 s). Noise on this machine is too high to
call the remaining gap without medians.

*Output layout.* With `unpacked`, rustc leaves each binary's object files next to
it. Binaries used to be compiled straight into `target/debug/`, which would have
filled it with `*.rcgu.o`. They now compile into `deps/`, as in Cargo, and only the
friendly name is linked into `target/debug/` (`executor::bin_dir_for`).
`--message-format json` reports that name as `executable`, like Cargo. Examples
stay in `examples/`. Test:
`binaries_compile_in_deps_and_only_their_names_appear_in_the_profile_dir`.

**`check` after `build` (P2).** Before: `check` right after `build` recompiled 148
of 181 units (6.1 s); only host-side units were shared. Now it compiles 0
(0.3 s). An edit to `rune-common` rechecks just the 10 crates above it, and
`build` → `check` → `build` → `check` stays at 0 after each switch.

The finding that shaped the design: on stable rustc, a check-only `.rmeta` and a
full build's `.rmeta` of the same crate, with identical `-C metadata` and flags,
are **not interchangeable**. The bytes differ, and a crate checked against one
fails with E0460 ("found possibly newer version of crate") when the other shows up
in the same compilation. So a naive "use the build's `.rmeta`" would break builds
whenever the store held full results for some crates but not others. The rule
implemented in `Executor::execute`:
- for every check unit, also compute the key it would have in a full build (from
  its dependencies' full keys);
- the check unit takes the full result only if that entry is in the local store
  **and** every check-mode dependency was substituted too (host-side units are the
  same in both modes);
- a substituted unit then *carries the full key*, so every dependent's key records
  which flavour of `.rmeta` it was compiled against.

Test: `check_after_build_reuses_the_build_and_still_checks_edits`. It covers a
build script's `OUT_DIR` restored for the substituted unit, a real check of an
edited crate against reused `.rmeta`s, a type error still reported, and the build's
warnings replayed by the check.

**A false alarm worth recording.** Benchmark loops sometimes took 10+ minutes
wall-clock for builds that reported ~14 s. Sampling showed `rustc` processes
"alive for 15 minutes". Cause: the laptop went into idle/maintenance sleep on
battery (`pmset -g log`). Rust's `Instant` on macOS does not count sleep, while
`ps` and the wall clock do. Not a Rune bug. Run benchmarks under `caffeinate -i`.

**Not done:** P8 path remapping (still the precondition for byte-identical shared
artifacts), macOS SDK identity, a median-of-N cold-build comparison against Cargo
on macOS.

---

## 11. What changed in the daily-driver update (2026-09-28)

Measured and developed on macOS aarch64 (Apple M5), rustc 1.98.1; the network
here moves about 1.5 MB/s, which bounds every cold number below.

**Resolver speed.** On a warm cache every command was already fast (a
re-resolution after a manifest edit: 55 ms; `tree`/`metadata`: 0.12 s, like
Cargo's 0.10 s). The slow paths were network ones:
- `rune update` took 3.2 s (Cargo: 1.7 s): the index was revalidated one
  dependency *level* at a time, 17 sequential waves. The crates the current
  lockfile names are now requested in one concurrent wave before the walk
  (`DependencyResolver::with_prefetch_hint`); the walk then reads from memory.
  Now 0.7–1.2 s.
- A cold `fetch` (empty `RUNE_HOME`, lockfile present) measured 62 s vs Cargo's
  36 s. Most of that was the bug below; with it fixed, 34 s vs Cargo's 35–38 s.
  The rest is bandwidth: `curl` alone needs 25 s for the same 221 `.crate` files,
  and the index server sends uncompressed responses whatever is asked for.

**A lockfile bug that looked like a slow resolver.** `LockfileManager::load_fast`
preferred Rune's binary cache (`Rune.lock.bin`) over `Cargo.lock`. After a
`git pull`, a checkout of an older commit or a `cargo update --precise`, the next
Rune command re-resolved from the *stale* cache and wrote the old versions back
into `Cargo.lock` — a full resolution with network traffic each time, and with
rust-analyzer running `cargo` next door, a tug of war. `Cargo.lock` is now the
source of truth; the binary cache is used only while it describes the same
packages. Test: `a_cargo_lock_changed_outside_rune_is_respected_not_reverted`.

**Resolution fidelity (G2).** Cargo allows one version per semver-compatible range
of a crate. The greedy walk took the newest version per requirement, so a stricter
requirement arriving later (an `=` pin next to a newer crate's own pin) left two
`1.x` versions in the graph. After each walk, every such range is now settled:
if one version satisfies all the requirements in force (optional dependencies only
when activated), it is preferred and the walk re-run; if none does, the requirement
that is the odd one out is found and its requirer's release excluded, so an older
release that agrees is picked. Each round reuses the in-memory index, so a graph
without conflicts costs one extra scan. A fresh lock for Rune's 229-package
workspace is byte-identical to `cargo generate-lockfile`'s; on the backtracking
test graph old Rune produced `a 1.0.0 + a 1.1.0 + b 2.0.0`, new Rune and Cargo
`a 1.0.0 + b 1.0.0`. Not a full PubGrub solver: see G2.

**Bindings.** Eleven scenarios, each compared with Cargo (the two `pyo3` ones set
up as pyo3 documents for macOS): `cc` (C and C++), `bindgen`, `cxx`, bundled
SQLite, `libz-sys` + `flate2` + CoreFoundation, `pyo3` extension module imported
from Python, `pyo3` embedding Python, `cbindgen` + `staticlib` linked from a C
program, a `cdylib` loaded with `libloading`. What was wrong:
- *Native search paths did not propagate.* A library records which native
  libraries it needs, not where they are; Cargo passes every dependency build
  script's `rustc-link-search` to each rustc invocation above it. Rune passed only
  the unit's own, so any `-sys` crate linking a **dynamic** library from a
  non-default directory failed at the final link (embedding Python:
  `library 'python3.9' not found`). Static libraries worked because they are
  bundled into the rlib.
- *`rune run`/`test` could not load them either:* only `LD_LIBRARY_PATH` was
  set (ignored on macOS). Now `DYLD_FALLBACK_LIBRARY_PATH`/`LD_LIBRARY_PATH`/`PATH`
  with build-script library directories inside the build directory, `deps/` and
  the profile directory, plus macOS's defaults.
- *No plain library names.* Cargo puts `libfoo.a`, `libfoo.dylib`/`.so`,
  `foo.dll`, `libfoo.rlib` of workspace libraries in `target/debug/`; Rune only had
  hash-named files in `deps/`.
- *Doc tests and `rune doc` lacked the build script's `OUT_DIR`, `cfg`s and
  environment:* a bindings crate (`include!(concat!(env!("OUT_DIR"), …))`)
  could not be documented, and its doc tests failed.
- *Dependency build scripts' warnings leaked* (`--cap-lints` was not passed when
  compiling them).

**Commands.** `install` from crates.io/git/path with Cargo's tracking files
(verified: `cargo install --list` and `cargo uninstall` see Rune's installs),
`uninstall`, `package`, `publish`, `login`, `logout`, `yank`, `owner`, `search`,
`info`, `vendor` (Rune and Cargo both build offline from it with empty caches),
`fix` and `clippy --fix` (applying machine-applicable suggestions from the JSON
diagnostics Rune already stores), `rustc`/`rustdoc` with extra arguments,
`pkgid`, `verify-project`, `clippy -- <lint flags>` (in the key, so a cached
warning never passes `-D warnings`), `fmt -- --check`, `update --precise
--dry-run -p`, `add --git/--branch/--tag/--rev/--rename/--target/--registry
--dry-run` (and `{ workspace = true }` when the workspace declares the
dependency), `[alias]`, external `rune-*`/`cargo-*` subcommands, `--config`,
`[profile]` in configuration and `CARGO_PROFILE_*`, unused-`[patch]` warnings.

**Behaviour fixes found on the way.** Status lines went to stdout, so
`rune vendor > config.toml` or anything piping a command's output got them mixed
in; they now go to stderr like Cargo's. A malformed `.cargo/config.toml` was
silently ignored (a vendoring or mirror setup would just not apply); it is now an
error. `rune doc --open` used `xdg-open` everywhere.

**Tests:** 353 (18 new: 12 end-to-end, 6 unit), all passing under
`cargo test` and under `rune test` on Rune's own workspace; clippy clean.

**Not done:** Windows, full PubGrub, P8 `trim-paths` (unstable in Cargo too),
structured diagnostics (G4), JSON test output, `-Z` flags, config `include`,
OS keychain credential providers.

## 12. What changed in the build-speed and tooling update (2026-09-28)

Asked for: use the whole machine (a 32 GB Apple M5 barely saw its CPU and
memory used), make Rune faster, and close roadmap items. Developed and measured
on Linux x86_64 with **4 cores** and 15 GB, rustc 1.98.1, on Rune's own
workspace unless stated. The M5 was not available: numbers there will differ,
and what to expect is said where it matters.

**What limits a build is rarely the job count.** On 4 cores a cold build of
Rune's own workspace keeps the machine ~86 % busy with Rune and with Cargo alike
(55.3–55.7 s vs Cargo's 54.4–55.0 s,
two runs each, empty store; 170 s vs 168 s of CPU, of which Rune's own process
2.6 s and Cargo's 1.3 s). With more cores, what stops a build from using them is the chain
of crates that must be compiled one after another and the one-core front end of
each large crate — no job count changes that on stable Rust. So this update
removes what made Rune's chains longer than they need to be, keeps memory from
becoming the limit, and makes the remaining limit visible:
- *Build scripts are their own step*, as in Cargo. A crate with a `build.rs`
  used to start only once every transitive dependency was compiled and
  linked, then compiled and ran its script, then itself — disabling `.rmeta`
  pipelining for `serde`, `serde_json`, `proc-macro2`, `libc`, `anyhow`,
  `rustix`, … and putting every script on the critical path. The script now
  starts once its build-dependencies are built, overlapping the package's other
  dependencies; the library starts when their metadata exists. The unit's lock
  is held across both steps; lock waits no longer occupy job slots.
- *Results are stored after dependents are released*, and the store hard-links
  the compiler's fresh outputs instead of copying them.
- *Memory*: before starting another compiler the scheduler checks available
  memory (Linux `MemAvailable`, macOS memory pressure) and while it is below a
  tenth of the machine (1–4 GiB) lets running units finish first. It never
  holds back with nothing running. A test reports memory short and sees four
  crates built one at a time (3.7 at once without the check).
- *Seeing the limit*: every step's ready/start/metadata/finish times are
  recorded, and the chain of steps that set the build's length is found. A
  build over 10 s that kept fewer than 60 % of the cores busy says so in one
  line with that chain; `--timings` prints it and writes
  `.rune/timings/rune-timing.html`.
- *Front-end threads* (`build.frontend-threads`, nightly only): `auto` gives a
  crate that starts while cores are idle as many `-Z threads` as there are idle
  cores. On 4 cores it kept the tail busy but saved nothing (47.4 s vs 47.9 s);
  with 10 cores and a long single-crate tail it has more room. Off by default.
- `build.jobs` stays CPUs + ⅓ (13 on a 10-core M5), and each `rustc` borrows
  idle jobserver slots for its codegen threads, so a lone compiler at the end
  of a build is not limited to one thread in its backend.

**Less work per command.**
- *No-op build 137 → 49 ms* (Cargo: 179 ms). Every command used to start
  `rustc` four times through the rustup proxy before doing anything, and `cc`/
  `ar` for their versions. Probes are now cached in
  `RUNE_HOME/cache/toolchains.json`, trusted only while the sysroot's `rustc`
  and `librustc_driver` are unchanged, and `rustc` is started from its sysroot
  (no proxy per unit, one compiler for the whole build even if rustup updates
  mid-build).
- *Build scripts compile beside `OUT_DIR`*, not into it: the compiled script was
  part of what the store kept for the script's result (106 MB of the 111 MB
  under `build/`). CAS after a cold build: 830 → 724 MB.
- *Build scripts rerun only when what they watch changes.* A script's result is
  keyed by its configuration, build-dependencies and `DEP_*` values, plus — when
  it declared `rerun-if-changed` — the watched files and its own sources (from
  rustc's dep-info). Editing `src/` of a crate with a `build.rs` no longer
  recompiles and reruns the script; a script that declared nothing still reruns
  on any change of its package, as with Cargo; a script that must rerun because
  a watched file changed is not recompiled. Edit-rebuild of a leaf crate with a
  build script: **0.70 s → 0.19 s** (Cargo: 0.24 s).

**Correctness.**
- rustc writes `MACOSX_DEPLOYMENT_TARGET` (and the iOS/tvOS/watchOS/visionOS
  equivalents) into every object and links against `SDKROOT` without listing
  either in its dep-info (verified: the object bytes differ, the dep-info does
  not), so an artifact built for one macOS version was reused for another. Units
  compiled for Apple targets include those variables when set.
- On macOS `/usr/bin/cc` is a shim into the active developer directory, so its
  fingerprint did not change when the Command Line Tools or Xcode were updated.
  The native tool identity now covers the developer directory, the real clang and
  the SDK (by path, size and mtime — no `xcrun` per build).
- HTTP: every client trusts the OS certificate store next to the bundled roots
  and honours `http.cainfo`/`CARGO_HTTP_CAINFO`/`SSL_CERT_FILE`, `http.proxy`,
  `http.timeout`, `http.user-agent` and `net.retry`; requests are retried on
  connection errors, timeouts, 429 and 5xx. Behind a TLS-intercepting proxy
  every index request used to fail and was reported as "crate not found".
- The merge of the Windows branch had left the workspace uncompilable (three
  stray braces); fixed.
- `rune-common` lacked the `libc` dependency its macOS memory probe needs — found
  by `rune unused`, invisible to Linux builds.

**Commands.**
- `rune outdated` / `rune upgrade [--incompatible] [--pinned] [--dry-run]`:
  newer releases per declared dependency (honouring `rust-version`), and
  requirements raised in place with their style and comments kept, then only
  those crates re-resolved. Rune's 34 declared dependencies: 0.19 s.
- `rune unused [--fix]`: rustc's `unused_crate_dependencies` lint over every
  target of the workspace's packages; a dependency is unused when every
  compilation given it reports it; one used only by tests is suggested as a
  dev-dependency; a name still appearing in the sources is reported, never
  removed. It found 23 in Rune's own crates, each confirmed by hand.
- `rune sbom` (CycloneDX 1.5 / SPDX 2.3), `rune tree --exec-surface` (32 of 187
  packages in Rune's graph run code at build time), `resolve.min-release-age`
  (from the index's `pubtime`; locked versions stay).
- `rune test --retries N` (flaky tests are warned about, not failed),
  `--junit PATH`, `--message-format json` results per binary, the three slowest
  binaries after a run of 5 s or more.
- `rune cas export/import`: the store entries a build needs as one (gzip) tar,
  imported as untrusted (paths, hashes, whole trees checked). Rune's workspace:
  219 entries, 739 MiB (231 MiB gzipped), export 0.9 s, import 1.4 s, then a
  clean checkout builds with 0 compiled. Build-script results travel too.
- `rune affected --partition N/M` for sharding CI.
- `rune tree` shows what a build on this machine uses, like `cargo tree`: the
  build's feature resolver runs for the host (`--target`, `--target all`), so
  other platforms' dependencies and optional dependencies nothing enables are
  gone and features are the ones built with; `[build-dependencies]` /
  `[dev-dependencies]` headings; `-e/--edges` including `-e features` (with
  `-i`: which package or feature turned each feature on — P10's question),
  `--prefix`, `name@version` specs; `-d` is `--duplicates` as in Cargo. On the
  469-package corpus below it matches `cargo tree` line for line except where
  Cargo draws a crate used at build time and at run time as two nodes.
- `rune add` lists the added release's features with the enabled ones marked,
  as `cargo add` does (a crate with hundreds, like `windows-sys`, gets the
  enabled ones and a count), plus its `rust-version` and publication date.
- `rune graph`: the package graph (as locked; build/dev edges apart) or, with
  `--units`, the units a build would run coloured by cache state, as DOT,
  Mermaid or JSON — the JSON unit graph is a machine-readable build plan.
- Build scripts' compiler output is captured (a failure shows it; warnings of a
  workspace package's script are shown and replayed), and `cargo::warning=` of
  workspace packages is shown.

**Ecosystem check (M2, started).** A project depending on 80 popular crates
(469 packages: tokio, axum, reqwest, tonic, clap, serde, regex, image, rusqlite
with bundled SQLite, openssl, git2/libgit2, aws-lc-sys, ring, zstd, bzip2, xz,
bindgen, ratatui, …):
- `rune generate-lockfile` is byte-identical to `cargo generate-lockfile`.
- It builds, and from an empty store **faster than Cargo: 112.5 s vs 122.8 s**
  (4 cores, same CPU time — the build-script steps overlap the heavy C builds).
- `rune tree` matches `cargo tree` line for line but for host/target node
  splitting (above).
- 42 popular crates' own test suites (unit, integration, doc tests; 7,800+
  tests) run under both tools. Three real incompatibilities turned up and are
  fixed: `rune test` did not build examples or run `test = true` ones
  (`once_cell`); `panic = "abort"` in `[profile.dev]` reached test harnesses
  (`winnow` — Cargo ignores `panic` for tests, proc-macros and build scripts);
  `#[cfg(test)]` was an unexpected cfg under `deny(warnings)` because Rune did
  not declare Cargo's `cfg(docsrs,test)` (`http`, `httparse`). Now every crate
  ends as under Cargo, test for test: 36 pass identically; `unicode-ident`,
  `anyhow` and `glob` fail the same test under both, `mime`, `nom` fail to build under both; and
  `same-file` passes only under Rune (Cargo cannot read the v1 lockfile it ships).
- A second batch of 30 crates heavy on build scripts, proc-macros and C
  (`ring`, `blake3`, `zstd`, `rusqlite`, `libgit2-sys`, `tokio`, `rayon`,
  `regex`, `proc-macro2`, `serde_derive`, …) turned up five more, all fixed:
  `[profile.test] lto` never reached test binaries (`ahash`'s `nopanic` test);
  a `links` package conflicted with its own integration tests (`ring`); a
  workspace crate stood in for the published release its dev-dependencies use
  (`proc-macro2` + `quote` — the lockfile is now byte-identical to Cargo's);
  doc tests were skipped for a library outside `src/` or documented through
  `cfg_attr(…, doc = …)` (`fnv`); proc-macro crates' doc tests never ran
  (`serde_derive`). Now 16 pass identically, 9 fail identically (test data not
  shipped in the package, lints the crate denies, `tokio`'s tests needing
  `--features full`), and the 5 end as under Cargo. Doc tests are named
  `src/lib.rs - Item (line N)` as under Cargo. Remaining difference: packages
  are keyed by name and version, so a published release with *exactly* the
  local version still resolves to the local package (Cargo would lock both).
- New lockfiles use format 3 when a member's `rust-version` predates 1.83,
  as Cargo does.
- Found on the way: a registry crate's `cargo:warning` lines leaked into every
  build as log lines (Cargo hides dependencies' script warnings).
- rust-analyzer: `--message-format json` now includes Cargo's
  `build-script-executed` messages (each script's `OUT_DIR`, cfgs and
  environment, also for results from the store), so generated code and
  script cfgs resolve in the editor.

**Disk.** The store is cleaned automatically: entries no build has used for
`cas.max-age` (default 30 days) are removed by a `rune cas gc` a build starts in
the background at most once a day. Before, it only shrank when `cas.max-size`
was set.

**Measured end to end (4 cores, Rune's own workspace, 183 units):**

| | Rune | Cargo |
| --- | --- | --- |
| cold build, empty store | 55.3–55.7 s | 54.4–55.0 s |
| no-op build | 53 ms | 231 ms |
| after deleting the target directory | 76 ms (0 compiled) | a cold build |
| `check` after `build` | 58 ms | compiles the workspace again |
| edit a crate with a build script, rebuild | 0.19 s | 0.24 s |
| `rune test` of the workspace | 429 passed, 52 s | |

On a 10-core M5 a cold build is limited by the chain of crates rather than by
the cores (the one-line hint and `--timings` show which); everything a store
hit or a smaller rebuild saves above holds there as well.

**Tests:** 444 (91 new), passing under `cargo test` and under `rune test` on
Rune's own workspace; clippy clean.

**Not done:** Windows; the sandbox (G1) and with it default-on cached tests;
a PubGrub resolver; scheduling single tests across binaries; `rune features`
(P10); zstd and pinning in the store (P6); measurements on the M5 itself.
