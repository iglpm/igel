# Cargo Compatibility & Ecosystem Support

Igel is designed to be 100% compatible with the Rust and Cargo ecosystem. It reads your existing `Cargo.toml`, follows your locked dependencies in `Cargo.lock`, executes compiler passes with `rustc`, and respects hierarchical configurations in `.cargo/config.toml`. 

You can introduce Igel to your team without changing a single line in your repository, and team members can freely use `cargo` and `igl` side by side.

## Supported Cargo Features

### Manifests and Workspaces

Igel supports the full specification of Cargo manifests:
- Standard `[package]` metadata and edition settings (supports TOML 1.1 features including multi-line inline tables and trailing commas).
- Multi-member `[workspace]` structures, including `members`, `exclude`, `default-members`, and resolver versions `1`, `2`, and `3`.
- Workspace inheritance (`workspace = true`) for package fields, dependencies, compiler lints, and metadata — including inheritance from foreign workspaces in Git repositories.
- All crate target types: `[lib]` crate-types (`rlib`, `dylib`, `cdylib`, `staticlib`, `proc-macro`, and multi-target combinations), plus `[[bin]]`, `[[example]]`, `[[test]]`, and `[[bench]]`.
- Target auto-discovery (`autolib`, `autobins`, `autoexamples`, `autotests`, `autobenches`).

### Dependency Management

Igel handles all standard dependency sources and specifications:
- **Registry Sources**: crates.io and alternative sparse registries with token authentication.
- **Git Dependencies**: Pinned commits, tags, branches, and nested submodules.
- **Path Dependencies**: Local paths and sibling workspace references.
- **Renamed Dependencies**: `package = "..."` syntax with aliased imports.
- **Platform-Gated Dependencies**: Target-specific `target.'cfg(...)'.dependencies` evaluated against the compiler's actual target configuration.
- **Override Mechanisms**: Full support for `[patch]` and `[replace]` in both manifests and `.cargo/config.toml`.
- **Source Replacement**: `replace-with` configuration pointing to mirror servers, vendor directories, or local package stores.

### Feature Resolution Model

Igel faithfully implements Cargo's complete feature model:
- `default`, optional dependencies, explicit `dep:`, weak feature activation (`dep?/feat`), and namespaced `foo/bar` features.
- Isolated resolution per selection and role: building a specific package (`-p client`) only enables features requested by that target, preventing accidental feature bloat.
- Under resolver 2 and 3, host tools (build scripts and procedural macros) receive their own dedicated feature sets independent of target builds.

### Host and Target Roles

Igel strictly separates execution environments:
- Build scripts, procedural macros, and their dependencies compile for the host machine.
- Project libraries, binaries, examples, and tests compile for the designated `--target` triple.
- Platform configurations (`CARGO_CFG_*`) match `rustc --print=cfg` output exactly.

### Build Scripts (`build.rs`)

Igel provides complete runtime support for the `cargo::` build script specification:
- Output directives: `rustc-link-lib`, `rustc-link-search`, `rustc-link-arg`, `rustc-cdylib-link-arg`, `rustc-flags`, `rustc-cfg`, `rustc-check-cfg`, and `rustc-env`.
- Standard environment variables: `TARGET`, `HOST`, `OUT_DIR`, `CARGO_PKG_*`, `CARGO_FEATURE_*`, and `CARGO_ENCODED_RUSTFLAGS`.
- Shared package execution: a single build script run serves all targets (lib, bins, tests) within the package.
- Conflict detection: validation of `links` attributes across the workspace graph.

### Native C, C++, and System Bindings

Igel supports the native binding ecosystem out of the box, verified against production workloads:
- Build tools: `cc` (C and C++ compilation), `bindgen`, and `cxx`/`cxx-build`.
- Bundled C libraries (e.g. `rusqlite` with bundled SQLite, `libz-sys`, `flate2`).
- Apple frameworks (`CoreFoundation`, `Security`).
- Python extensions via `pyo3` (both Python extension modules and embedded Python runtimes).
- C ABI export headers via `cbindgen`, and dynamic plugin loading via `libloading`.
- Dynamic library search paths (`LD_LIBRARY_PATH`, `DYLD_FALLBACK_LIBRARY_PATH`, and `PATH`) are automatically set when running binaries or tests.

### Profiles and Optimization

Igel honors built-in profiles (`dev`, `release`, `test`, `bench`) and custom user profiles:
- Profile inheritance (`inherits = "release"`).
- Granular profile overrides: `[profile.*.package.<name>]` and `[profile.*.build-override]`.
- Overrides via `.cargo/config.toml` and environment variables (`CARGO_PROFILE_<NAME>_<KEY>`).
- On Apple targets, `split-debuginfo` defaults to `unpacked` for faster debug builds.

### Dependency Resolution & Lockfiles

Igel's resolver guarantees deterministic dependency graphs:
- Reads and updates standard `Cargo.lock` files without syntax changes.
- MSRV-aware resolution with resolver 3.
- Parallel registry index fetching: resolves sparse index entries concurrently for superior resolution speed.
- Generates byte-identical lockfiles compared to Cargo's native resolver.

## Known Boundaries

Igel strives for 100% compatibility while prioritizing modern workflows. Take note of these current design boundaries:

- **Sparse Registries Only**: Igel exclusively supports modern HTTP sparse registry indexes (`sparse+https://`). Legacy Git-based registry indexes are not supported.
- **Credential Providers**: Supported authentication methods include static API tokens and command-based token providers (`cargo:token-from-stdout`). Platform OS keychains are not currently supported.
- **External Git Tool**: Git dependencies utilize the system `git` executable on your `PATH`, ensuring that your SSH keys, credentials helpers, and `insteadOf` configurations apply automatically.
- **Identical Name & Version Isolation**: In rare cases where a workspace package consumes an external published version of itself sharing the exact same package name and version number, Igel unifies the local workspace version.
- **Cargo Deny Integration**: To use `cargo-deny`, install it normally and execute it as `igl deny`.
