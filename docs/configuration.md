# Settings: `rune config`

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
| `build.frontend-threads` | off | The parallel front end, for nightly compilers (`-Z threads`) and [rustc-runified](../rustc-runified/README.md): `auto` gives a crate that starts while cores are idle (typically the last, largest ones) as many threads as there are idle cores; a number applies to every crate. Ignored with a stable `rustc`. With rustc-runified the output is the same whatever the thread count. |
| `build.compiler` | `auto` | `auto`: compile with [rustc-runified](../rustc-runified/README.md) when it is installed for the toolchain in use (`cargo runified driver --rune` in `rustc-runified/`, or `RUSTC` naming it), with the toolchain's `rustc` otherwise. rustc-runified is that same compiler plus what a stable `rustc` lacks: a parallel front end (`build.frontend-threads`), and executables, tests and examples that compile while their dependencies are still generating code and link once they are done. With a runified toolchain (rustc-runified's patches, `cargo runified build`) libraries also write their `.rmeta` before monomorphization, so dependents start sooner, and derive macros known to be pure keep their output in the incremental cache. `runified`: rustc-runified, or an error saying how to install it. `rustc`: always the toolchain's `rustc`. |
| `build.dependency-debuginfo` | `false` | **Off:** registry and git dependencies compile with `debug = "line-tables-only"` in the dev profile — backtraces and breakpoints work, local variables *inside dependency code* are not inspectable — which made a cold build of Rune itself about 10 % faster. **On:** Cargo's full debuginfo. A `debug` key in any profile (`[profile.dev]`, `[profile.dev.package."*"]`, …) always wins, and your own crates are never affected. |
| `build.low-priority` | `false` | Run compilers and build scripts at a lower scheduling priority, so the rest of the machine stays responsive. `rune watch` always does. |
| `build.sandbox` | `false` | Run build scripts and the compiler (proc-macros) with no network and nothing writable outside the build — see [supply chain](supply-chain.md#confining-build-time-code). |
| `build.sandbox-network` | none | Packages whose build scripts may use the network in the sandbox (comma-separated). |
| `build.feature-unification` | `selected` | `workspace`: every member's feature requests count, so any subset of the workspace shares dependency builds (see [caching](caching.md#workspace-feature-unification)). |
| `build.linker` | `hyprlnk` | `hyprlnk`: link binaries, tests, build scripts, proc-macros and dynamic libraries with [hyprlnk](../hyprlnk/README.md), the linker built into Rune (nothing to install; about twice as fast as lld and Apple's ld on Rune's own build, and one and a half to two times as fast as lld and mold on Linux). It takes precedence over `target.<triple>.linker`; on Linux, a C compiler named there (a cross compiler, say) is the one hyprlnk links through, for the C runtime, else `cc`. Links Linux targets (`*-unknown-linux-gnu*`, `*-unknown-linux-musl*`: x86-64, x86, AArch64, ARM, RISC-V 64, POWER little-endian, s390x, LoongArch), Windows targets (`*-pc-windows-msvc`: x64, ARM64, x86), PDBs included, and macOS targets (`*-apple-darwin`: arm64, arm64e, x86_64), debug maps and code signatures included; other targets keep their linker. `default`: the linker `rustc` and `.cargo/config.toml` choose. |
| `build.target-dir` | `.rune/target` | Build output directory. |
| `cas.enabled` | `true` | Use the CAS at all. |
| `cas.dir` | `~/.rune/cas` | CAS location. |
| `cas.remote` | none | A store shared with other machines, checked before compiling and published to after: a directory, or one or more Cairn URLs separated by commas (entries are sharded across them) — see [CI and shared caches](ci.md#sharing-the-store). |
| `cas.remote-token` | none | Bearer token for a Cairn server. Prefer `RUNE_CAS_REMOTE_TOKEN`: `Rune.toml` is readable by everyone who can read the project. |
| `cas.remote-replicas` | `1` | With several Cairn URLs: how many servers hold each entry (`2` survives losing one). |
| `cas.remote-write` | `true` | Publish to `cas.remote`. Set `false` where only CI should write (e.g. developers). |
| `cas.max-age` | `30d` | Entries no build has used for this long are removed by a background `rune cas gc` that a build starts at most once a day (like Cargo's automatic cache cleanup). `0` keeps everything. |
| `cas.max-size` | `auto` | Evict least-recently-used entries after a build when the CAS grows past this (`20GB`, `512MiB`). `auto`: a tenth of its disk, 5–50 GiB; `0`: unlimited. Entries of pinned workspaces (`rune cas pin`) stay. |
| `cas.compress-after` | `7d` | Compress (zstd) entries no build has used for this long, in the daily background cleanup; the next build that needs one decompresses it. `0`: never. |
| `cas.verify` | `false` | Re-hash every artifact restored from the CAS (entries are always checked against their key and recorded sizes). |
| `resolve.min-release-age` | `0` | Only choose releases published at least this long ago (`14d`, `72h`) when resolving new versions; locked versions stay. Malicious releases are usually yanked within days. |
| `resolve.write-cargo-lock` | `true` | Create/update `Cargo.lock` when the set of locked packages changes. An up-to-date lockfile is never touched. |
| `net.offline` | `false` | Never touch the network. |
| `test.doctests` | `true` | Run documentation tests in `rune test`. |
| `test.cache` | `false` | Same as `rune test --cached`: remember passing test binaries (see [testing](testing.md#cached-test-results)). |
| `test.jobs` | `1` | How many test binaries (and doc-test runs) run at once; same as `rune test --test-jobs N`. More is faster, but tests in different binaries that share a port, a file or a database then fail now and then, which is why Cargo runs one at a time too. The build goes on meanwhile either way. |
| `test.timeout` | `0` | Stop a test binary (or doc-test run) that runs longer than this (`90s`, `10m`) and report it as failed, naming the tests that had not finished; same as `rune test --timeout`. `0`: no limit, as with Cargo. |
| `test.schedule` | `binaries` | `tests` (or `rune test --schedule tests`): each test binary's tests are split into batches, each its own process, scheduled across all binaries at once (like cargo-nextest). Tests then no longer share a process. |

Each key maps to an environment variable, e.g. `RUNE_BUILD_RELEASE_REBUILD_ALL=1`.
`RUNE_HOME` moves everything Rune keeps outside your project (settings, CAS,
registry cache, git checkouts), which is also how the test suite isolates itself.
