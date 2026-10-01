# Manifests: `Cargo.toml` or `Rune.toml`

Rune reads Cargo's manifest format from either file, so a project can be plain
Cargo, **Rune-only**, or both:

| The directory has | The manifest is |
| --- | --- |
| `Cargo.toml` | `Cargo.toml` (as always) |
| `Rune.toml` with a `[package]` or `[workspace]` table (and no `Cargo.toml`) | `Rune.toml` — a Rune-only project |
| `Rune.toml` with only settings (`[build]`, `[cas]`, …) beside a `Cargo.toml` | `Cargo.toml`; the `Rune.toml` is just settings, exactly as before |
| both, and both have `[package]`/`[workspace]` | `Rune.toml` (`rune doctor` warns, because plain `cargo` would read the other) |

```bash
rune new my-app --manifest rune       # creates Rune.toml, no Cargo.toml
rune init --manifest rune             # in an existing directory; keeps a settings-only Rune.toml's settings
rune run / build / test / add / remove / metadata / locate-project …   # all work on either
```

The contents are the same `[package]`, `[dependencies]`, `[workspace]`, `[features]`,
`[profile.*]`, `[[bin]]`, … as in Cargo. Everything that finds a manifest —
workspace roots and members, `--manifest-path` (a file, or a directory), path
dependencies, git dependencies, `[patch]`, `rune add`/`remove` (which edit the file
they found), `rune metadata` / `locate-project` / `--message-format json` — asks
one place, so they cannot disagree. A workspace may mix the two, and a path or git
dependency may be `Rune.toml`-only. Registry crates are always `Cargo.toml` (that is
what `cargo publish` produces).

A `Rune.toml` manifest can also hold Rune's own settings tables next to the
manifest tables; `rune config set build.jobs 4` writes into the same file and keeps
the rest. One name collides: `[test]` (settings) and `[[test]]` (test targets). A
`[test]` *table* is read as settings; if the file has `[[test]]` targets,
`rune config set test.…` refuses and points at `--global` or the
`RUNE_TEST_…` environment variable.

What you give up without a `Cargo.toml`: plain `cargo`, `rust-analyzer` setups and
other tools that look for one only understand it. The lockfile stays `Cargo.lock`
(Cargo's format) either way. The file name is not part of any cache key, so the
`Cargo.toml` and `Rune.toml` versions of one project share compiled artifacts.
Verified on Rune's own workspace (11 manifests) with every `Cargo.toml` renamed: 182 units
build, `0 compiled` after wiping `target`, all tests pass, and `Cargo.lock` is
identical.
