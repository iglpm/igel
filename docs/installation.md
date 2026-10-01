# Installing Rune

## The install script

```bash
curl -fsSL https://runepm.com/install.sh | sh
```

The script downloads the release for your machine (Linux or macOS, x86_64 or
aarch64), checks it against its published SHA-256 and installs `rune` to
`~/.rune/bin`. It tells you how to add that directory to your `PATH` if it is
not there yet.

Options, passed after `sh -s --`:

```bash
curl -fsSL https://runepm.com/install.sh | sh -s -- --version v0.1.0   # a given release
curl -fsSL https://runepm.com/install.sh | sh -s -- --to ~/.local/bin  # another directory
```

The same through the environment: `RUNE_VERSION`, `RUNE_INSTALL_DIR`, and
`RUNE_DOWNLOAD_BASE` for a mirror that holds the release files (an internal
artifact server, an air-gapped network).

## From source

Rune builds with stable Rust:

```bash
cargo install --git https://github.com/runepm/rune rune-cli
```

or, in a checkout, `cargo build --release` (the binary is
`target/release/rune`).

## After installing

```bash
rune version -v                                   # version, commit, toolchain it found
rune completions zsh > ~/.zfunc/_rune             # or bash, fish, elvish, powershell
rune doctor                                       # what slows your builds down, with the fix
```

Rune needs a Rust toolchain (`rustup` or a `rustc` on `PATH`), and `git` for
git dependencies. `rune coverage` adds the `llvm-tools` component on first use.

## Where Rune keeps things

| Path | What |
| --- | --- |
| `~/.rune/bin` | the `rune` binary (install script default) |
| `~/.rune/cas` | the content-addressable store of compiled artifacts, shared by all projects |
| `~/.rune/registry`, `~/.rune/cache`, `~/.rune/git` | registry indexes, downloaded crates, git checkouts, compiler probes |
| `~/.rune/advisory-db` | the advisory database `rune audit` checks against |
| `~/.rune/config.toml` | global settings (`rune config set --global …`) |
| `<project>/.rune/` | the project's build output (`target/`), timings and fingerprints |

`rune install` puts programs where `cargo install` does (`~/.cargo/bin`, or
`CARGO_INSTALL_ROOT`), and records them in Cargo's own `.crates.toml`.

`RUNE_HOME` moves everything under `~/.rune` elsewhere. The store cleans
itself up (see [caching](caching.md#keeping-the-store-small)).

## Uninstalling

Delete the binary and, if you like, `~/.rune` (or `$RUNE_HOME`). Projects keep
working with Cargo: Rune writes a Cargo-format `Cargo.lock` and changes
nothing else in your manifests.
