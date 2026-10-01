# Single-file scripts

A single `.rs` file can carry its own dependencies:

```rust
#!/usr/bin/env -S rune script
---cargo
[dependencies]
serde_json = "1"
---
fn main() { println!("{}", serde_json::json!({ "hello": "world" })); }
```

`rune script hello.rs [args]` (or `./hello.rs` with the shebang) builds it as a
throwaway package under `$RUNE_HOME/scripts/` and runs it; stdout is the script's
output and nothing else, and its exit code is Rune's. Because dependencies come
from the store, a script whose dependencies any project has built before starts in
about 0.15 s. Line numbers in errors match the file.
