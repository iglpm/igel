# Cross-Compilation

Rune builds, tests and runs for other targets by itself: no `cross`, no separate install.
[hyprcross](../hyprcross/README.md) is built in and decides where a build can happen:

- **On this machine**, when it can build the target: its own target, targets that need no C
  toolchain (WebAssembly, bare metal), and targets whose C cross compiler is installed
  (`aarch64-linux-gnu-gcc`, `x86_64-w64-mingw32-gcc`, …). Tests run under QEMU, Wine or Wasmtime
  when they are installed.
- **In a container** otherwise, from the target's image (`ghcr.io/runepm/hyprcross/<target>`),
  which has the C toolchain and something to run the tests with. The container stays warm:
  the next build starts in it at once, without a `docker`/`podman` call.

In both cases this Rune does the building, with its shared cache and its built-in
[hyprlnk](../hyprlnk/README.md) linker.

## Building for another target

Two ways to say the same thing:

```sh
rune build --cross aarch64-unknown-linux-gnu --release
rune cross build -t aarch64-unknown-linux-gnu --release
```

`--cross` works with `build`, `check`, `test`, `run`, `bench`, `clippy` and `doc`. It takes the
target as its value, or from `--target`, or from `cross.default-target`:

```sh
rune test --target x86_64-pc-windows-gnu --cross     # tests under Wine
rune config set cross.default-target aarch64-unknown-linux-gnu
rune build --cross                                    # for aarch64-unknown-linux-gnu
rune run --cross -- --help                            # the program's own arguments after --
```

Every other option is Rune's as usual (`-p`, `--features`, `--release`, test filters, `-- …`).
`--cross-engine direct` or `--cross-engine container` overrides where the build runs.

The first container build for a target pulls its image and starts the container (a few
seconds, plus the download). Later builds take as long as they would on the host: with
nothing to compile, `rune build --cross aarch64-unknown-linux-gnu` in the `hyprcross` package
takes about 38 ms in a warm container.

## `rune cross`

| Command | What it does |
| :--- | :--- |
| `rune cross build`, `check`, `test`, `run`, `bench`, `clippy`, `doc` | As `rune <command> --cross` |
| `rune cross targets` | The targets with images, and how each builds on this machine |
| `rune cross doctor` | What this machine has for cross-compiling (container engine, QEMU, C cross compilers) |
| `rune cross init -t <triple>` | Set `cross.default-target` in the project's `Rune.toml` |
| `rune cross daemon status` / `stop` / `prune` | The warm containers |
| `rune cross images` / `images pull -t <triple>` | The build images on this machine, or a newer one |
| `rune cross clean` | Remove build output |

`rune cross help` lists them with their options; `rune cross help <command>` shows one.

## Settings

`rune config set` writes them (to `~/.rune/config.toml`, or the project's `Rune.toml` with
`--project`), and `RUNE_CROSS_*` variables override them, as with every Rune setting:

| Key | Default | |
| :--- | :--- | :--- |
| `cross.default-target` | none | The target `rune cross` and `--cross` build for when none is given |
| `cross.engine` | `auto` | `auto`, `direct` (always here) or `container` (always in a container) |
| `cross.daemon` | `true` | Warm containers; `false` starts a fresh one per build |
| `cross.idle-timeout` | `15m` | How long a warm container waits for the next build |
| `cross.container-engine` | `auto` | `podman`, `docker`, `finch` or `nerdctl`; `auto` takes the first installed |
| `cross.image-repository` | `ghcr.io/runepm/hyprcross` | Where per-target images come from |
| `cross.image-tag` | `latest` | `latest`, a date (`20261015`) or a hyprcross release (`0.2`) |

Per target, in `Rune.toml` (or `~/.rune/config.toml`):

```toml
[cross]
default-target = "aarch64-unknown-linux-gnu"

[cross.target.aarch64-unknown-linux-gnu]
# Libraries for the target, installed once when the warm container is created:
pre-build = ["apt-get update && apt-get install -y libssl-dev:arm64"]
env = { OPENSSL_STATIC = "1" }
# Or an image of your own, or one built from a Dockerfile (FROM the target's image):
# image = "registry.example.com/arm64-builder:1"
# dockerfile = "docker/Dockerfile.arm64"
# Extra mounts, and where tests run in direct mode:
# volumes = ["/opt/sdk:/opt/sdk:ro"]
# runner = "qemu-aarch64"
# sysroot = "/usr/aarch64-linux-gnu"
```

A project's `Hyprcross.toml` or cross-rs' `Cross.toml` is read too; Rune's settings go over
them. The tables take the same keys as hyprcross' `[target.<triple>]`: see
[hyprcross' configuration reference](../hyprcross/docs/configuration.md).

## What a container build needs

- Podman or Docker (or Finch, nerdctl). Rootless Podman works; files the build writes belong to
  you.
- The project, `~/.cargo`, `~/.rustup` and `~/.rune` are mounted at the paths they have here,
  so the container builds with your toolchain, shares Rune's cache, and reports errors with
  your paths. Path dependencies outside the project are mounted too.
- The target's standard library: Rune installs it with `rustup target add` when it's missing.

Targets without an image (macOS, Windows MSVC) build on a machine of their own kind, or here
with their SDK installed (`--cross-engine direct`).

## Troubleshooting

- `rune cross doctor` says what's missing and how to get it.
- `-v` prints every command cross-compilation runs (`rune build --cross … -v`).
- A warm container is replaced by itself when what it was made from changes (the image, the
  mounts, a new Rune, `pre-build`). `rune cross daemon stop -t <triple>` stops one by hand.
- More in [hyprcross' troubleshooting guide](../hyprcross/docs/troubleshooting.md).
