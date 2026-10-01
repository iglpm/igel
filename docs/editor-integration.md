# Editor and tool integration

`--message-format json` prints Cargo's JSON messages (`compiler-message`,
`compiler-artifact`, `build-finished`) on stdout, one object per line. Warnings
of unchanged crates are replayed from the store, so a tool never loses a
diagnostic because a crate was a cache hit. The output is tested against Cargo's
message shape, not yet against a live rust-analyzer. To try it there:

```json
{
  "rust-analyzer.check.overrideCommand": ["rune", "check", "--message-format=json", "--workspace", "--all-targets"],
  "rust-analyzer.cargo.buildScripts.overrideCommand": ["rune", "check", "--message-format=json", "--workspace", "--all-targets"]
}
```

The second line has rust-analyzer take build scripts' results from Rune too:
the JSON includes Cargo's `build-script-executed` messages (each script's
`OUT_DIR`, cfgs and environment, also when the result came from the store), so
`include!(concat!(env!("OUT_DIR"), …))` and script-set cfgs resolve in the editor.

`rune test --message-format json` adds one `test-suite-finished` line per test
binary (outcome, counts, every test and its status). `rune graph --units
--format json` is the build plan: every unit with its key, whether the store
has it, its features and platform, and its dependencies.
