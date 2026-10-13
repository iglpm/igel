# Performance: using the whole machine

Rune runs about 1.3 compiler processes per core (`-j`, `build.jobs`) and hands
every `rustc` and build script the same GNU jobserver, so a compiler that is
alone at the end of a build can take the idle slots for its codegen threads
while the total never exceeds the budget. A crate starts the moment the
`.rmeta` of each dependency exists; a build script is its own step that starts
once its build-dependencies are built, overlapping the rest of its package's
dependencies. Before starting another compiler Rune checks available memory
(Linux `MemAvailable`, macOS memory pressure) and, while it is short, lets
running units finish first — a machine with many cores and little memory
neither swaps nor loses a `rustc` to the OOM killer. Each compiler's peak
memory is measured (`wait4`) and remembered, so a crate known to need 2 GiB
waits until 2 GiB are free instead of starting next to three others like it.
Starting a job never waits on the jobserver pipe either: rustc's codegen
threads borrow idle tokens, and a job that starts while they hold them all
simply owes one, repaid as they return — the critical path never waits behind
another crate's code generation. `build.low-priority = true` runs every
compiler at a lower scheduling priority so the rest of the machine stays
responsive (`rune watch` always does).

What stable Rust cannot parallelize is a chain of crates that must be compiled
one after another, and a single crate's front end. A build longer than 10 s that
kept fewer than 60 % of the cores busy ends with one line naming that chain;
`rune build --timings` (or `RUNE_TIMINGS=1`) prints it in full, with how long
each phase of the command took, and writes
`.rune/timings/rune-timing.html` — a timeline of every step with the chain
highlighted and how many ran at once — plus a Chrome trace in
`.rune/fingerprints/trace.json` (<https://ui.perfetto.dev>). Splitting the
largest crate of that chain is usually what helps. On a nightly compiler,
`build.frontend-threads = "auto"` gives crates that start while cores are idle
a parallel front end.

With [rustc-runified](../rustc-runified/README.md) installed for the toolchain
in use (`build.compiler = "auto"`), executables, tests, benches and examples
stop being the end of every chain: like a library, each compiles as soon as
its direct dependencies' metadata exists, then waits — without a job slot —
until every rlib it links is built, and links. The link is its own step in the
timeline. On Rune's own build from scratch the executable's 8 s of
compilation now overlap the code generation of hyprlnk, and the build takes
47 s instead of 55 s. rustc-runified also accepts `build.frontend-threads` on
stable toolchains, with the same output whatever the thread count.
