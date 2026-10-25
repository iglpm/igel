# CI Pipelines & Distributed Caching

CI pipelines spend an enormous amount of time rebuilding identical dependencies and compiling code that has already passed in other branches.

Igel transforms your CI workflow through hermetic cache bundles, distributed remote stores, and intelligent change detection.

## Recommended CI Workflow

A robust CI pipeline in Igel consists of three straightforward steps:

```bash
# 1. Restore cache bundle (gracefully continues if missing on cold runs)
igl cas import ci.tar.gz || true

# 2. Compile and run test suite with flaky retry and JUnit export
igl build
igl test --retries 2 --junit report.xml

# 3. Export exactly what this run needed into a clean bundle
igl cas export -o ci.tar.gz --for build --for test
```

### Why Cache Bundles Beat `target/` Caching

Conventional CI workflows cache the entire `target/` directory. Over time, that directory accumulates stale object files, obsolete dependency versions, and multiple profiles, ballooning to tens of gigabytes.

In contrast, `igl cas export` bundles only the precise artifacts required by your current commit — including compiled libraries, build script outputs, and diagnostics. On Igel's own 249-package workspace:
- The exported bundle contains 219 entries and compresses to roughly 230 MiB.
- A clean checkout on a fresh runner restores and builds with **0 compiled units**.
- Import operations treat archives as untrusted input, validating all artifact checksums before admission.

## Distributed Remote Caching

Instead of uploading and downloading archive bundles on every job, you can connect your developers and CI machines directly to a shared remote cache store using `cas.remote`.

A remote store can be a network filesystem (such as an NFS share or shared cloud volume) or a **Cairn cache server** (Igel's dedicated high-performance remote cache daemon):

```bash
# Connect to a Cairn cache server
export IGEL_CAS_REMOTE=http://cairn.internal:7070
export IGEL_CAS_REMOTE_TOKEN=your-ci-token
igl build
```

```bash
# Multiple Cairn servers with rendezvous hashing and replication
export IGEL_CAS_REMOTE=http://cache-a:7070,http://cache-b:7070
export IGEL_CAS_REMOTE_REPLICAS=2
igl build
```

```bash
# Developer laptops: Read-only access (never push untested local artifacts)
export IGEL_CAS_REMOTE_WRITE=false
igl build
```

### Performance & Resilience

Whenever a local machine encounters a cache miss, it checks the remote store. If another machine or CI worker has built that unit, Igel streams the compiled artifact into the local store and continues compilation immediately:

- **Sub-Second Restores**: Across 182 units (811 MB of artifacts), a developer with an empty local store restores an entire workspace from a Cairn server in **0.65 seconds** (versus 30+ seconds to build from scratch).
- **Background Publishing**: CI workers upload completed artifacts asynchronously in the background, adding virtually zero overhead (~1%) to build times.
- **Circuit Breaker Fault Tolerance**: If a remote cache server becomes unreachable, Igel logs a single warning and falls back to local compilation without failing your build.

### Security and Trust Boundaries

Because cache keys identify compilation inputs, artifacts are binary-compatible only within trusted environments. Maintain clear security boundaries:
- Give CI runners read-write access to your remote cache.
- Give developer workstations read-only access (`cas.remote-write = false`).
- Share caches only among machines within the same trust boundary.

## Selective CI: `igl affected`

In large workspaces, building and testing every package on every pull request is wasteful when a commit touches only a single component.

`igl affected --since <git-ref>` analyzes your Git diff and identifies the exact set of packages impacted by the changes — including the modified packages and all downstream packages that depend on them:

```bash
# Run tests only on affected packages since origin/main
igl test $(igl affected --since origin/main --format args)
```

If a change modifies global configurations (such as `Cargo.lock`, the root `Cargo.toml`, `rust-toolchain.toml`, or `.cargo/config.toml`), `igl affected` automatically designates the entire workspace as affected.

### Parallel CI Matrix Sharding

Distribute affected packages evenly across multiple CI runner machines using `--partition`:

```bash
# Runner 1 of 4
igl test $(igl affected --since origin/main --partition 1/4 --format args)

# Runner 2 of 4
igl test $(igl affected --since origin/main --partition 2/4 --format args)
```

## Continuous Watch Mode: `igl watch`

Igel includes an integrated file watcher that detects modifications and automatically triggers commands:

```bash
# Watch and re-run check and library tests on file changes
igl watch -x check -x "test --lib"
```

The watcher automatically ignores files listed in `.gitignore` (such as logs, coverage outputs, and temporary test fixtures), preventing infinite restart loops.
