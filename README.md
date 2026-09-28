# Rune

**A fast, Cargo-compatible build system and package manager for Rust.**

Rune reads your existing `Cargo.toml` — or a **`Rune.toml`** with the same
contents, see *Manifests* below — (including TOML 1.1: multi-line inline tables,
trailing commas), builds with `rustc` directly, and reuses compiled artifacts
through a content-addressable store (CAS). A project with a `Cargo.toml` keeps
working with plain `cargo`: Rune writes a Cargo-format `Cargo.lock` and
understands `.cargo/config.toml`.

- **A key is an identity, not a hash of the source** — compiler, wrappers,
  target, flags, profile, features, dependencies' keys, build-script results and
  the outside world a unit read all go into what makes an artifact reusable
- **Content-hash fingerprinting** — rebuild when contents change, not mtimes
- **rmeta pipelining** — dependents start as soon as a dependency's metadata exists
- **Content-addressable store** — immutable, verified artifacts shared across
  projects, profiles and targets
- **Critical-path scheduling** — weighted by measured compile times from earlier builds
- **Build scripts as their own step** — a script runs while its package's other
  dependencies still compile, and reruns only when a file it watches changes
- **Uses the whole machine** — about 1.3 compilers per core sharing one GNU
  jobserver (each `rustc` borrows idle slots for its codegen threads), no new
  compilers while memory is short, and a build that left cores idle tells you
  which chain of crates held it back
- **Pipelined test runner** — test binaries run the moment they link; retries
  with flaky-test detection, JUnit and JSON reports

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
rune add serde -F derive       # lists the release's features, + for those turned on
rune tree -i serde             # who depends on serde? (what a build on this machine uses)
rune tree -e features -i serde # which package turned on each of serde's features
rune why serde
rune update                    # only re-downloads changed index entries
rune new my-app                # or: rune init  (add --manifest rune for a Rune.toml project)
rune build --deps-only         # just the dependencies: a Docker layer / CI cache step
rune check --message-format json   # Cargo-format JSON on stdout (rust-analyzer, CI annotations)
rune test --cached             # skip test binaries that passed with identical inputs
rune watch -x check -x "test --lib"   # re-run on every change; going back to old code is instant
rune affected --since origin/main     # packages a change can influence, for selective CI
rune script hello.rs           # single-file scripts with `---cargo` dependencies (or a shebang)
rune doctor                    # what slows your builds down, with the fix
rune build --timings           # HTML timeline with the chain of crates that set the build's length
rune explain                   # which units the next build would compile, and what changed in each
rune cas du                    # what takes the space in the store, and how much is idle
rune graph --format mermaid    # the package graph (DOT, Mermaid or JSON); --units for the build plan

rune outdated                  # newer releases than the lockfile or the manifests allow
rune upgrade [--incompatible]  # raise requirements in the manifests, keeping their style
rune unused [--fix]            # dependencies the compiler says a package never uses
rune sbom -o bom.json          # CycloneDX 1.5 (or --format spdx) of what the workspace ships
rune tree --exec-surface       # every crate that runs code at build time, and who brings it in

rune test --retries 2 --junit report.xml   # flaky tests reported, not failed; CI test report
rune cas export -o ci.tar.gz --for build --for test   # the store entries a CI run needs, as one file
rune cas import ci.tar.gz      # (verified: nothing unchecked enters the store)
rune affected --since origin/main --partition 2/4    # this machine's share of the affected packages

rune install ripgrep           # from crates.io (also --git URL, --path DIR); rune install --list
rune uninstall ripgrep
rune clippy -- -D warnings     # lint flags for the workspace's own crates
rune fmt --all -- --check      # the usual CI formatting check
rune fix                       # apply the compiler's machine-applicable suggestions (clippy --fix too)
rune rustc --lib -- --emit=asm # extra rustc arguments for one target (rune rustdoc -- … likewise)
rune update -p serde --precise 1.0.200   # pin one dependency (--dry-run to preview)
rune package / rune publish    # build and upload a .crate (login, logout, yank, owner too)
rune search http client        # crates.io search; rune info tokio for one crate
rune vendor                    # every dependency into vendor/, with the config to use it
rune pkgid serde / rune verify-project
rune nextest run               # any `cargo-<name>` / `rune-<name>` on PATH is a subcommand
```

Cargo's selection flags work as expected: `-p/--package`, `--workspace`,
`--exclude`, `--bin`, `--lib`, `--example`, `--test`, `--bench`, `--all-targets`,
`-F/--features`, `--all-features`, `--no-default-features`, `--profile`,
`--target`, `--manifest-path`, `--locked`, `--offline`, `--frozen`,
`--ignore-rust-version`. `[alias]` entries from `.cargo/config.toml`
(`xtask = "run --package xtask --"`) work as commands, and `[profile.*]` in
configuration and `CARGO_PROFILE_<NAME>_<KEY>` variables override the
manifest's profiles, as with Cargo. Status lines go to stderr; stdout carries only
what a command produces (`metadata`, `vendor`'s configuration, your program's
output), so it can be piped.

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

**C, C++ and other bindings.** Everything the binding ecosystem relies on
works as with Cargo, verified with real crates on macOS: `cc` (C and C++),
`bindgen`, `cxx`/`cxx-build`, bundled C libraries (`rusqlite` with `bundled`),
`libz-sys`/`flate2`, frameworks (`core-foundation`), `pyo3` both as a Python
extension module and embedding Python, `cbindgen` headers, and `cdylib` plugins
loaded with `libloading`. In particular:
- native library directories a `-sys` crate's build script reports reach every
  crate that links it, however far up (a dynamic `-lpython3.9`, OpenSSL from
  Homebrew), and doc tests;
- `rune run`/`rune test` find dynamic libraries built by build scripts and by
  the workspace (`DYLD_FALLBACK_LIBRARY_PATH`, `LD_LIBRARY_PATH` or `PATH`);
- a workspace library's `staticlib`/`cdylib`/`dylib`/`rlib` also appears under
  Cargo's names in `target/debug/` (`libfoo.a`, `libfoo.so`/`.dylib`,
  `foo.dll`), where C build systems, Python packaging and plugin loaders look;
- doc tests and `rune doc` see the build script's `OUT_DIR` (generated
  bindings), `cfg`s and environment;
- warnings from dependencies' build scripts are silenced, as Cargo does.

**Toolchain and configuration.** `RUSTC`, `RUSTC_WRAPPER`,
`RUSTC_WORKSPACE_WRAPPER`, `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`,
`rust-toolchain(.toml)` (resolved through `rustup`), and the hierarchical
`.cargo/config.toml`: `[build]` (`rustc`, wrappers, `rustflags`, `target`,
`rustdocflags`), `[target.<triple|cfg>]` (`linker`, `runner`, `rustflags`),
`[env]`, `[registries]`, `[source]`, `[patch]`, tokens and
`credential-provider = "cargo:token-from-stdout …"`.

**Profiles.** `dev`/`release`/`test`/`bench`, custom profiles with `inherits`,
`[profile.*.package.<name|*>]` and `[profile.*.build-override]` (by default
build scripts and proc-macros compile unoptimized, as in Cargo), also from
`.cargo/config.toml` and `CARGO_PROFILE_<NAME>_<KEY>`. The key uses the
*effective* profile of each unit. On Apple targets, `split-debuginfo` defaults
to `unpacked` like Cargo's (no `dsymutil` run per binary).

**Resolution.** Newest compatible versions, seeded by `Cargo.lock` (which stays
the source of truth: a lockfile changed by `git pull` or `cargo update` is
respected), MSRV-aware with resolver 3, and one version per semver-compatible
range as Cargo requires: when a stricter requirement turns up late, resolution
settles on the version every requirement accepts, or backtracks to an older
release of the crate that brought the conflict in. A fresh lockfile for Rune's
own 229-package workspace is byte-identical to `cargo generate-lockfile`'s.
Index entries a lockfile names are fetched in one concurrent wave rather than
level by level (`rune update`: 3.2 s → 0.7–1.2 s, faster than Cargo's 1.7 s).

**Registry workflow.** `rune install` (crates.io, `--git`, `--path`; tracked in
Cargo's own `.crates.toml`, so `cargo install --list`/`uninstall` agree),
`package` (normalized manifest, workspace inheritance resolved, path/git
dependencies turned into registry ones, `.cargo_vcs_info.json`, verification
build), `publish` (then waits until the index has it), `login`/`logout` (in
Cargo's `credentials.toml`), `yank`, `owner`, `search`, `info`, `vendor` (with
`.cargo-checksum.json`, so Cargo accepts the directory too; vendored git
dependencies included).

## Manifests: `Cargo.toml` or `Rune.toml`

Rune reads Cargo's manifest format from either file, so a project can be plain
Cargo, **Rune-only**, or both:

| The directory has | The manifest is |
| --- | --- |
| `Cargo.toml` | `Cargo.toml` (as always) |
| `Rune.toml` with a `[package]` or `[workspace]` table (and no `Cargo.toml`) | `Rune.toml` — a Rune-only project |
| `Rune.toml` with only settings (`[build]`, `[cas]`, …) beside a `Cargo.toml` | `Cargo.toml`; the `Rune.toml` is just settings, exactly as before |
| both, and both have `[package]`/`[workspace]` | `Rune.toml` (`rune doctor` warns, because plain `cargo` would read the other) |

```bash
rune new my-app --manifest rune       # creates Rune.toml, no Cargo.toml
rune init --manifest rune             # in an existing directory; keeps a settings-only Rune.toml's settings
rune run / build / test / add / remove / metadata / locate-project …   # all work on either
```

The contents are the same `[package]`, `[dependencies]`, `[workspace]`, `[features]`,
`[profile.*]`, `[[bin]]`, … as in Cargo. Everything that finds a manifest —
workspace roots and members, `--manifest-path` (a file, or a directory), path
dependencies, git dependencies, `[patch]`, `rune add`/`remove` (which edit the file
they found), `rune metadata` / `locate-project` / `--message-format json` — asks
one place, so they cannot disagree. A workspace may mix the two, and a path or git
dependency may be `Rune.toml`-only. Registry crates are always `Cargo.toml` (that is
what `cargo publish` produces).

A `Rune.toml` manifest can also hold Rune's own settings tables next to the
manifest tables; `rune config set build.jobs 4` writes into the same file and keeps
the rest. One name collides: `[test]` (settings) and `[[test]]` (test targets). A
`[test]` *table* is read as settings; if the file has `[[test]]` targets,
`rune config set test.…` refuses and points at `--global` or the
`RUNE_TEST_…` environment variable.

What you give up without a `Cargo.toml`: plain `cargo`, `rust-analyzer` setups and
other tools that look for one only understand it. The lockfile stays `Cargo.lock`
(Cargo's format) either way. The file name is not part of any cache key, so the
`Cargo.toml` and `Rune.toml` versions of one project share compiled artifacts.
Verified on Rune's own workspace (11 manifests) with every `Cargo.toml` renamed: 182 units
build, `0 compiled` after wiping `target`, all tests pass, and `Cargo.lock` is
identical.

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
| `build.jobs` | CPUs + ⅓ | Parallel compiler jobs (13 on a 10-core machine). Each `rustc` may borrow slots other jobs leave idle for its own codegen threads. |
| `build.frontend-threads` | off | `-Z threads` for nightly compilers' parallel front end: `auto` gives a crate that starts while cores are idle (typically the last, largest ones) as many threads as there are idle cores; a number applies to every crate. Ignored on stable. |
| `build.dependency-debuginfo` | `false` | **Off:** registry and git dependencies compile with `debug = "line-tables-only"` in the dev profile — backtraces and breakpoints work, local variables *inside dependency code* are not inspectable — which made a cold build of Rune itself about 10 % faster. **On:** Cargo's full debuginfo. A `debug` key in any profile (`[profile.dev]`, `[profile.dev.package."*"]`, …) always wins, and your own crates are never affected. |
| `build.target-dir` | `.rune/target` | Build output directory. |
| `cas.enabled` | `true` | Use the CAS at all. |
| `cas.dir` | `~/.rune/cas` | CAS location. |
| `cas.remote` | none | A store shared with other machines, checked before compiling and published to after: a directory, or one or more [Cairn](../cairn) URLs separated by commas (entries are sharded across them) — see *Sharing the store*. |
| `cas.remote-token` | none | Bearer token for a Cairn server. Prefer `RUNE_CAS_REMOTE_TOKEN`: `Rune.toml` is readable by everyone who can read the project. |
| `cas.remote-replicas` | `1` | With several Cairn URLs: how many servers hold each entry (`2` survives losing one). |
| `cas.remote-write` | `true` | Publish to `cas.remote`. Set `false` where only CI should write (e.g. developers). |
| `cas.max-age` | `30d` | Entries no build has used for this long are removed by a background `rune cas gc` that a build starts at most once a day (like Cargo's automatic cache cleanup). `0` keeps everything. |
| `cas.max-size` | unlimited | Evict least-recently-used entries after a build when the CAS grows past this (`20GB`, `512MiB`). |
| `cas.verify` | `false` | Re-hash every artifact restored from the CAS (entries are always checked against their key and recorded sizes). |
| `resolve.min-release-age` | `0` | Only choose releases published at least this long ago (`14d`, `72h`) when resolving new versions; locked versions stay. Malicious releases are usually yanked within days. |
| `resolve.write-cargo-lock` | `true` | Create/update `Cargo.lock` when the set of locked packages changes. An up-to-date lockfile is never touched. |
| `net.offline` | `false` | Never touch the network. |
| `test.doctests` | `true` | Run documentation tests in `rune test`. |
| `test.cache` | `false` | Same as `rune test --cached`: remember passing test binaries (see below). |

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
  script — the identity of the C toolchain (on macOS including the active
  Xcode/Command Line Tools and SDK). The key then follows the *current values*
  of those, so an artifact is only found again when everything it read is
  unchanged. For Apple targets, `MACOSX_DEPLOYMENT_TARGET` (and its iOS/tvOS/…
  siblings) and `SDKROOT` are part of the key when set: rustc writes them into
  the objects without listing them in its dep-info.
* **Build scripts have their own key**: the script's configuration, its
  build-dependencies and `DEP_*` values, plus — if it declared
  `rerun-if-changed` — exactly the files it watches and its own sources. Editing
  the rest of the package does not run it again (as in Cargo), and a script
  that must run again because a watched file changed is not recompiled.
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
- **Packages are identified by name and version.** A workspace crate whose
  dev-dependencies use its *published release of exactly the same version*
  gets the local crate there too; Cargo keeps the two apart. (With any other
  version they are kept apart, as in Cargo.)
- **The resolver is not a full SAT/PubGrub solver.** It picks the newest
  version that fits, then settles each semver-compatible range on one version,
  backtracking one requirer at a time. A conflict that needs several crates
  moved at once is reported as a warning and left with two versions side by
  side, where Cargo would find a solution or refuse.
- **Git dependencies need `git` on `PATH`** (Rune uses your own git, so your
  credential helpers and `insteadOf` rules just work).
- **Structured diagnostics.** Warnings are replayed as rendered text.
- Linux is the platform this was developed on; the test suite also passes on
  macOS (Apple Silicon). Windows has not been exercised.
- `audit` and `deny` are not built in; installed as the usual
  `cargo-audit`/`cargo-deny`, they work as `rune audit` etc.
  (external subcommands get `CARGO` pointing at the real `cargo`, since plugins
  are written against it; `RUNE_PLUGIN_CARGO=rune` points them at Rune).

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
├── rune-scheduler    (parallel executor, jobserver, pipelining, build-script steps, declared inputs, schedule analysis)
├── rune-compiler     (direct rustc invocation, build scripts)
├── rune-cas          (immutable content-addressable store, atomic writes, LRU GC)
└── rune-common       (shared types, settings, toolchain, .cargo/config.toml, cfg evaluation)
```

## Using the whole machine

Rune runs about 1.3 compiler processes per core (`-j`, `build.jobs`) and hands
every `rustc` and build script the same GNU jobserver, so a compiler that is
alone at the end of a build can take the idle slots for its codegen threads
while the total never exceeds the budget. A crate starts the moment the
`.rmeta` of each dependency exists; a build script is its own step that starts
once its build-dependencies are built, overlapping the rest of its package's
dependencies. Before starting another compiler Rune checks available memory
(Linux `MemAvailable`, macOS memory pressure) and, while it is short, lets
running units finish first — a machine with many cores and little memory
neither swaps nor loses a `rustc` to the OOM killer.

What stable Rust cannot parallelize is a chain of crates that must be compiled
one after another, and a single crate's front end. A build longer than 10 s that
kept fewer than 60 % of the cores busy ends with one line naming that chain;
`rune build --timings` (or `RUNE_TIMINGS=1`) prints it in full, with how long
each phase of the command took, and writes
`.rune/timings/rune-timing.html` — a timeline of every step with the chain
highlighted and how many ran at once — plus a Chrome trace in
`.rune/fingerprints/trace.json` (<https://ui.perfetto.dev>). Splitting the
largest crate of that chain is usually what helps. On a nightly compiler,
`build.frontend-threads = "auto"` gives crates that start while cores are idle
a parallel front end.

## CI

```bash
rune cas import ci.tar.gz || true                     # restore (a missing file is fine)
rune build && rune test --retries 2 --junit report.xml
rune cas export -o ci.tar.gz --for build --for test   # save exactly what this run needs
```

A bundle holds the store entries a build of the workspace needs — artifacts,
build-script results, replayed warnings, recorded outside inputs — and nothing
else, so it does not grow with every run the way a cached `target/` does
(Rune's own: 219 entries, 231 MiB gzipped; a clean checkout with it builds with
`0 compiled`). Import treats the file as untrusted: every artifact is checked
against its recorded hash and an entry the store already has is kept.
`rune affected --since origin/main --partition N/M` splits the packages a change
can influence between M machines, the same way on each.

## Sharing the store

`cas.remote` points at something several machines can reach; a build checks it
before compiling and publishes to it after. It is either a **directory** (a network
share, a CI cache volume) or **Cairn** servers — the companion project in
[`../cairn`](../cairn), a small server with tokens, integrity checks, eviction,
metrics and multi-server topologies.

```bash
export RUNE_CAS_REMOTE=http://cairn.example:7070      # or a directory: /mnt/team-cache/rune
export RUNE_CAS_REMOTE_TOKEN=<token>                  # prefer the environment to Rune.toml
rune build                                            # CI: compiles, publishes in the background
RUNE_CAS_REMOTE_WRITE=false rune build                # a developer: read-only

# several servers: entries are sharded by rendezvous hashing; keep 2 copies of each
export RUNE_CAS_REMOTE=http://a:7070,http://b:7070
export RUNE_CAS_REMOTE_REPLICAS=2
```

Whatever another machine has built is copied into your local store on a miss and
used like a local entry — artifacts, build-script output and generated files alike,
with binaries still executable. Keys do not contain the checkout path, so different
paths, users and `RUNE_HOME`s share. Measured on Rune's own 182 units (811 MB): a
developer with an empty local store restores everything from a Cairn server in
**0.65 s** (about 30 s to build), from a plain directory in 0.7 s, and — with two
replicated servers, one of them killed — still in 0.65 s. Publishing costs a CI build
about 1 %: it happens in the background.

Entries are immutable and written slot by slot with the marker file last (a
half-copied entry is invisible), checked against their manifest when pulled, and the
first publisher wins, so any number of machines can write at once. A missing or
unreachable store is just a miss; a server that cannot be connected to is skipped for
30 s (one attempt, not one per unit); a refused token is reported once.

**Trust:** a key identifies what went *into* an artifact, not what came out, so only
share a store between machines you would run each other's binaries on (give laptops
read-only access). Artifacts built elsewhere carry that machine's absolute paths in
debug info and panic messages (harmless, but they differ byte for byte from a local
build). There is no eviction on a shared *directory*; Cairn evicts by last use.

## Cached test results

`rune test --cached` (or `test.cache = true`) remembers a passing test binary under
a key made of the binary's bytes (which cover code, dependencies and compiler),
every file in the package directory (fixtures), the arguments, and the
`RUST_*`/`CARGO_*`/`RUNE_*` environment. Change any of them and the binary runs
again; a failing run is never remembered; `--cached=false` forces a run; doc-tests
always run. On Rune's own 285 tests: about 8 s → 0.26 s when nothing changed.

It is opt-in because Rune can only key what it can see. **Do not enable it** for
tests that depend on the network, the clock, randomness, other environment
variables or files outside the package — they would keep "passing" from the cache
while the world they test has changed.

## Scripts

A single `.rs` file can carry its own dependencies:

```rust
#!/usr/bin/env -S rune script
---cargo
[dependencies]
serde_json = "1"
---
fn main() { println!("{}", serde_json::json!({ "hello": "world" })); }
```

`rune script hello.rs [args]` (or `./hello.rs` with the shebang) builds it as a
throwaway package under `$RUNE_HOME/scripts/` and runs it; stdout is the script's
output and nothing else, and its exit code is Rune's. Because dependencies come
from the store, a script whose dependencies any project has built before starts in
about 0.15 s. Line numbers in errors match the file.

## Dependencies and supply chain

`rune outdated` lists the declared dependencies with newer releases (compatible,
and newest overall, honouring each package's `rust-version`); `rune upgrade`
raises the requirements in place, keeping comments and how each requirement is
written (`1` stays `1`, `0.7` becomes `0.9`). `rune unused` runs `rustc`'s own
`unused_crate_dependencies` lint over every target of the workspace's packages —
no text search, so proc-macros, renamed crates and build-dependencies are
judged correctly — and suggests moving a dependency only tests use to
`[dev-dependencies]`; `--fix` removes the unused ones.

`rune sbom` writes a CycloneDX 1.5 or SPDX 2.3 bill of materials with package
URLs, checksums, licences and the locked dependency edges (`SOURCE_DATE_EPOCH`
makes it reproducible). `rune tree --exec-surface` lists every crate that runs
code on your machine while building — build scripts, proc-macros, native
`links` crates — with the chain that brings each in. `resolve.min-release-age =
"14d"` keeps resolution away from releases younger than that.

## Watching and selective CI

`rune watch` re-runs commands (`-x check`, by default) when files change, killing
and restarting one that is still running. `rune affected --since <rev>` prints the
workspace packages a diff can influence — the packages owning changed files and
everything that depends on them; `--format args` gives `-p a -p b` for
`rune test $(rune affected --since origin/main --format args)`. A change to
`Cargo.lock`, the root `Cargo.toml`, `rust-toolchain` or `.cargo/config.toml`
affects everything.

## `check` after `build`

`rune check` reuses what `rune build` already compiled: a checked crate takes
the stored result of the same crate fully built, whose `.rmeta` is what its
dependents type-check against, and its warnings are replayed. On Rune's own
workspace (181 units), `check` right after `build` compiles nothing (0.3 s;
before, it recompiled 148 units in 6 s). After an edit, only the edited crate
and what depends on it are checked for real.

rustc gives a checked and a built `.rmeta` of the same crate different crate
hashes, so the two must never meet in one dependency chain. A crate is reused
this way only if everything below it is too, and it then carries the built
crate's key, so each dependent's key records which kind it was checked against.
`build` after `check` still compiles: a check produces no code.

## Several `rune` processes at once

An editor's background check next to a terminal build, CI shards on one
checkout, two agents in one working tree: none of them blocks the others, and
none breaks the others. Rune takes no lock on the target directory; every file is
written and linked into place atomically, and a process only waits when another
one is building *the same unit* — after which it takes that result from the
store instead of compiling it again (four concurrent builds of a project with
registry dependencies compiled 10 units in total instead of 36).

## Editor and tool integration

`--message-format json` prints Cargo's JSON messages (`compiler-message`,
`compiler-artifact`, `build-finished`) on stdout, one object per line. Warnings
of unchanged crates are replayed from the store, so a tool never loses a
diagnostic because a crate was a cache hit. The output is tested against Cargo's
message shape, not yet against a live rust-analyzer. To try it there:

```json
{
  "rust-analyzer.check.overrideCommand": ["rune", "check", "--message-format=json", "--workspace", "--all-targets"],
  "rust-analyzer.cargo.buildScripts.overrideCommand": ["rune", "check", "--message-format=json", "--workspace", "--all-targets"]
}
```

The second line has rust-analyzer take build scripts' results from Rune too:
the JSON includes Cargo's `build-script-executed` messages (each script's
`OUT_DIR`, cfgs and environment, also when the result came from the store), so
`include!(concat!(env!("OUT_DIR"), …))` and script-set cfgs resolve in the editor.

`rune test --message-format json` adds one `test-suite-finished` line per test
binary (outcome, counts, every test and its status). `rune graph --units
--format json` is the build plan: every unit with its key, whether the store
has it, its features and platform, and its dependencies.

## Building Rune

```bash
cargo build --release      # or: rune build --release
cargo test --workspace     # or: rune test
```

## License

MIT OR Apache-2.0
