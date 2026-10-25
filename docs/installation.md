# Installing Igel

Igel distributes standalone binaries for Linux, macOS, and Windows. It integrates cleanly with your existing system and requires no special background daemons.

## Quick Installation

### Linux & macOS

Run the official installer in your terminal:

```bash
curl -fsSL https://runepm.com/install.sh | sh
```

### Windows (PowerShell)

Install via PowerShell:

```powershell
irm https://runepm.com/install.ps1 | iex
```

### What the Installer Does

The installer automatically detects your operating system and architecture (`x86_64` or `aarch64`), verifies the download against its published SHA-256 signature, and places the `igl` binary into `~/.igel/bin`. If that directory is not currently in your system `PATH`, the script displays instructions for your active shell (`bash`, `zsh`, `fish`, `elvish`, or `powershell`).

On Apple silicon, the installer selects native ARM64 binaries even when run from a Rosetta-emulated terminal. Pre-built Linux binaries link dynamically against glibc; for musl-based distributions such as Alpine, build Igel directly from source.

### Customizing the Installation

Pass installation flags using `sh -s --`:

```bash
# Install a specific release version
curl -fsSL https://runepm.com/install.sh | sh -s -- --version 2027.0

# Install to a custom directory
curl -fsSL https://runepm.com/install.sh | sh -s -- --to ~/.local/bin
```

You can configure the same settings via environment variables:
- `IGEL_VERSION`: Desired version or release tag (`2027.0`, `v2027.0.1`).
- `IGEL_INSTALL_DIR`: Custom destination directory for the executable.
- `IGEL_DOWNLOAD_BASE`: Base URL for air-gapped corporate environments or local mirror servers.

## Building from Source

You can build Igel from source using any stable Rust toolchain:

```bash
cargo install --git https://github.com/runepm/igel igel-cli
```

Or clone the repository and build a release binary locally:

```bash
git clone https://github.com/runepm/igel.git
cd igl
cargo build --release
# The compiled executable is located at target/release/igl
```

## Post-Installation Verification

Confirm your installation and inspect your development environment:

```bash
# Verify the installed binary and detected toolchain
igl version -v

# Generate shell completions (bash, zsh, fish, elvish, powershell)
igl completions zsh > ~/.zfunc/_igel

# Run system and toolchain diagnostics
igl doctor
```

Igel uses your system Rust toolchain (`rustup` or `rustc` on `PATH`) and standard `git` for resolving Git dependencies. When you run `igl coverage`, the `llvm-tools` component is installed automatically on first use.

## Filesystem Layout

Igel organizes its files systematically:

| Location | Contents |
| :--- | :--- |
| `~/.igel/bin` | The `igl` binary (default installation directory) |
| `~/.igel/cas` | Global content-addressable store containing compiled crate artifacts shared across all workspaces |
| `~/.igel/registry`, `~/.igel/cache`, `~/.igel/git` | Sparse registry indexes, downloaded package tarballs, Git checkouts, and compiler capabilities probes |
| `~/.igel/advisory-db` | Local cache of the RustSec security advisory database used by `igl audit` |
| `~/.igel/config.toml` | Global user configuration set via `igl config set --global` |
| `<project>/.igel/` | Workspace-specific build outputs (`target/`), compilation fingerprints, and timing profiles |

When installing CLI binaries via `igl install`, binaries are placed in Cargo's standard binary directory (`~/.cargo/bin` or `CARGO_INSTALL_ROOT`) and tracked in Cargo's `.crates.toml`.

To relocate all Igel data, set the `IGEL_HOME` environment variable. The cache automatically manages its footprint (see [Caching Deep Dive](caching.md#managing-store-size)).

## Managing Updates

Keep Igel current using the built-in update command:

```bash
# Update to the latest stable release
igl self update

# Check if a newer version is available without installing
igl self update --check

# Switch to a specific release tag
igl self update --version v2027.1.0
```

## Uninstalling

Remove Igel whenever you choose:

```bash
# Remove only the binary
igl self uninstall

# Remove the binary and purge all cached artifacts (~/.igel)
igl self uninstall --purge

# Uninstall via script if the binary was already removed
curl -fsSL https://runepm.com/uninstall.sh | sh
irm https://runepm.com/uninstall.ps1 | iex
```

Because Igel maintains standard `Cargo.lock` and `Cargo.toml` files, your projects remain fully operational with standard `cargo` before, during, and after using Igel. To clear local project artifacts, simply delete the `.igel/` directory inside your workspace.
