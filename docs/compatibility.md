# Cargo compatibility

Rune reads your existing `Cargo.toml` (or a [`Rune.toml`](manifests.md) with
the same contents, including TOML 1.1: multi-line inline tables, trailing
commas), builds with `rustc` directly, writes a Cargo-format `Cargo.lock` and
understands `.cargo/config.toml`. A project keeps working with plain `cargo`.

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
- **Git dependencies need `git` on `PATH`** (Rune uses your own git, so your
  credential helpers and `insteadOf` rules just work).
- **Structured diagnostics.** Warnings are replayed as rendered text.
- Linux is the platform this was developed on; the test suite also passes on
  macOS (Apple Silicon). Windows has not been exercised.
- `deny` is not built in (`rune audit` is); installed as the usual
  `cargo-deny`, it works as `rune deny`, like any `cargo-<name>` plugin
  (external subcommands get `CARGO` pointing at the real `cargo`, since plugins
  are written against it; `RUNE_PLUGIN_CARGO=rune` points them at Rune).

See [ROADMAP.md](../ROADMAP.md) for the full status against the compatibility
specification and what comes next.
