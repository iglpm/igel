<div align="center">

# Rune

**Rust builds that never do the same work twice.**

A fast, Cargo-compatible build system and package manager for Rust.
Drop it into any Cargo project — no changes, no lock-in.

```bash
curl -fsSL https://runepm.com/install.sh | sh
```

</div>

---

Rune builds your existing Cargo projects with `rustc` directly and keeps every
compiled crate in a content-addressable store shared by all your projects,
branches, profiles and machines. Switch branches, check out an old commit, open
a second worktree, start a fresh CI runner: whatever was built before is
reused in milliseconds instead of compiled again.

```text
$ rune build
▸ resolved 249 packages (0.21s)
⠹ building ━━━━━━━━━━━━━━━╾───────── 147/197 regex-syntax, tokio, serde_derive +3
✓ dev · 12 compiled · 185 cached · 4.81s
```

## Why Rune

### ⚡ Fast where it matters

| On Rune's own workspace (249 packages) | Rune | Cargo |
| --- | ---: | ---: |
| Nothing changed: `build` | **56 ms** | 220 ms |
| Nothing changed: `check` | **57 ms** | 230 ms |
| Nothing changed: `test --no-run` | **66 ms** | 250 ms |
| `tree` | **23 ms** | 163 ms |
| Full build from scratch | **112.5 s** | 122.8 s |
| Fresh machine, shared cache | **0.65 s** | full rebuild |

- **Nothing is compiled twice.** Every crate is stored under a key that
  describes everything that went into it — compiler, flags, features,
  dependencies, the environment it read. Same inputs, same artifact, from any
  project on the machine.
- **`check` after `build` is free.** Type-checking reuses what a build already
  compiled.
- **The whole machine, not one core at a time.** Crates start the moment their
  dependencies' metadata exists, build scripts run while other dependencies
  still compile, the longest chain goes first, and memory is watched so a big
  build never swaps.

### 🧪 A test runner you can trust

- **Testing starts while the build is still running** — the first test binary
  runs the moment it links.
- **A live view** of what's running, how many tests passed, and how long each
  binary takes.
- **Stable by default.** Test binaries run one at a time as with Cargo, so
  tests that share a port or a database don't fail at random. Opt into
  parallel binaries with `--test-jobs`.
- **Built in:** timeouts for hung tests, retries with flaky-test detection,
  JUnit reports, cached test results (`--cached`: 8 s → 0.26 s when nothing
  changed), nextest-style scheduling of tests across binaries, and **code
  coverage** with `rune coverage`.

### ☁️ Built for CI and teams

- **Share the cache** between laptops and CI through a directory or a cache
  server: a developer with an empty store restores a whole workspace in under
  a second.
- **One-file cache bundles** for CI (`rune cas export` / `import`) that never
  grow with every run the way a cached `target/` does.
- **Selective CI**: `rune affected --since origin/main` lists the packages a
  change can influence, and splits them across machines.

### 🛡️ Supply-chain safety

- **Sandboxed build scripts and proc-macros**: no network, nothing writable
  outside the build (`build.sandbox = true`).
- **`rune audit`** checks every dependency against the RustSec database, with
  the path that brings it in and the fix.
- **SBOMs** (CycloneDX, SPDX), **`rune tree --exec-surface`** (everything that
  runs code at build time), and **`resolve.min-release-age`** to stay away from
  brand-new releases.

### 🧰 Everything in one tool

`outdated`, `upgrade`, `unused` dependencies, `watch`, single-file **scripts**
with dependencies, `explain` (why a crate rebuilds), `doctor` (what slows your
build down), build **timelines**, dependency graphs, `install`, `publish`,
`vendor` — plus every `cargo-<name>` plugin you already have.

## Install

**Linux and macOS**

```bash
curl -fsSL https://runepm.com/install.sh | sh
```

**Windows** (PowerShell)

```powershell
irm https://runepm.com/install.ps1 | iex
```

The installer verifies the download's checksum and puts `rune` in
`~/.rune/bin`. You need a Rust toolchain (`rustup`). Stay current with
`rune self update`; other ways to install, shell completions and uninstalling:
[docs/installation.md](docs/installation.md).

## Get started

In any Cargo project:

```bash
rune build                  # build (rune b)
rune test                   # unit, integration and doc tests
rune run -- --my-flag       # build and run
rune check                  # type-check, reusing what the build compiled
rune add serde -F derive    # add a dependency
rune watch -x test          # re-run on every change
rune help                   # everything else
```

The commands, flags and manifests are Cargo's. Your project keeps working with
plain `cargo`: Rune writes a Cargo-format `Cargo.lock`, reads
`.cargo/config.toml`, and changes nothing else.

## Works with what you have

Workspaces, features, build scripts, proc-macros, `cc`/`bindgen`/`cxx`/`pyo3`
and other native bindings, git and path dependencies, alternative registries
and mirrors, `[patch]`, cross-compilation, custom profiles, `rust-toolchain`
files, `RUSTC_WRAPPER`, rust-analyzer — tested against real-world workspaces
(ripgrep, serde, regex) and a corpus of popular crates whose own test suites
pass under Rune as they do under Cargo. See
[what's supported](docs/compatibility.md), including the known limits.

## Documentation

| | |
| --- | --- |
| [Installation](docs/installation.md) | install options, completions, where Rune keeps things |
| [Commands](docs/commands.md) | every command and flag |
| [Testing](docs/testing.md) | the test runner, flaky tests, cached results, coverage |
| [Caching](docs/caching.md) | how Rune decides what to rebuild |
| [CI and shared caches](docs/ci.md) | cache bundles, shared stores, selective CI |
| [Settings](docs/configuration.md) | `rune config` and every setting |
| [Supply chain](docs/supply-chain.md) | sandboxing, audits, SBOMs |
| [Performance](docs/performance.md) | how Rune uses the machine, build timelines |
| [All documentation](docs/README.md) | manifests, scripts, editor integration, architecture |

## Status

**Rune 2027.0** is the first stable release. Linux and macOS are its home;
Windows builds are new and less tested. It is developed against Cargo's
behaviour, with ~490 automated tests and a corpus of real crates whose own
test suites must pass under Rune as under Cargo. Something off? `rune doctor`
checks your setup. See the [roadmap](ROADMAP.md) and the
[changelog](CHANGELOG.md).

## License

MIT OR Apache-2.0
