# Single-File Rust Scripts

Rune allows you to write self-contained, single-file Rust scripts that declare their own dependencies and execute directly from the terminal.

No need to configure a separate directory or create a multi-file Cargo workspace for quick automation, system scripts, or developer utilities.

## Writing a Script

A single `.rs` file can embed dependencies using standard cargo-script front matter:

```rust
#!/usr/bin/env -S rune script
---cargo
[dependencies]
serde_json = "1.0"
reqwest = { version = "0.12", features = ["blocking", "json"] }
---

fn main() {
    let payload = serde_json::json!({
        "status": "healthy",
        "service": "rune-script"
    });
    println!("{}", payload);
}
```

## Running Scripts

Make your script executable and run it directly, or invoke it via `rune script`:

```bash
# Direct execution via shebang
chmod +x hello.rs
./hello.rs

# Or execute with rune
rune script hello.rs --arg value
```

### Execution Mechanics

- **Isolated Caching**: Rune compiles the script as a lightweight transient package cached under `$RUNE_HOME/scripts/`.
- **Instant Subsequent Starts**: Because dependencies are restored directly from Rune's global content-addressable store, scripts with previously built dependencies launch in approximately **0.15 seconds**.
- **Transparent I/O**: `stdout` and `stderr` pass directly to your terminal without banner noise, and the script's exit code is returned directly to your shell.
- **Accurate Error Mapping**: Compiler diagnostic line numbers map directly to your script file, making debugging seamless.
