# Supply Chain Security & Dependency Auditing

Software supply chain attacks targeting build scripts, procedural macros, and newly published malicious packages are an increasing threat in modern software development.

Rune builds security into your workflow by default, providing execution sandboxing, real-time vulnerability scanning, dependency hygiene tools, and Software Bill of Materials (SBOM) generation.

## Confining Build-Time Code Execution

Standard Cargo builds execute arbitrary code from dependencies via build scripts (`build.rs`) and procedural macros with full system user privileges. A malicious dependency can access personal files, read SSH keys, or make unauthorized network connections.

Rune provides process-level build sandboxing:

```bash
# Enable build sandboxing via CLI or Rune.toml
rune build --sandbox
```

Or configure permanently in `Rune.toml`:

```toml
[build]
sandbox = true
sandbox-network = "openssl-src, libz-sys" # Allowlist specific packages that need network
```

When sandboxing is active:
- **Filesystem Isolation**: Build scripts and procedural macros run with a read-only filesystem view, except for their designated `OUT_DIR` and a private temporary directory. Attempts to modify files outside `OUT_DIR` or access dotfiles like `~/.bashrc` fail immediately.
- **Network Isolation**: By default, sandboxed build scripts have zero network access.
- **Zero Overhead**: On Linux, sandboxing uses unprivileged user, mount, and network namespaces. On macOS, it leverages `sandbox-exec`. A full sandboxed build of Rune (including `ring` and `zstd-sys`) measures virtually identical wall-clock time to an unconfined build.

## Continuous Vulnerability Auditing: `rune audit`

Scan your locked dependencies against the official RustSec Advisory Database and OSV vulnerability feeds:

```bash
rune audit
```

```text
▸ checked 236 packages against 1,875 advisories, updated 2026-09-30
✗ smallvec 1.6.0 · RUSTSEC-2021-0003 · critical 9.8
  Buffer overflow in SmallVec::insert_many
  fix   >=1.6.1 — `rune update smallvec --precise 1.6.1`
  path  app 0.1.0 → middleware 1.0.0 → smallvec 1.6.0
  info  https://rustsec.org/advisories/RUSTSEC-2021-0003
! bincode 1.3.3 · RUSTSEC-2025-0141 · unmaintained
✗ 1 vulnerability found · 1 warning
```

`rune audit` provides actionable remediation:
- Identifies critical CVEs, unmaintained packages, unsound code, and yanked releases.
- Traces the exact dependency path from your workspace root to the vulnerable package.
- Recommends the exact `rune update ... --precise` command to upgrade to a compatible, patched release.
- Operates offline using a cached local database in `~/.rune/advisory-db`.

In CI, enforce zero-tolerance security policies:

```bash
rune audit --deny warnings --deny yanked
```

To accept an audited advisory after security review:

```bash
rune audit --ignore RUSTSEC-2021-0003
```

## Inspecting the Build-Time Attack Surface

Understand exactly which crates in your dependency graph run code at build time:

```bash
rune tree --exec-surface
```

This command outputs a filtered tree displaying every crate with a `build.rs` script, procedural macro, or native C/C++ `-sys` linkage, along with the dependency chain that introduced it.

## Quarantine Young Packages: `resolve.min-release-age`

Most malicious crate attacks are detected and yanked within 48 to 72 hours of publication. You can shield your project from zero-day registry compromises by enforcing a release age buffer:

```toml
[resolve]
min-release-age = "14d"
```

When resolving new dependencies or updates, Rune selects only releases that have been published for at least 14 days, ensuring the open-source community and security scanners have had time to review new versions.

## Dependency Maintenance Tools

### Upgrading Dependencies: `rune outdated` and `rune upgrade`

Keep dependencies modern without manual research:

```bash
# Check for available updates adhering to MSRV constraints
rune outdated

# Upgrade version requirements directly in your manifests
rune upgrade

# Upgrade across breaking semver boundaries
rune upgrade --incompatible
```

`rune upgrade` edits your `Cargo.toml` in-place while preserving custom comments, formatting style, and inline table layout.

### Eliminating Dead Code: `rune unused`

Identify dependencies that your codebase never actually references:

```bash
# Detect unused dependencies using compiler lints
rune unused

# Automatically remove unused dependencies from manifests
rune unused --fix
```

Unlike crude text-grep tools, `rune unused` uses `rustc`'s native `unused_crate_dependencies` analysis, accurately identifying procedural macros, build dependencies, and renamed crates.

## Software Bill of Materials (SBOM) Export

Generate standard machine-readable supply chain manifests for compliance and security auditing:

```bash
# Export CycloneDX 1.5 JSON
rune sbom -o bom.json

# Export SPDX 2.3 format
rune sbom --format spdx -o bom.spdx.json
```

The exported SBOM includes verified package URLs, SHA-256 checksums, declared licenses, and locked dependency relationships. When combined with `SOURCE_DATE_EPOCH`, generation is completely deterministic and reproducible.
