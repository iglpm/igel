# Igel Demonstration Workspace (`igel-test`)

This multi-crate project serves as a practical, lightweight showcase for testing and exploring the **Igel** build system.

## Workspace Layout

```text
igel-test/
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

Run these everyday commands from within this directory to see Igel in action:

### 1. Compile in Development Mode

```bash
../target/release/igl build
```

### 2. Execute the Compiled Application

```bash
./.igel/target/debug/igel-demo
```

### 3. Build with Optimizations

```bash
../target/release/igl build --release
./.igel/target/release/igel-demo
```

### 4. Inspect Cache Telemetry

```bash
../target/release/igl cas du
```

### 5. Clean Generated Workspace Outputs

```bash
../target/release/igl clean
```
