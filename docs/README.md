# Rune documentation

Start with [installation](installation.md) and the [commands](commands.md);
everything else is reference.

| Page | What it covers |
| --- | --- |
| [Installation](installation.md) | The install script and its options, building from source, shell completions, where Rune keeps its files |
| [Commands](commands.md) | Everyday commands, Cargo's selection flags, help, the pager and progress display |
| [Cargo compatibility](compatibility.md) | Manifests, dependencies, features, build scripts, native libraries, toolchains, profiles, resolution, the registry workflow — and the known limits |
| [Manifests](manifests.md) | `Cargo.toml`, or a Rune-only `Rune.toml` |
| [Settings](configuration.md) | `rune config` and every setting |
| [Testing](testing.md) | How `rune test` runs tests, retries and flaky tests, cached test results, `rune coverage` |
| [Caching](caching.md) | What goes into a unit's key, `check` after `build`, concurrent builds, feature unification, keeping the store small |
| [Performance](performance.md) | How Rune uses cores and memory, and finding what makes a build long |
| [CI and shared caches](ci.md) | Store bundles for CI, a store shared between machines, selective CI |
| [Supply chain](supply-chain.md) | Sandboxed build scripts, resolution, `outdated`/`upgrade`/`unused`, SBOMs, `rune audit` |
| [Scripts](scripts.md) | Single-file Rust scripts with dependencies |
| [Editor integration](editor-integration.md) | JSON messages, rust-analyzer, the build plan as JSON |
| [Architecture](architecture.md) | The crates Rune is made of, building and testing Rune itself |
| [Benchmarks](../bench/README.md) | `rune-bench`: measuring Rune against Cargo on generated and real projects |

[ROADMAP.md](../ROADMAP.md) has the status against the compatibility
specification, measurements and what comes next; [CHANGELOG.md](../CHANGELOG.md)
what changed in each release.
