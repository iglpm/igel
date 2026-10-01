# Commands

Rune's commands are Cargo's, plus a few of its own. `rune help` shows the
everyday ones, grouped; `rune help --all` lists every command, and
`rune help <command>` (or `rune <command> --help`) a command's options, with
examples for the common ones.

## Everyday use

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
rune features serde            # which of serde's features are on, and who turned each on
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

## Finding your way around

`rune help` shows the everyday commands, grouped; `rune help --all` lists all of
them, and `rune help <command>` (or `rune <command> --help`) a command's options,
grouped like Cargo's, with examples for the common ones. Long output — help,
`rune search`, `rune config list` — opens in a built-in pager on a terminal:

| key | |
|---|---|
| `↑` `↓` `j` `k`, mouse wheel | scroll a line |
| `space` `b`, `PgDn` `PgUp` | a page down / up |
| `g` `G` | top / bottom |
| `/` then text, `n` `N` | search (smart case, matches highlighted), next / previous match |
| `q` `Esc` | quit (`rune search` leaves the page you were on in the terminal) |

Piped or redirected output is printed as it is; `RUNE_PAGER=never` turns the
pager off.

While it works, Rune shows one status line that never wraps or piles up: the
index files fetched while resolving, the crates being downloaded (count, bytes,
names), and during a build the crates being compiled, oldest first:

```text
⠹ building ━━━━━╾────────────────── 47/197 regex-syntax, memchr, icu_locale_core +3
```

Each phase ends in one summary line (`▸ resolved 469 packages (0.84s)`,
`▸ downloaded 468 crates (76.4 MiB, 2.12s)`), and anything printed meanwhile —
warnings, test results arriving while crates still compile — lands above the
status line instead of tearing it.
