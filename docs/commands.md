# CLI Commands Reference

Igel provides a comprehensive command-line interface that mirrors Cargo's commands while adding high-productivity developer utilities. Run `igl help` to see common workflows, `igl help --all` for the full command set, or `igl help <command>` for in-depth documentation and examples.

## Everyday Developer Commands

```bash
# Compilation & Execution
igl build                         # Compile active package or workspace (alias: igl b)
igl build --release -j 8          # Multi-threaded release build
igl build --target aarch64-unknown-linux-gnu # Build for a target triple (its toolchain installed here)
igl build --cross aarch64-unknown-linux-gnu # Cross-compile, in a container when this machine can't
igl cross test -t x86_64-pc-windows-gnu # Run the tests of another target (here: under Wine)
igl build --deps-only             # Compile only external dependencies (ideal for Docker layers)
igl check                         # Fast type-checking, reusing cached build outputs
igl run -- --arg value            # Compile and execute binary with arguments
igl watch -x check -x "test --lib"# Continuous watch mode, re-running on file modifications

# Testing & Quality
igl test                          # Run unit, integration, and doc tests
igl test filter -- --nocapture    # Run matching tests with unfiltered stdout
igl test --cached                 # Re-use passing test results when inputs are unchanged
igl test --failed                 # Run only previously failed tests
igl test --retries 2 --junit report.xml # Flaky-test retries with JUnit XML export
igl bench                         # Run benchmarks across the workspace
igl clippy -- -D warnings         # Lint the workspace's packages
igl fmt --all -- --check          # Check workspace source formatting
igl fix                           # Automatically apply compiler suggestions

# Dependency & Workspace Management
igl add serde -F derive           # Add dependency and display active features
igl update                        # Fetch modified registry indexes and refresh lockfile
igl update -p serde --precise 1.0.200 # Pin specific dependency version
igl tree -i serde                 # Inspect inverse dependency tree for serde
igl tree -e features -i serde     # Trace which packages enable specific features
igl why serde                     # Explain why a dependency is pulled into the workspace
igl features serde                # Display active features and origin packages
igl new my-app                    # Initialize a new package (or: igl init)
igl vendor                        # Vendor dependencies into vendor/ directory

# Inspection & Diagnostics
igl doctor                        # Audit environment, toolchain, and linker bottlenecks
igl explain                       # Show what units the next build will compile and why
igl build --timings               # Generate HTML build timeline and critical path graph
igl cas du                        # Report CAS disk usage, active crates, and idle entries
igl graph --format mermaid        # Output package graph in Mermaid, DOT, or JSON format

# Supply Chain & Security
igl audit                         # Scan dependencies against RustSec/OSV advisories
igl outdated                      # Detect available updates beyond lockfile pins
igl upgrade [--incompatible]      # Bump manifest version requirements in-place
igl unused [--fix]                # Identify unused direct dependencies using rustc lints
igl sbom -o bom.json              # Generate CycloneDX or SPDX Software Bill of Materials
igl tree --exec-surface           # List all build scripts and proc-macros executing code
```

## Flags and Filtering

Igel fully supports Cargo's target and package selection flags:

- **Package Selection**: `-p/--package`, `--workspace`, `--exclude`
- **Target Selection**: `--bin`, `--lib`, `--example`, `--test`, `--bench`, `--all-targets`
- **Feature Management**: `-F/--features`, `--all-features`, `--no-default-features`
- **Compilation Options**: `--profile`, `--target`, `--target-dir`, `--jobs/-j`, `--timings`
- **Manifest Control**: `--manifest-path`, `--locked`, `--offline`, `--frozen`, `--ignore-rust-version`

Configuration aliases in `.cargo/config.toml` (such as `xtask = "run --package xtask --"`) execute as direct subcommands. Environment variables like `CARGO_PROFILE_<NAME>_<KEY>` override profile settings seamlessly.

Standard status outputs are written to `stderr`, while data outputs (`metadata`, JSON streams, program output) are directed to `stdout` for clean Unix piping.

## Interactive Terminal Pager

When viewing long text output — such as `igl help`, `igl search`, or `igl config list` — Igel launches an interactive, zero-dependency terminal pager:

| Key Binding | Action |
| :--- | :--- |
| `↑` / `↓`, `j` / `k`, Mouse wheel | Scroll by one line |
| `Space` / `b`, `PgDn` / `PgUp` | Scroll forward or backward by one page |
| `g` / `G` | Jump to the top or bottom of the output |
| `/` followed by text | Search (smart case with highlighted occurrences) |
| `n` / `N` | Jump to the next or previous search match |
| `q` / `Esc` | Exit pager (keeps your terminal screen intact) |

When Igel's output is piped or redirected, the pager disables automatically. You can disable it permanently by setting `IGEL_PAGER=never`.

## Clean Terminal Dashboard

During execution, Igel maintains a compact, non-wrapping status bar that reports current downloads, resolution times, and active compilations:

```text
⠹ building ━━━━━╾────────────────── 47/197 regex-syntax, memchr, icu_locale_core +3
```

Each stage concludes with an execution summary (`▸ resolved 469 packages (0.84s)`). Warnings, errors, and test outputs are printed neatly above the active progress bar without tearing terminal output.
