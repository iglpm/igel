# Manifest Configuration: `Cargo.toml` and `Rune.toml`

Rune offers maximum flexibility in project configuration: you can stick with standard Cargo manifests, create pure Rune projects without any Cargo files, or use both side by side.

## Manifest Resolution Rules

Rune automatically identifies workspace configuration based on files present in the root directory:

| Workspace Files | Manifest Used | Behavior |
| :--- | :--- | :--- |
| `Cargo.toml` only | `Cargo.toml` | Standard Cargo workflow. Fully compatible with both `cargo` and `rune`. |
| `Rune.toml` containing `[package]` or `[workspace]` (no `Cargo.toml`) | `Rune.toml` | Pure Rune workspace. No Cargo manifest is required. |
| `Cargo.toml` alongside a settings-only `Rune.toml` (`[build]`, `[cas]`, etc.) | `Cargo.toml` | Manifest targets resolve from `Cargo.toml`, while Rune reads project settings from `Rune.toml`. |
| Both files containing package/workspace declarations | `Rune.toml` | Rune prioritizes `Rune.toml`. `rune doctor` reports a warning to avoid confusion with `cargo`. |

## Working with Rune-Only Projects

To initialize a pure Rune project:

```bash
# Initialize a new package using Rune.toml directly
rune new my-app --manifest rune

# Initialize within an existing directory
rune init --manifest rune
```

Every command — including `rune build`, `rune run`, `rune test`, `rune add`, and `rune metadata` — operates identically whether your manifest is named `Cargo.toml` or `Rune.toml`.

## Manifest Syntax and Compatibility

The syntax inside `Rune.toml` is 100% compatible with Cargo:

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

In a `Rune.toml` file, you can place Rune's configuration tables (`[build]`, `[cas]`, `[resolve]`, `[test]`) alongside package and dependency tables. When you modify settings with `rune config set build.jobs 8`, the values are saved cleanly into the same file while preserving existing comments and structure.

> **Note on `[test]` Tables**: In manifests with `[[test]]` integration test targets, Rune distinguishes the single `[test]` table as settings. To avoid ambiguity when configuring via CLI, set test options globally (`--global`) or via environment variables (`RUNE_TEST_*`).

## Trade-Offs of Pure `Rune.toml` Workspaces

If you choose to use `Rune.toml` instead of `Cargo.toml`:
- **Cargo Tooling**: Standard `cargo` will not recognize the workspace without a `Cargo.toml`.
- **Editor Tooling**: External tools that expect a literal `Cargo.toml` (like vanilla rust-analyzer) will need their check commands pointed to `rune check` (see [Editor Integration](editor-integration.md)).
- **Lockfile Consistency**: Rune continues generating standard `Cargo.lock` lockfiles in both modes. Cache keys are computed from package semantics rather than manifest filenames, ensuring that switching between `Cargo.toml` and `Rune.toml` maintains full cache reuse.
