# Cross-compilation

Igel builds, tests, and runs for other targets through the bundled
[hyprcross](../hyprcross/README.md) integration. hyprcross runs directly on the host, installs
small target SDKs when required, and configures [hyprlnk](../hyprlnk/README.md) or the target's
default linker.

## Building for another target

These forms are equivalent:

```sh
igl build --cross aarch64-unknown-linux-gnu --release
igl cross build -t aarch64-unknown-linux-gnu --release
```

`--cross` works with `build`, `check`, `test`, `run`, `bench`, `clippy`, and `doc`. Supply the
target as its value, with `--target`, or through `cross.default-target`.

```sh
igl test --target x86_64-pc-windows-gnu --cross
igl config set cross.default-target aarch64-unknown-linux-gnu
igl build --cross
igl run --cross -- --help
```

Igel keeps its usual package, feature, profile, and test-filter options. The first build may
download an SDK; later projects reuse it from hyprcross's data directory.

## Commands

| Command | Purpose |
| :--- | :--- |
| `igl cross build`, `check`, `test`, `run`, `bench`, `clippy`, `doc` | Run the matching Igel command for a target |
| `igl cross targets` | List supported targets |
| `igl cross doctor` | Check SDKs, linkers, and runners |
| `igl cross init -t <triple>` | Set the project's default cross target |
| `igl cross clean` | Remove build output |

Foreign Linux tests and programs run through QEMU when it is installed. Windows GNU targets
can use Wine, and WASI targets can use Wasmtime. A runner is optional for building.

## Settings

`igl config set` writes settings to `~/.igel/config.toml`, or to the project's `Igel.toml`
with `--project`. `IGEL_CROSS_*` environment variables override configuration.

```toml
[cross]
default-target = "aarch64-unknown-linux-gnu"

[cross.target.aarch64-unknown-linux-gnu]
env = { OPENSSL_STATIC = "1" }
```

A project's `Hyprcross.toml` or `Cross.toml` is also read. SDK locations can be overridden with
`HYPRCROSS_SDK_PATH` or the target-specific form described in
[hyprcross's configuration reference](../hyprcross/docs/configuration.md).

## Troubleshooting

- Run `igl cross doctor` to see missing target components.
- Add `-v` to print the invoked builder, linker, and runner commands.
- Reinstall an invalid SDK with `hyprcross sdk remove <target>` followed by `hyprcross sdk install <target>`.
- See [hyprcross's troubleshooting guide](../hyprcross/docs/troubleshooting.md) for linker and runner details.
