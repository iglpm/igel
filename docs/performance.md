# Performance & Resource Management

Igel is engineered to maximize hardware efficiency across CPU cores, memory bandwidth, and disk I/O, transforming how fast large Rust projects compile.

Instead of idling cores while long dependency chains compile, Igel overlaps pipeline stages, schedules critical paths first, and throttles concurrency dynamically to keep your system responsive.

## Whole-Machine Saturation

### Dynamic Jobserver Coordination

By default, Igel allocates approximately 1.3 compiler worker jobs per physical core (`build.jobs`). All `rustc` instances, build scripts, and native C/C++ compilers share a single internal GNU jobserver token pool:
- When a large compilation reaches codegen at the end of a build, it can borrow idle tokens for codegen threads without exceeding system thread budgets.
- Token borrowing is non-blocking: a newly scheduled compilation step immediately claims an idle slot or borrows forward, ensuring that the critical compilation path never queues behind secondary code generation.

### Metadata Pipelining

Compilation steps do not wait for dependencies to finish linking or emitting object files. The moment a dependency emits its lightweight `.rmeta` metadata interface, dependent crates begin compilation in parallel.

Build scripts (`build.rs`) run as discrete execution stages as soon as their host build-dependencies link, compiling concurrently with unrelated target dependencies across your workspace.

## Intelligent Memory Pressure Management

A common issue during heavily parallel builds on high-core machines is running out of physical RAM and triggering swap storms or the kernel OOM killer.

Igel prevents out-of-memory crashes through active telemetry:
- **Real-Time Memory Monitoring**: Before spawning an additional compiler process, Igel queries system memory availability (`MemAvailable` on Linux, kernel memory pressure on macOS).
- **Adaptive Admission Control**: If physical memory is constrained, Igel delays scheduling new jobs until active compilation units finish and free their memory.
- **Learned Peak Memory Tracking**: Igel measures each crate's peak memory usage via `wait4` and records it across runs. A crate known to require 2 GiB will wait until 2 GiB of RAM is free rather than spawning alongside three other memory-heavy units.
- **Low-Priority Scheduling**: Setting `build.low-priority = true` (or running `igl watch`) schedules compiler processes with lower OS scheduling priority (`nice`), keeping your desktop UI and editor completely smooth during full builds.

## Eliminating Build Bottlenecks

### Identifying the Critical Path

When a build takes longer than 10 seconds and leaves more than 40% of CPU cores idle, Igel automatically prints the exact chain of dependent crates that restricted parallelism:

```text
note: build bottleneck: unicode-ident → proc-macro2 → syn → tracing → hyper → app
```

### Visual Timelines and Chrome Traces

Generate interactive performance profiles:

```bash
# Generate build timeline and Chrome trace
igl build --timings
```

This generates two detailed diagnostic artifacts:
1. **Interactive HTML Timeline** (`.igel/timings/igel-timing.html`): Displays an interactive Gantt chart of every crate's compile duration, highlighting the critical bottleneck path and concurrency saturation over time.
2. **Perfetto / Chrome Trace** (`.igel/fingerprints/trace.json`): Open in [Perfetto UI](https://ui.perfetto.dev) or Chrome (`chrome://tracing`) for thread-level execution timelines and system call inspection.

## Advanced Compiler Acceleration: `rustc-runified`

In standard Rust compilation, executables must wait for every upstream library rlib to be fully generated before compilation begins.

With [rustc-runified](../rustc-runified/README.md) installed (`build.compiler = "auto"`), Igel unlocks two major architectural improvements:

### Pipelined Binary Linking

Executables, test runners, and benchmarks compile against dependency metadata immediately — just like libraries. When dependencies finish generating code, the executable only performs a fast final link. 

On Igel's own clean build, compiling the final binary overlaps with code generation of its largest dependencies, shaving overall build time from **55.4 seconds down to 47.1 seconds**.

### Deterministic Parallel Frontends

On stable toolchains, setting `build.frontend-threads = "auto"` allows crates starting during idle CPU windows (such as end-of-chain crates) to activate multi-threaded frontends with guaranteed deterministic output matching single-threaded builds.
