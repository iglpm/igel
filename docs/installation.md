# Installing Rune

## The install script

Linux and macOS:

```bash
curl -fsSL https://runepm.com/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://runepm.com/install.ps1 | iex
```

The script downloads the release for your machine (Linux, macOS or Windows;
x86_64 or aarch64), checks it against its published SHA-256 and installs
`rune` to `~/.rune/bin`. It tells you how to add that directory to your `PATH`
(for your shell) if it is not there yet. On Apple silicon it installs the
native build even from a terminal running under Rosetta. The Linux builds
need glibc: on musl-based systems such as Alpine, build Rune from source.

Options, passed after `sh -s --`:

```bash
curl -fsSL https://runepm.com/install.sh | sh -s -- --version 2027.0       # a given release
curl -fsSL https://runepm.com/install.sh | sh -s -- --to ~/.local/bin     # another directory
```

`--version` takes `2027.0`, `2027.0.1` or the tag itself (`v2027.0.1`).

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

## Updating

```bash
rune self update            # to the latest release
rune self update --check    # only say whether there is a newer one
rune self update --version v2027.1.0
```

## Uninstalling

```bash
rune self uninstall              # the binary
rune self uninstall --purge      # and ~/.rune: the store, caches and settings
curl -fsSL https://runepm.com/uninstall.sh | sh            # without Rune itself
irm https://runepm.com/uninstall.ps1 | iex                  # Windows
```

Projects keep working with Cargo: Rune writes a Cargo-format `Cargo.lock` and
changes nothing else in your manifests (delete a project's `.rune/` directory
to drop its build output).
