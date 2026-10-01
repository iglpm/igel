# Caching: how Rune decides what to rebuild

Every compiled crate ("unit") goes into a content-addressable store under a
key that describes everything that went into it. A build looks each unit up
first and compiles only what is missing; the store is shared by every project,
profile and target on the machine, and can be shared between machines (see
[CI and shared caches](ci.md)).

## How keys work

A unit's key is its complete identity:

```
UnitKey = hash(
    package + source identity           registry+checksum · git commit+submodules · local files
  + target kind, crate types, edition, features, host/target role
  + compiler (executable, version, driver) + RUSTC_WRAPPER + RUSTC_WORKSPACE_WRAPPER
  + effective rustflags, linker, lints, [env]
  + the unit's effective profile (after package/build overrides)
  + how each dependency is linked (alias, normal/build) and every dependency's own key
  + values of the external inputs it declared (below)
)
```

* **Registry and git packages are immutable**, so their key never reads the
  disk; registry packages are identified by *index + checksum*, git packages by
  the *resolved commit* (a branch name never enters a key).
* **Local packages** are hashed package-wide — every file except `.rune/`,
  `target/`, directories tagged as caches (`CACHEDIR.TAG`, which Rune writes
  into every target directory and store), VCS and tool directories, editor
  droppings, nested packages, lock files, `Rune.toml`, and `Cargo.toml` itself
  (whose semantics are in the key already). Hidden files *are* inputs, since a crate may `include_str!` them. A
  size+mtime cache means warm runs only `stat` the tree.
* **The outside world.** After a unit is built, Rune records what it read beyond
  its source: a build script's `rerun-if-env-changed`/`rerun-if-changed`, the
  native libraries it linked, `env!`/`option_env!` variables (from rustc's
  dep-info), the manifest if a crate includes it, and — for units with a build
  script — the identity of the C toolchain (on macOS including the active
  Xcode/Command Line Tools and SDK). The key then follows the *current values*
  of those, so an artifact is only found again when everything it read is
  unchanged. For Apple targets, `MACOSX_DEPLOYMENT_TARGET` (and its iOS/tvOS/…
  siblings) and `SDKROOT` are part of the key when set: rustc writes them into
  the objects without listing them in its dep-info.
* **Build scripts have their own key**: the script's configuration, its
  build-dependencies and `DEP_*` values, plus — if it declared
  `rerun-if-changed` — exactly the files it watches and its own sources. Editing
  the rest of the package does not run it again (as in Cargo), and a script
  that must run again because a watched file changed is not recompiled.
* **Correctness beats hit rate.** A false rebuild is acceptable; reusing a wrong
  artifact is not. Objects in the store are immutable (first writer wins),
  read-only, published atomically, and verified on every hit.
* Compiler warnings are stored with the artifact and replayed on cache hits.

## `check` after `build`

`rune check` reuses what `rune build` already compiled: a checked crate takes
the stored result of the same crate fully built, whose `.rmeta` is what its
dependents type-check against, and its warnings are replayed. On Rune's own
workspace (181 units), `check` right after `build` compiles nothing (0.3 s;
before, it recompiled 148 units in 6 s). After an edit, only the edited crate
and what depends on it are checked for real.

rustc gives a checked and a built `.rmeta` of the same crate different crate
hashes, so the two must never meet in one dependency chain. A crate is reused
this way only if everything below it is too, and it then carries the built
crate's key, so each dependent's key records which kind it was checked against.
`build` after `check` still compiles: a check produces no code.

## Several `rune` processes at once

An editor's background check next to a terminal build, CI shards on one
checkout, two agents in one working tree: none of them blocks the others, and
none breaks the others. Rune takes no lock on the target directory; every file is
written and linked into place atomically, and a process only waits when another
one is building *the same unit* — after which it takes that result from the
store instead of compiling it again (four concurrent builds of a project with
registry dependencies compiled 10 units in total instead of 36).

## Workspace feature unification

By default `rune build -p a` compiles `a`'s dependencies with the features `a`
needs — so building different members can build the same dependency with
different features. `build.feature-unification = "workspace"` (or Cargo's
`resolver.feature-unification = "workspace"` in `.cargo/config.toml`) counts
every member's feature requests, so every subset of the workspace reuses the
same dependency builds — what `cargo hakari` achieves with a generated crate,
here without one. Only what the selected packages reach is built.

## Keeping the store small

The store caps itself at a tenth of its disk (5–50 GiB; `cas.max-size`),
removes entries no build has used for 30 days (`cas.max-age`) and compresses
those unused for 7 (`cas.compress-after`, zstd; the next build that needs one
decompresses it). `rune cas pin` keeps everything this workspace's build,
check and test need through every cleanup (`rune cas pins`, `rune cas unpin`).
