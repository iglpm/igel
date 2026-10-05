# Getting started

This page takes you from a fresh install to everyday use: a first project, an
existing Cargo project, and the handful of commands you will use every day.
Rune is a drop-in for Cargo: the same manifests, the same commands, the same
flags. Where it differs is *what happens underneath* — every compiled crate goes
into a store shared by all your projects, so work done once is never done again.

## Install

```bash
curl -fsSL https://runepm.com/install.sh | sh        # Linux and macOS
irm https://runepm.com/install.ps1 | iex             # Windows (PowerShell)
```

Rune needs a Rust toolchain (`rustup` or a `rustc` on `PATH`). Check that it
found one, and what it thinks of your machine:

```bash
rune version -v
rune doctor
```

More options — a given version, another directory, building from source — are
in [installation](installation.md).

## A first project

```bash
rune new hello
cd hello
rune run
```

```text
✓ dev · 1 compiled · 0 cached · 0.55s
▸ running .rune/target/debug/hello
Hello, world!
```

`rune new` creates the same project `cargo new` would (with a `.gitignore` that
leaves out `target/` and `.rune/`). Run it again and nothing is compiled: the
result comes from the store.

Add a dependency and use it:

```bash
rune add serde -F derive      # lists the release's features, + for those turned on
rune build
rune test
```

## An existing Cargo project

There is nothing to convert. In any directory with a `Cargo.toml`:

```bash
rune build
```

Rune reads `Cargo.toml`, `Cargo.lock` and `.cargo/config.toml` as Cargo does,
keeps the versions your `Cargo.lock` pins, and builds. Your project keeps
working with `cargo` — you can switch back and forth.

What Rune adds to a project:

| File | What it is | Commit it? |
| --- | --- | --- |
| `Cargo.lock` | The lockfile, in Cargo's format; created if missing, rewritten only when the set of locked packages changes | Yes, as with Cargo |
| `Rune.lock.bin` | A binary copy of the resolution, so the next command skips resolving; rebuilt whenever a manifest or `Cargo.lock` changes | Not needed |
| `.rune/` | Build output (`.rune/target/`), timings and fingerprints — Rune's `target/` | No: add `.rune/` to `.gitignore` |
| `Rune.toml` | Only if you create it: project [settings](configuration.md) (or a Rune-only [manifest](manifests.md)) | Yes |

Build output goes to `.rune/target/<profile>/`, laid out as Cargo lays out
`target/`: `rune build` puts your program at `.rune/target/debug/<name>`, and
`--release` at `.rune/target/release/<name>`. `--target-dir` (or
`CARGO_TARGET_DIR`, or the `build.target-dir` setting) puts it elsewhere.

## The everyday loop

```bash
rune check                 # type-check: fastest feedback while editing
rune build                 # compile (rune b)
rune run -- --my-flag      # build and run; arguments after --
rune test                  # unit, integration and doc tests
rune test my_filter        # only tests whose name contains my_filter
rune test --failed         # only what failed last time
rune watch -x check        # re-run on every change
```

Each command ends with one line saying what happened:

```text
✓ dev · 3 compiled · 194 cached · 2.71s
```

*compiled* units ran the compiler; *cached* ones came from the store. The
first build of a project compiles everything that no other project on your
machine has compiled before; later builds compile only what you changed and
what depends on it.

## Why it is fast

Every compiled crate is stored under a key that describes everything that went
into it: the compiler, flags, features, dependencies and — for your own crates
— the files the compiler read. Same inputs, same key, same artifact, from any
project. That is why

- **switching branches** or undoing an edit restores the earlier artifacts
  instead of compiling them again;
- **a second checkout or worktree** of the same project builds in a second;
- **`rune check` after `rune build`** compiles nothing: a check reuses the
  build's results;
- **dependencies shared between your projects** (`serde`, `tokio`, `syn`) are
  compiled once per machine, not once per project.

[Caching](caching.md) explains what goes into a key and how the store is kept
small; [CI and shared caches](ci.md) shows how to share it with CI and your
team.

## Where to go next

- [Commands](commands.md) — every command, with Cargo's flags.
- [Testing](testing.md) — retries, flaky tests, cached results, coverage.
- [Settings](configuration.md) — `rune config` and every setting.
- [Troubleshooting](troubleshooting.md) — when something does not work.
- [Cargo compatibility](compatibility.md) — what is supported, and the known
  limits.
