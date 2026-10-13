# Rune Demonstration Workspace (`rune-test`)

This multi-crate project serves as a practical, lightweight showcase for testing and exploring the **Rune** build system.

## Workspace Layout

```text
rune-test/
├── Cargo.toml
├── src/
│   └── main.rs                  # CLI runner and presentation layer
└── crates/
    ├── math-engine/             # Library: Prime generation, primality testing, statistics
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── primes.rs
    │       └── stats.rs
    └── text-processor/          # Library: Terminal banners and ASCII tables
        ├── Cargo.toml
        └── src/
            ├── lib.rs
            ├── banner.rs
            └── table.rs
```

## Quickstart

Run these everyday commands from within this directory to see Rune in action:

### 1. Compile in Development Mode

```bash
../target/release/rune build
```

### 2. Execute the Compiled Application

```bash
./.rune/target/debug/rune-demo
```

### 3. Build with Optimizations

```bash
../target/release/rune build --release
./.rune/target/release/rune-demo
```

### 4. Inspect Cache Telemetry

```bash
../target/release/rune cas du
```

### 5. Clean Generated Workspace Outputs

```bash
../target/release/rune clean
```
