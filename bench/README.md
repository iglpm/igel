# rune-bench

Measures Rune against Cargo on the situations that decide how a build tool
feels day to day: starting up, building from scratch, finding out nothing
changed, rebuilding after an edit, reusing earlier work, running tests, and
answering questions about the project.

```bash
cargo build --release -p rune-cli -p rune-bench
./target/release/rune-bench run                 # quick suite: tiny + medium (~10 min)
./target/release/rune-bench run --suite full    # + large, release builds, more runs
./target/release/rune-bench run --project ~/src/my-app --edit-core crates/core/src/lib.rs
./target/release/rune-bench list                # workloads and scenarios
```

Results go to `./rune-bench-results/<date-time>/`: `results.json` (every run),
`report.md` and `report.html` (tables, per-scenario charts, CPU time, peak
memory). They are rewritten after each workload, so an interrupted run keeps
what it measured. Each tool builds its own copy of every workload, so a run
needs free disk space where the results go — about 3 GiB for the quick
suite, 12 GiB for `large`, 8 GiB for a mid-sized `--project` — and stops up
front, saying so, when there is not enough. The copies are removed at the
end (`--keep-work` keeps them).

## What is measured

| Category | Scenarios | The situation |
| --- | --- | --- |
| Startup | `startup` | `--version`: the floor under every command, editor request and script |
| Cold build | `cold-build`, `cold-check`, `cold-release` | No build output and no cache: a new machine, a CI runner without a cache |
| Disk | `disk` | Bytes after a cold build (Rune: project output plus its store, hard links counted once) |
| Nothing changed | `noop-build`, `noop-check`, `noop-test` | The cost of finding out there is nothing to do |
| After an edit | `edit-leaf`, `edit-core`, `edit-leaf-check`, `edit-leaf-test`, `edit-unread` | The edit-compile(-test) loop, at the top and the bottom of the dependency graph; and a file no crate reads (notes, a log) changing in the bottom crate |
| Caching | `fresh-checkout`, `branch-switch` | Same code built before on this machine: a second worktree, `git clean`, switching branches back and forth |
| Tests | `test-run` | Running every test binary when they are built |
| Inspection | `metadata`, `tree`, `lockfile` | What IDEs and tools ask; resolving from scratch |

Each scenario is the same command for both tools, from the same starting
state. "Cold" removes Rune's store too; downloaded crates and registry
indexes stay for both tools (neither is benchmarked on network speed).

## Workloads

Generated, deterministic workspaces (no network needed):

| Name | Shape |
| --- | --- |
| `tiny` | a library and an app: where tool overhead dominates |
| `medium` | 25 crates in 4 layers: the size of a typical service |
| `large` | 73 crates in 6 layers (full suite) |

Every crate of a layer depends on two of the layer below, layer 1 on the
`core` crate, and the `app` (library, binary, integration test) on the top
layer: editing `core` rebuilds everything, editing `app` only `app`.

`--project DIR` benchmarks a real project. It is copied first (without
`target/`, `.rune/` or VCS data), so the original is never touched. Edit
scenarios change `--edit-leaf FILE` (default `src/main.rs` or `src/lib.rs`)
and `--edit-core FILE` by appending a comment; without `--edit-core`, the
core-edit and branch-switch scenarios are skipped.

## Method

- Each tool works in its own copy of a workload, with its own build output
  and (Rune) its own `RUNE_HOME`, so neither benefits from the other.
- Runs alternate between the tools (rune, cargo, cargo, rune, …) so a
  machine warming up or getting busier does not favour whichever went first.
- Cheap scenarios get warm-up runs that are not recorded; expensive ones
  (builds from scratch) start from their prepared state every run.
- Wall time, CPU time (user + system of the whole process tree, from
  `wait4`) and the peak memory of the largest process are recorded for every
  run. Reports show medians with the coefficient of variation; the overall
  figure is the geometric mean of Rune's speedups.
- The caller's environment cannot tilt the result: `CARGO_TARGET_DIR`,
  `RUSTFLAGS`, `RUSTC_WRAPPER` (sccache) and `RUNE_*` are removed for both
  tools.

## Catching regressions

```bash
rune-bench run --out before ...   # on the old version
rune-bench run --out after  ...   # on the new one
rune-bench compare before/results.json after/results.json --threshold 10
```

`compare` lists every measurement of Rune that changed and exits 1 when one
got slower by more than the threshold *and* by more than the runs' own noise
(twice their coefficient of variation): usable as a CI gate. `--tool cargo`
compares Cargo's numbers instead (a check that the machine itself did not
change). `rune-bench report results.json --format html|markdown|terminal`
renders a results file again.
