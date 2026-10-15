# Configuration Reference

Rune provides flexible configuration options that can be scoped to individual repositories or applied globally across your machine.

Project configuration lives in `Rune.toml` at the root of your workspace (Cargo ignores this file). Global settings reside in `~/.rune/config.toml`.

## Configuration Precedence

Settings are resolved in the following order of precedence (highest to lowest):

1. Command-line flags (e.g. `--release`, `--jobs 8`)
2. Environment variables (`RUNE_*`)
3. Project configuration (`./Rune.toml`)
4. Global user configuration (`~/.rune/config.toml`)
5. Built-in defaults

## Managing Settings with the CLI

Use `rune config` to view and modify settings without editing TOML files by hand:

```bash
# View active settings with their origin source
rune config list --show-origin

# Configure settings for the current project
rune config set build.release-rebuild-all true

# Configure settings globally across all projects
rune config set build.linker hyprlnk --global

# Inspect or remove a specific setting
rune config get build.compiler
rune config unset build.release-rebuild-all
```

## Available Configuration Keys

### Build Settings (`[build]`)

| Key | Default | Description |
| :--- | :--- | :--- |
| `build.compiler` | `auto` | Compiler selection mode. `auto` uses [rustc-runified](../rustc-runified/README.md) if installed for your toolchain to unlock pipelined binary linking and parallel frontends; otherwise falls back to `rustc`. Set to `rustc` to always use standard toolchain binaries, or `runified` to require the enhanced driver. |
| `build.linker` | `hyprlnk` | Linker selection. `hyprlnk` uses the built-in [hyprlnk](../hyprlnk/README.md) linker across Linux, Windows, and macOS (typically 2× faster than lld and Apple ld). Set to `default` to use the linker specified by `rustc` or `.cargo/config.toml`. |
| `build.jobs` | CPUs + ⅓ | Parallel compiler worker budget (e.g. 13 jobs on a 10-core machine). Compiler workers share an internal GNU jobserver, allowing idle slots to be borrowed dynamically by codegen threads. |
| `build.frontend-threads` | `off` | Parallel compiler frontend for nightly compilers and rustc-runified. `auto` dynamically assigns idle cores to compilation units (ideal for large end-of-chain crates). |
| `build.dependency-debuginfo`| `false` | When `false`, registry and Git dependencies compile with `debug = "line-tables-only"` in dev profiles, speeding up cold builds by ~10% while preserving backtraces and breakpoints. Set to `true` to match Cargo's full dependency debuginfo. |
| `build.feature-unification` | `selected` | Feature resolution behavior across workspace builds. Setting to `workspace` unifies feature requirements across all workspace members, maximizing shared artifact reuse without helper crates. |
| `build.sandbox` | `false` | Enforce execution sandboxing for build scripts and procedural macros. Disallows network access and makes the filesystem read-only outside of `OUT_DIR`. |
| `build.sandbox-network` | none | Comma-separated list of package names permitted to access the network during sandboxed builds. |
| `build.low-priority` | `false` | Runs compilation jobs at a lower process scheduling priority (`nice`), keeping your desktop responsive during heavy background builds. |
| `build.release-rebuild-all`| `false` | When `false`, release builds reuse existing CAS artifacts. When `true`, all crates are recompiled unconditionally for release profiles. |
| `build.target-dir` | `.rune/target` | Output directory for build artifacts and intermediate files. |

### Cache Store Settings (`[cas]`)

| Key | Default | Description |
| :--- | :--- | :--- |
| `cas.enabled` | `true` | Enables or disables content-addressable caching. |
| `cas.dir` | `~/.rune/cas` | Filesystem path for the local content-addressable artifact store. |
| `cas.remote` | none | Remote cache target. Can be a shared filesystem path or one or more comma-separated Cairn cache server URLs (e.g. `http://cairn.internal:7070`). |
| `cas.remote-token` | none | Bearer authentication token for Cairn cache servers. (Prefer setting via `RUNE_CAS_REMOTE_TOKEN` to avoid committing credentials). |
| `cas.remote-replicas` | `1` | Number of replicas to maintain across multiple Cairn cache servers for high availability. |
| `cas.remote-write` | `true` | Allows publishing new artifacts to the remote store. Set to `false` on developer laptops for read-only remote cache access. |
| `cas.max-size` | `auto` | Maximum disk storage for the local CAS (`10GB`, `500MB`). `auto` allocates 10% of available disk space (between 5 and 50 GiB). |
| `cas.max-age` | `30d` | Automatic garbage collection threshold. Artifacts unused for this duration are pruned during periodic maintenance. |
| `cas.compress-after` | `7d` | Automatically compresses idle artifacts using zstd after this duration of inactivity. Decompresses transparently on access. |
| `cas.verify` | `false` | Re-verifies cryptographic checksums of every artifact restored from the local store. |

### Dependency Resolution (`[resolve]`)

| Key | Default | Description |
| :--- | :--- | :--- |
| `resolve.min-release-age` | `0` | Restricts new dependency resolutions to releases published at least this long ago (e.g. `14d`, `72h`), mitigating zero-day supply chain attacks. |
| `resolve.write-cargo-lock` | `true` | Automatically creates or updates standard `Cargo.lock` files when dependencies change. |

### Testing Settings (`[test]`)

| Key | Default | Description |
| :--- | :--- | :--- |
| `test.cache` | `false` | Caches successful test outcomes based on code, fixtures, and environment variables. (Equivalent to `rune test --cached`). |
| `test.jobs` | `1` | Maximum number of test binaries running concurrently. Defaults to `1` to prevent port and database collisions across test suites. |
| `test.timeout` | `0` | Maximum execution time per test binary (e.g. `90s`, `10m`). Automatically terminates and reports hanging tests. |
| `test.schedule` | `binaries` | Test scheduling strategy. `tests` splits individual test cases into parallel batches across all CPU cores (nextest-style). |
| `test.doctests` | `true` | Automatically includes documentation tests during `rune test`. |

### Cross-Compilation (`[cross]`)

| Key | Default | Description |
| :--- | :--- | :--- |
| `cross.default-target` | none | The target `rune cross` and `--cross` build for when none is given. Plain `rune build` is unaffected. |
| `cross.engine` | `auto` | Where cross builds run: `auto` (here when this machine can build the target, else in a container), `direct` or `container`. |
| `cross.daemon` | `true` | Keep a warm container per project and target, which builds start in at once. |
| `cross.idle-timeout` | `15m` | How long a warm container waits for the next build before it stops (`0`: until stopped). |
| `cross.container-engine` | `auto` | `podman`, `docker`, `finch` or `nerdctl`; `auto` takes the first one installed. |
| `cross.image-repository` | `ghcr.io/runepm/hyprcross` | Where the per-target images come from (`<repository>/<target>:<tag>`). |
| `cross.image-tag` | `latest` | The images' tag: `latest`, a date, or a hyprcross release. |

Per-target settings (`pre-build`, `env`, `image`, `dockerfile`, `volumes`, `runner`, `sysroot`) go in `[cross.target.<triple>]` tables; see [Cross-Compilation](cross-compilation.md).

### Network Settings (`[net]`)

| Key | Default | Description |
| :--- | :--- | :--- |
| `net.offline` | `false` | Disables all network operations across resolution, download, and audit phases. |

## Environment Variables

Every configuration key can be set using environment variables prefixed with `RUNE_`. Convert periods and hyphens to underscores in uppercase:

- `build.jobs` → `RUNE_BUILD_JOBS=12`
- `build.compiler` → `RUNE_BUILD_COMPILER=auto`
- `cas.remote` → `RUNE_CAS_REMOTE=http://cairn:7070`
- `cas.remote-token` → `RUNE_CAS_REMOTE_TOKEN=secret-token`

Setting `RUNE_HOME` shifts all global Rune data (binaries, store, registry indexes, settings) to a custom directory, making it easy to create isolated environments for testing and containers.
