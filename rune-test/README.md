# Rune Demo Project (`rune-test`)

This is a multi-crate demo project designed to test and showcase the **Rune** build system.

## Project Structure

```
rune-test/
├── Cargo.toml
├── src/
│   └── main.rs                  (Binary application: CLI runner and statistics presenter)
└── crates/
    ├── math-engine/             (Library: Prime generation, primality testing, stats)
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── primes.rs
    │       └── stats.rs
    └── text-processor/          (Library: ASCII banners & formatted tables)
        ├── Cargo.toml
        └── src/
            ├── lib.rs
            ├── banner.rs
            └── table.rs
```

## How to Build & Run with Rune

### 1. Build in Dev Mode
```bash
../target/release/rune build
```

### 2. Run the Compiled Binary
```bash
./.rune/target/debug/rune-demo
```

### 3. Build in Release Mode
```bash
../target/release/rune build --release
./.rune/target/release/rune-demo
```

### 4. Check CAS Cache Statistics
```bash
../target/release/rune cas stats
```

### 5. Clean Generated Artifacts
```bash
../target/release/rune clean
```
