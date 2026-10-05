# Troubleshooting

What to do when a build fails or does something you did not expect. Most
problems fall into one of three groups: the project itself (it fails with
Cargo too), the machine (a missing tool, little memory or disk), or Rune.
Start by telling them apart.

## First steps

```bash
rune doctor          # missing tools, slow linkers, disk and memory, with the fix
cargo build          # does the project build with Cargo? (in its own target/)
rune explain         # which units the next build would compile, and why
```

If `cargo build` fails the same way, the problem is in the project, not in
Rune. If only Rune fails, the sections below cover the usual causes; if none
fits, [report it](#reporting-a-bug).

## The build compiles more than it should

`rune explain` lists every unit the next build would compile and what changed
in each — a source file, a feature, a flag, an environment variable a build
script watches, a dependency:

```text
▸ 1 of 197 units would be compiled (0 come from the store)

  app v0.1.0 (bin `app`) [dev]
    · sources changed: src/main.rs (modified)
```

Common causes:

- **A build script without `rerun-if-changed`** is rerun (and its crate
  recompiled) when *any* file of its package changes, as with Cargo. Add
  `println!("cargo::rerun-if-changed=build.rs")` and the files it reads.
- **An environment variable a build script reads** (`rerun-if-env-changed`)
  differs between shells, IDEs and CI. `rune explain` names it.
- **Different flags** — `RUSTFLAGS`, `--features`, `--profile`, a
  `RUSTC_WRAPPER` — make different artifacts; each is stored separately and
  stays in the store.
- **A new Rune version** may change how crates are compiled; the first build
  after an update then compiles everything once.

## The build is slow

A build longer than 10 seconds that left cores idle ends with a note naming the
chain of crates that had to be built one after another. `rune build --timings`
writes a timeline to `.rune/timings/rune-timing.html` with that chain
highlighted. See [performance](performance.md) for what helps: splitting the
largest crate of the chain, a faster linker (`rune doctor` says which), more
memory.

`note: memory ran short, so fewer compilers ran at once` means Rune held back
compilers so the machine would not swap. Close memory-hungry programs or lower
`-j`; it is not an error.

## Errors from the compiler about crates

```text
error[E0463]: can't find crate for `tracing`
error[E0460]: found possibly newer version of crate `dep` which `mid` depends on
```

when the same project builds with Cargo, rustc found two different builds of
one dependency side by side. Rune records, for every stored crate, the builds of
its dependencies it was compiled against, and restores it only next to those,
so this should not happen; if it does, it is a bug — please
[report it](#reporting-a-bug). To get going again, rebuild everything once:

```bash
rune build --rebuild-all     # compile every unit, dependencies included
```

## Errors about the lockfile or resolution

`the dependency resolution does not contain …` — a dependency a build needs has
no package in the lockfile, and resolving again did not find one either. Check
that the dependency's version requirement can be met (`rune update -p <name>`
tries again for one package), and that a `[patch]` or source replacement in
`.cargo/config.toml` provides it.

`failed to select a version for …` — the requirements of your dependencies
cannot all be met at once; the message explains which ones conflict.
`rune tree -i <name>` and `rune why <name>` show who asks for what.

`the lock file … needs to be updated but --locked was passed` — the manifests
changed since `Cargo.lock` was written. Run `rune build` once without
`--locked` (or `rune update`) and commit the new `Cargo.lock`.

Offline (`--offline`, `net.offline`): only versions already in the local index
cache can be chosen. Run once with the network to fill it.

## Build scripts and native code

Crates with C code (`ring`, `openssl-sys`, `libz-sys`, …) need a C compiler
and often `pkg-config` or the library's headers. A failing build script prints
its output, and Rune adds a hint when it recognizes a missing tool. On Windows,
install the *Desktop development with C++* workload of the Visual Studio Build
Tools.

With `build.sandbox = true`, build scripts cannot use the network or write
outside the build. A script that downloads something needs an exception:
`rune config set build.sandbox-network <package>` (see
[supply chain](supply-chain.md#confining-build-time-code)).

## Disk space

The store keeps itself under a tenth of its disk (5–50 GiB, `cas.max-size`) and
drops entries no build has used for 30 days. To see and reclaim space now:

```bash
rune cas du                       # which packages take the space, and how much is idle
rune cas gc --max-age 7d          # remove entries unused for a week
rune cas gc --max-size 10GB       # shrink to 10 GB, least recently used first
rune clean                        # this project's build output (.rune/target)
```

Nothing is lost for good: anything removed is compiled again when needed.

## Windows

- **"Access is denied" replacing `rune.exe`** while building Rune itself: a
  `rune` started from that binary is still running. Wait for it, or build into
  another `--target-dir`.
- **Long paths**: very deep project directories can exceed Windows' 260
  character path limit inside `.rune/target`. Enable long paths
  (`LongPathsEnabled` in the registry, or the group policy), or move the project
  closer to the drive root.
- **Antivirus**: real-time scanning of `.rune/` and `~/.rune/` slows builds a
  lot. Excluding both directories (as is commonly done for `target/`) helps.

## Starting from a clean slate

In order of how much they throw away:

```bash
rune build --rebuild-all     # compile everything again; the store keeps the results
rune clean                   # delete this project's build output
rm -rf ~/.rune/cas           # delete the whole store (every project compiles again)
```

None of these touch your sources, `Cargo.lock` or settings.

## Seeing what Rune does

```bash
rune build -v                          # verbose output
RUST_LOG=debug rune build              # Rune's internal log (cache hits, restores, rustc commands)
RUNE_TIMINGS=1 rune build              # how long each phase of the command took
rune graph --units --format json       # the build plan: every unit, its key, whether it is stored
```

`RUST_LOG=rune_scheduler=debug` shows, among other things, every `rustc`
command line (`Running …`) and why a stored unit was not reused.

## Reporting a bug

Open an issue at <https://github.com/runepm/rune/issues> with:

- `rune version -v` (Rune, its commit, and the toolchain it found);
- your operating system;
- the command and its full output, ideally with `RUST_LOG=debug`;
- whether `cargo build` succeeds on the same project;
- if you can, a small project that shows the problem.

A crash prints what happened and where to report it; include that text.
