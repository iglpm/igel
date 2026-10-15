# CLI Commands Reference

Rune provides a comprehensive command-line interface that mirrors Cargo's commands while adding high-productivity developer utilities. Run `rune help` to see common workflows, `rune help --all` for the full command set, or `rune help <command>` for in-depth documentation and examples.

## Everyday Developer Commands

```bash
# Compilation & Execution
rune build                         # Compile active package or workspace (alias: rune b)
rune build --release -j 8          # Multi-threaded release build
rune build --target aarch64-unknown-linux-gnu # Build for a target triple (its toolchain installed here)
rune build --cross aarch64-unknown-linux-gnu # Cross-compile, in a container when this machine can't
rune cross test -t x86_64-pc-windows-gnu # Run the tests of another target (here: under Wine)
rune build --deps-only             # Compile only external dependencies (ideal for Docker layers)
rune check                         # Fast type-checking, reusing cached build outputs
rune run -- --arg value            # Compile and execute binary with arguments
rune watch -x check -x "test --lib"# Continuous watch mode, re-running on file modifications

# Testing & Quality
rune test                          # Run unit, integration, and doc tests
rune test filter -- --nocapture    # Run matching tests with unfiltered stdout
rune test --cached                 # Re-use passing test results when inputs are unchanged
rune test --failed                 # Run only previously failed tests
rune test --retries 2 --junit report.xml # Flaky-test retries with JUnit XML export
rune bench                         # Run benchmarks across the workspace
rune clippy -- -D warnings         # Lint the workspace's packages
rune fmt --all -- --check          # Check workspace source formatting
rune fix                           # Automatically apply compiler suggestions

# Dependency & Workspace Management
rune add serde -F derive           # Add dependency and display active features
rune update                        # Fetch modified registry indexes and refresh lockfile
rune update -p serde --precise 1.0.200 # Pin specific dependency version
rune tree -i serde                 # Inspect inverse dependency tree for serde
rune tree -e features -i serde     # Trace which packages enable specific features
rune why serde                     # Explain why a dependency is pulled into the workspace
rune features serde                # Display active features and origin packages
rune new my-app                    # Initialize a new package (or: rune init)
rune vendor                        # Vendor dependencies into vendor/ directory

# Inspection & Diagnostics
rune doctor                        # Audit environment, toolchain, and linker bottlenecks
rune explain                       # Show what units the next build will compile and why
rune build --timings               # Generate HTML build timeline and critical path graph
rune cas du                        # Report CAS disk usage, active crates, and idle entries
rune graph --format mermaid        # Output package graph in Mermaid, DOT, or JSON format

# Supply Chain & Security
rune audit                         # Scan dependencies against RustSec/OSV advisories
rune outdated                      # Detect available updates beyond lockfile pins
rune upgrade [--incompatible]      # Bump manifest version requirements in-place
rune unused [--fix]                # Identify unused direct dependencies using rustc lints
rune sbom -o bom.json              # Generate CycloneDX or SPDX Software Bill of Materials
rune tree --exec-surface           # List all build scripts and proc-macros executing code
```

## Flags and Filtering

Rune fully supports Cargo's target and package selection flags:

- **Package Selection**: `-p/--package`, `--workspace`, `--exclude`
- **Target Selection**: `--bin`, `--lib`, `--example`, `--test`, `--bench`, `--all-targets`
- **Feature Management**: `-F/--features`, `--all-features`, `--no-default-features`
- **Compilation Options**: `--profile`, `--target`, `--target-dir`, `--jobs/-j`, `--timings`
- **Manifest Control**: `--manifest-path`, `--locked`, `--offline`, `--frozen`, `--ignore-rust-version`

Configuration aliases in `.cargo/config.toml` (such as `xtask = "run --package xtask --"`) execute as direct subcommands. Environment variables like `CARGO_PROFILE_<NAME>_<KEY>` override profile settings seamlessly.

Standard status outputs are written to `stderr`, while data outputs (`metadata`, JSON streams, program output) are directed to `stdout` for clean Unix piping.

## Interactive Terminal Pager

When viewing long text output — such as `rune help`, `rune search`, or `rune config list` — Rune launches an interactive, zero-dependency terminal pager:

| Key Binding | Action |
| :--- | :--- |
| `↑` / `↓`, `j` / `k`, Mouse wheel | Scroll by one line |
| `Space` / `b`, `PgDn` / `PgUp` | Scroll forward or backward by one page |
| `g` / `G` | Jump to the top or bottom of the output |
| `/` followed by text | Search (smart case with highlighted occurrences) |
| `n` / `N` | Jump to the next or previous search match |
| `q` / `Esc` | Exit pager (keeps your terminal screen intact) |

When Rune's output is piped or redirected, the pager disables automatically. You can disable it permanently by setting `RUNE_PAGER=never`.

## Clean Terminal Dashboard

During execution, Rune maintains a compact, non-wrapping status bar that reports current downloads, resolution times, and active compilations:

```text
⠹ building ━━━━━╾────────────────── 47/197 regex-syntax, memchr, icu_locale_core +3
```

Each stage concludes with an execution summary (`▸ resolved 469 packages (0.84s)`). Warnings, errors, and test outputs are printed neatly above the active progress bar without tearing terminal output.
