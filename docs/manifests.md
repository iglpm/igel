# Manifest Configuration: `Cargo.toml` and `Igel.toml`

Igel offers maximum flexibility in project configuration: you can stick with standard Cargo manifests, create pure Igel projects without any Cargo files, or use both side by side.

## Manifest Resolution Rules

Igel automatically identifies workspace configuration based on files present in the root directory:

| Workspace Files | Manifest Used | Behavior |
| :--- | :--- | :--- |
| `Cargo.toml` only | `Cargo.toml` | Standard Cargo workflow. Fully compatible with both `cargo` and `igl`. |
| `Igel.toml` containing `[package]` or `[workspace]` (no `Cargo.toml`) | `Igel.toml` | Pure Igel workspace. No Cargo manifest is required. |
| `Cargo.toml` alongside a settings-only `Igel.toml` (`[build]`, `[cas]`, etc.) | `Cargo.toml` | Manifest targets resolve from `Cargo.toml`, while Igel reads project settings from `Igel.toml`. |
| Both files containing package/workspace declarations | `Igel.toml` | Igel prioritizes `Igel.toml`. `igl doctor` reports a warning to avoid confusion with `cargo`. |

## Working with Igel-Only Projects

To initialize a pure Igel project:

```bash
# Initialize a new package using Igel.toml directly
igl new my-app --manifest igl

# Initialize within an existing directory
igl init --manifest igl
```

Every command — including `igl build`, `igl run`, `igl test`, `igl add`, and `igl metadata` — operates identically whether your manifest is named `Cargo.toml` or `Igel.toml`.

## Manifest Syntax and Compatibility

The syntax inside `Igel.toml` is 100% compatible with Cargo:

```toml
[package]
name = "api-service"
version = "0.1.0"
edition = "2024"

[dependencies]
tokio = { version = "1.40", features = ["full"] }
serde = { version = "1.0", features = ["derive"] }

[build]
jobs = 8
compiler = "auto"
```

In a `Igel.toml` file, you can place Igel's configuration tables (`[build]`, `[cas]`, `[resolve]`, `[test]`) alongside package and dependency tables. When you modify settings with `igl config set build.jobs 8`, the values are saved cleanly into the same file while preserving existing comments and structure.

> **Note on `[test]` Tables**: In manifests with `[[test]]` integration test targets, Igel distinguishes the single `[test]` table as settings. To avoid ambiguity when configuring via CLI, set test options globally (`--global`) or via environment variables (`IGEL_TEST_*`).

## Trade-Offs of Pure `Igel.toml` Workspaces

If you choose to use `Igel.toml` instead of `Cargo.toml`:
- **Cargo Tooling**: Standard `cargo` will not recognize the workspace without a `Cargo.toml`.
- **Editor Tooling**: External tools that expect a literal `Cargo.toml` (like vanilla rust-analyzer) will need their check commands pointed to `igl check` (see [Editor Integration](editor-integration.md)).
- **Lockfile Consistency**: Igel continues generating standard `Cargo.lock` lockfiles in both modes. Cache keys are computed from package semantics rather than manifest filenames, ensuring that switching between `Cargo.toml` and `Igel.toml` maintains full cache reuse.
