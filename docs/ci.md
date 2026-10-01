# CI and shared caches

## A CI job

```bash
rune cas import ci.tar.gz || true                     # restore (a missing file is fine)
rune build && rune test --retries 2 --junit report.xml
rune cas export -o ci.tar.gz --for build --for test   # save exactly what this run needs
```

A bundle holds the store entries a build of the workspace needs — artifacts,
build-script results, replayed warnings, recorded outside inputs — and nothing
else, so it does not grow with every run the way a cached `target/` does
(Rune's own: 219 entries, 231 MiB gzipped; a clean checkout with it builds with
`0 compiled`). Import treats the file as untrusted: every artifact is checked
against its recorded hash and an entry the store already has is kept.
`rune affected --since origin/main --partition N/M` splits the packages a change
can influence between M machines, the same way on each.

## Sharing the store

`cas.remote` points at something several machines can reach; a build checks it
before compiling and publishes to it after. It is either a **directory** (a network
share, a CI cache volume) or **Cairn** servers — Rune's companion cache server: one small binary with
tokens, integrity checks, eviction, metrics and multi-server topologies.

```bash
export RUNE_CAS_REMOTE=http://cairn.example:7070      # or a directory: /mnt/team-cache/rune
export RUNE_CAS_REMOTE_TOKEN=<token>                  # prefer the environment to Rune.toml
rune build                                            # CI: compiles, publishes in the background
RUNE_CAS_REMOTE_WRITE=false rune build                # a developer: read-only

# several servers: entries are sharded by rendezvous hashing; keep 2 copies of each
export RUNE_CAS_REMOTE=http://a:7070,http://b:7070
export RUNE_CAS_REMOTE_REPLICAS=2
```

Whatever another machine has built is copied into your local store on a miss and
used like a local entry — artifacts, build-script output and generated files alike,
with binaries still executable. Keys do not contain the checkout path, so different
paths, users and `RUNE_HOME`s share. Measured on Rune's own 182 units (811 MB): a
developer with an empty local store restores everything from a Cairn server in
**0.65 s** (about 30 s to build), from a plain directory in 0.7 s, and — with two
replicated servers, one of them killed — still in 0.65 s. Publishing costs a CI build
about 1 %: it happens in the background.

Entries are immutable and written slot by slot with the marker file last (a
half-copied entry is invisible), checked against their manifest when pulled, and the
first publisher wins, so any number of machines can write at once. A missing or
unreachable store is just a miss; a server that cannot be connected to is skipped for
30 s (one attempt, not one per unit); a refused token is reported once.

**Trust:** a key identifies what went *into* an artifact, not what came out, so only
share a store between machines you would run each other's binaries on (give laptops
read-only access). Artifacts built elsewhere carry that machine's absolute paths in
debug info and panic messages (harmless, but they differ byte for byte from a local
build). There is no eviction on a shared *directory*; Cairn evicts by last use.

## Watching and selective CI

`rune watch` re-runs commands (`-x check`, by default) when files change, killing
and restarting one that is still running, and says which file changed. Files git
ignores don't count — a JUnit report, logs or snapshots a run writes itself would
otherwise restart it forever — and neither do the build directory, editor swap
files or the paths given with `-i`. `rune affected --since <rev>` prints the
workspace packages a diff can influence — the packages owning changed files and
everything that depends on them; `--format args` gives `-p a -p b` for
`rune test $(rune affected --since origin/main --format args)`. A change to
`Cargo.lock`, the root `Cargo.toml`, `rust-toolchain` or `.cargo/config.toml`
affects everything.
