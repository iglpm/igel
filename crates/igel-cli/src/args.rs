use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "igl",
    about = "A fast, next-generation build system & package manager for Rust",
    version = env!("IGEL_VERSION"),
    styles = crate::help::styles(),
    disable_help_subcommand = true,
    long_about = "Igel is an alternative build system and package manager for Rust that is \
        significantly faster through content-addressable caching (CAS), \
        parallel scheduling, streaming sparse registry resolution, and rmeta pipelining. \
        It reads ordinary Cargo.toml manifests — or a Igel.toml with the same contents — and accepts Cargo's flags."
)]
pub struct Cli {
    /// Coloring of output
    #[arg(long, global = true, value_parser = ["auto", "always", "never"], default_value = "auto", hide = true)]
    pub color: String,

    /// Override a configuration value (`KEY=VALUE` in TOML, e.g.
    /// `profile.release.lto=true`), or read a configuration file (`PATH.toml`)
    #[arg(
        long = "config",
        global = true,
        value_name = "KEY=VALUE|PATH",
        hide = true
    )]
    pub config_overrides: Vec<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Show version information
    #[command(after_help = crate::help::examples(&[("igl version", "the version and commit"), ("igl version -v", "also the host, toolchain and settings in effect")]))]
    Version {
        /// Also show the commit, host and toolchain
        #[arg(short, long)]
        verbose: bool,
    },
    /// Print a shell completion script
    #[command(after_help = crate::help::examples(&[("igl completions bash > ~/.local/share/bash-completion/completions/igl", "bash"), ("igl completions zsh > ~/.zfunc/_igel", "zsh (with ~/.zfunc in $fpath)"), ("igl completions fish > ~/.config/fish/completions/igl.fish", "fish")]))]
    Completions {
        /// bash, zsh, fish, elvish or powershell
        shell: clap_complete::Shell,
    },
    /// Compile the current package (and its dependencies)
    #[command(visible_alias = "b")]
    #[command(after_help = crate::help::examples(&[("igl build", "debug build of the current package"), ("igl build -r", "optimized (release) build"), ("igl build -p app --bin cli", "one binary of one workspace package"), ("igl build --timings", "show what the build waited on")]))]
    Build(BuildArgs),
    /// Type-check the package without producing binaries
    #[command(visible_alias = "c")]
    Check(BuildArgs),
    /// Build, test and run for other targets: here when this machine can, in a container when it can't
    #[command(
        disable_help_flag = true,
        after_help = crate::help::examples(&[
            ("igl cross build -t aarch64-unknown-linux-gnu", "build for ARM64 Linux"),
            ("igl cross test -t x86_64-pc-windows-gnu", "run the tests under Wine"),
            ("igl build --cross aarch64-unknown-linux-gnu -r", "the same as a flag of `igl build`"),
            ("igl cross targets", "the targets with images, and how each builds here"),
            ("igl cross help", "every cross command"),
        ])
    )]
    Cross(CrossArgs),
    /// Compile and run the package's tests
    #[command(visible_alias = "t")]
    #[command(after_help = crate::help::examples(&[("igl test", "every test of the package"), ("igl test parse", "only tests whose name contains \"parse\""), ("igl test --workspace --retries 2", "the whole workspace, retrying flaky tests"), ("igl test -- --nocapture", "pass arguments to the test binaries")]))]
    Test(TestArgs),
    /// Compile and run a binary or example
    #[command(visible_alias = "r")]
    #[command(after_help = crate::help::examples(&[("igl run", "build and run the package's binary"), ("igl run --bin cli -- --help", "run one binary, with arguments of its own"), ("igl run --example demo -r", "an optimized example")]))]
    Run(RunArgs),
    /// Compile and run the package's benchmarks
    Bench(TestArgs),
    /// Format every local crate's source with rustfmt
    Fmt(FmtArgs),
    /// Lint every local crate with clippy (`igl clippy -- -D warnings`)
    Clippy(ClippyArgs),
    /// Build the package's documentation
    Doc(DocArgs),
    /// Compile a package, passing extra arguments to rustc for its targets
    Rustc(RustcArgs),
    /// Apply the compiler's machine-applicable suggestions to the source
    Fix(FixArgs),
    /// Say which units a build would compile, and what changed in each
    Explain(BuildArgs),
    /// Document a package, passing extra arguments to rustdoc
    Rustdoc(RustdocArgs),
    /// Create a new package in a new directory
    New(NewArgs),
    /// Create a new package in an existing directory
    Init(InitArgs),
    /// Add a dependency to the manifest and resolve lockfiles
    #[command(after_help = crate::help::examples(&[("igl add serde -F derive", "with a feature"), ("igl add tokio@1.40", "a version requirement"), ("igl add --dev insta", "a dev-dependency"), ("igl add mylib --path ../mylib", "a local package")]))]
    Add(AddArgs),
    /// Remove a dependency from the manifest
    #[command(visible_alias = "rm")]
    Remove(RemoveArgs),
    /// Update dependencies in lockfiles to latest compatible versions
    Update(UpdateArgs),
    /// List dependencies with newer releases than the lockfile or the
    /// manifests allow
    Outdated(OutdatedArgs),
    /// Raise dependency requirements in the manifests to newer releases
    Upgrade(UpgradeArgs),
    /// Find dependencies the workspace's packages declare but never use
    Unused(UnusedArgs),
    /// Write a software bill of materials (CycloneDX or SPDX JSON)
    Sbom(SbomArgs),
    /// Check the locked dependencies for security advisories and yanked releases
    #[command(after_help = crate::help::examples(&[("igl audit", "vulnerabilities, unmaintained and unsound crates, yanked releases"), ("igl audit --deny warnings", "also fail on the warnings (CI)"), ("igl audit --ignore RUSTSEC-2023-0071", "accept one advisory"), ("igl audit --format json", "for tools")]))]
    Audit(AuditArgs),
    /// Measure which code the tests run (source-based coverage)
    #[command(after_help = crate::help::examples(&[("igl coverage", "run the tests, summarize coverage per file"), ("igl coverage --html", "browsable report in target/coverage/html"), ("igl coverage --lcov -o lcov.info", "for CI services and editors"), ("igl coverage --fail-under 80", "fail when line coverage is below 80 %")]))]
    Coverage(Box<CoverageArgs>),
    /// Display the resolved dependency tree
    #[command(after_help = crate::help::examples(&[("igl tree", "the whole dependency tree"), ("igl tree --depth 1", "direct dependencies only"), ("igl tree -i syn", "what depends on syn"), ("igl tree -e features -i serde", "why each of serde's features is on")]))]
    Tree(TreeArgs),
    /// Draw the package graph (or a build's units) as Graphviz DOT, Mermaid or JSON
    Graph(GraphArgs),
    /// Show a package's features: which are on, and who turned each on
    #[command(after_help = crate::help::examples(&[("igl features", "the workspace's features, and what each enables"), ("igl features serde", "which of serde's features are on, and who turned each on"), ("igl features tokio --all", "every feature, the ones that are off included")]))]
    Features(FeaturesArgs),
    /// Explain why a package is in the dependency graph
    Why(WhyArgs),
    /// Download every dependency without building
    Fetch(ManifestArgs),
    /// Resolve dependencies and write the lockfile
    GenerateLockfile(ManifestArgs),
    /// Print the path of the workspace's manifest as JSON
    LocateProject(ManifestArgs),
    /// Output workspace and dependency metadata as JSON
    Metadata(MetadataArgs),
    /// Build a package and install its binaries
    Install(InstallArgs),
    /// Remove binaries installed with `igl install` (or `cargo install`)
    Uninstall(UninstallArgs),
    /// Assemble the package into a distributable `.crate` file
    Package(PackageArgs),
    /// Package and upload a crate to the registry
    Publish(PublishArgs),
    /// Save a registry API token
    Login(LoginArgs),
    /// Remove a saved registry API token
    Logout(LoginArgs),
    /// Remove (or with --undo, restore) a published version from the index
    Yank(YankArgs),
    /// Manage a crate's owners on the registry
    Owner(OwnerArgs),
    /// Search the registry for crates
    #[command(after_help = crate::help::examples(&[("igl search http client", "crates matching all the words"), ("igl search serde --limit 50", "more results (scroll with the arrow keys, search with /)")]))]
    Search(SearchArgs),
    /// Show a crate's description, features and dependencies
    Info(InfoArgs),
    /// Vendor every dependency into a local directory
    Vendor(VendorArgs),
    /// Print a fully qualified package specification
    Pkgid(PkgidArgs),
    /// Check that the manifest is well-formed
    VerifyProject(ManifestArgs),
    /// Remove generated target artifacts
    Clean(CleanArgs),
    /// Check the toolchain, machine and project for what slows builds down
    Doctor,
    /// Re-run commands whenever the project's files change
    #[command(after_help = crate::help::examples(&[("igl watch", "check on every change"), ("igl watch -x test", "run the tests on every change"), ("igl watch -x check -x \"test --lib\"", "several commands, one after another")]))]
    Watch(WatchArgs),
    /// List the workspace packages a change can influence (for selective CI)
    Affected(AffectedArgs),
    /// Run a single-file Rust script with `---cargo` frontmatter dependencies
    Script(ScriptArgs),
    /// View and change settings (global or per project)
    #[command(subcommand)]
    Config(ConfigCommands),
    /// Inspect or clean the Content-Addressable Store (CAS)
    #[command(subcommand)]
    Cas(CasCommands),
    /// Manage binary and standard lockfiles
    #[command(subcommand)]
    Lockfile(LockfileCommands),
    /// Manage Igel itself (updates and uninstallation)
    #[command(name = "self", subcommand)]
    Self_(SelfCommands),
    /// Update Igel to the latest release (alias for `igl self update`)
    #[command(name = "self-update", hide = true)]
    SelfUpdate(SelfUpdateArgs),
}

/// `igl cross <command> …`: hyprcross' command line (`igl cross help`).
#[derive(Args, Debug)]
pub struct CrossArgs {
    /// build, check, test, run, bench, clippy, doc, clean, targets, doctor, daemon, images, init
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    pub args: Vec<std::ffi::OsString>,
}

/// `--cross` on the build commands. `crate::cross` takes them before these are parsed: they are
/// here for `--help` and completions.
#[derive(Args, Debug, Clone, Default)]
pub struct CrossOpts {
    /// Cross-compile for TRIPLE (else `--target`, else `cross.default-target`): on this machine
    /// when it can build the target, in a container from the target's image when it can't
    #[arg(
        long,
        value_name = "TRIPLE",
        num_args = 0..=1,
        default_missing_value = "",
        help_heading = "Cross Compilation"
    )]
    pub cross: Option<String>,

    /// Where a cross build runs: auto, direct or container (default: `cross.engine`)
    #[arg(long, value_name = "ENGINE", help_heading = "Cross Compilation")]
    pub cross_engine: Option<String>,
}

/// Flags shared by every command that resolves and builds.
#[derive(Args, Debug, Clone, Default)]
pub struct CommonArgs {
    /// Package to operate on (repeatable)
    #[arg(
        short = 'p',
        long = "package",
        value_name = "SPEC",
        help_heading = "Package Selection"
    )]
    pub package: Vec<String>,

    /// Operate on every workspace member
    #[arg(long, alias = "all", help_heading = "Package Selection")]
    pub workspace: bool,

    /// Exclude a package from --workspace (repeatable)
    #[arg(long, value_name = "SPEC", help_heading = "Package Selection")]
    pub exclude: Vec<String>,

    /// Features to activate (comma or space separated, repeatable)
    #[arg(
        short = 'F',
        long,
        value_name = "FEATURES",
        help_heading = "Feature Selection"
    )]
    pub features: Vec<String>,

    /// Activate every feature of the selected packages
    #[arg(long, help_heading = "Feature Selection")]
    pub all_features: bool,

    /// Do not activate the `default` feature
    #[arg(long, help_heading = "Feature Selection")]
    pub no_default_features: bool,

    /// Build with the optimized `release` profile
    #[arg(short = 'r', long, help_heading = "Compilation Options")]
    pub release: bool,

    /// Build with the named profile
    #[arg(
        long,
        value_name = "NAME",
        conflicts_with = "release",
        help_heading = "Compilation Options"
    )]
    pub profile: Option<String>,

    /// Ignore `rust-version` specifications in packages
    #[arg(long, help_heading = "Compilation Options")]
    pub ignore_rust_version: bool,

    /// Build for the given target triple
    #[arg(long, value_name = "TRIPLE", help_heading = "Compilation Options")]
    pub target: Option<String>,

    /// Compiler to use (`rustc` [default], `runified`, or experimental `hyprc`)
    #[arg(long, value_name = "COMPILER", help_heading = "Compilation Options")]
    pub compiler: Option<String>,

    /// Use the experimental hyprc compiler
    #[arg(long = "experimental-hyprc", help_heading = "Compilation Options")]
    pub experimental_hyprc: bool,

    /// Number of parallel jobs (default: number of CPUs)
    #[arg(
        short = 'j',
        long,
        value_name = "N",
        help_heading = "Compilation Options"
    )]
    pub jobs: Option<usize>,

    /// Directory for build output
    #[arg(long, value_name = "DIR", help_heading = "Compilation Options")]
    pub target_dir: Option<PathBuf>,

    /// Show verbose output
    #[arg(short = 'v', long, help_heading = "Output")]
    pub verbose: bool,

    /// Suppress progress and summary output
    #[arg(short = 'q', long, help_heading = "Output")]
    pub quiet: bool,

    /// Path to the manifest (Cargo.toml or Igel.toml), or a directory holding one
    #[arg(long, value_name = "PATH", help_heading = "Manifest")]
    pub manifest_path: Option<PathBuf>,

    /// Fail if the lockfile would need to change
    #[arg(long, help_heading = "Manifest")]
    pub locked: bool,

    /// Use only locally cached registry data; never touch the network
    #[arg(long, help_heading = "Manifest")]
    pub offline: bool,

    /// Equivalent to --locked --offline
    #[arg(long, help_heading = "Manifest")]
    pub frozen: bool,

    /// Print how long each phase of the build takes and the chain of crates
    /// that determined its length; write an HTML timeline to
    /// `.igel/timings/igel-timing.html` and a Chrome trace to
    /// `.igel/fingerprints/trace.json`
    #[arg(long, help_heading = "Compilation Options")]
    pub timings: bool,

    /// Output format of compiler messages: `human` (default) or `json` — one
    /// JSON object per line on stdout, in Cargo's format
    #[arg(
        long,
        value_name = "FMT",
        default_value = "human",
        help_heading = "Output"
    )]
    pub message_format: String,

    /// Recompile everything even if cached (`--rebuild-all=false` to force
    /// using the cache when `build.release-rebuild-all` is set)
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true
    , help_heading = "Compilation Options")]
    pub rebuild_all: Option<bool>,
}

/// Which targets of the selected packages to build.
#[derive(Args, Debug, Clone, Default)]
pub struct TargetArgs {
    /// Only the library
    #[arg(long, help_heading = "Target Selection")]
    pub lib: bool,
    /// All binaries
    #[arg(long, help_heading = "Target Selection")]
    pub bins: bool,
    /// The named binary (repeatable)
    #[arg(long, value_name = "NAME", help_heading = "Target Selection")]
    pub bin: Vec<String>,
    /// All examples
    #[arg(long, help_heading = "Target Selection")]
    pub examples: bool,
    /// The named example (repeatable)
    #[arg(long, value_name = "NAME", help_heading = "Target Selection")]
    pub example: Vec<String>,
    /// All integration tests
    #[arg(long, help_heading = "Target Selection")]
    pub tests: bool,
    /// The named integration test (repeatable)
    #[arg(long, value_name = "NAME", help_heading = "Target Selection")]
    pub test: Vec<String>,
    /// All benchmarks
    #[arg(long, help_heading = "Target Selection")]
    pub benches: bool,
    /// The named benchmark (repeatable)
    #[arg(long, value_name = "NAME", help_heading = "Target Selection")]
    pub bench: Vec<String>,
    /// Every target
    #[arg(long, help_heading = "Target Selection")]
    pub all_targets: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct BuildArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    #[command(flatten)]
    pub targets: TargetArgs,
    #[command(flatten)]
    pub cross: CrossOpts,

    /// Build only the dependencies of the selected packages, not the packages
    /// themselves: the step for a container image layer or CI cache that
    /// should only change when `Cargo.lock` does
    #[arg(long)]
    pub deps_only: bool,
}

#[derive(Args, Debug, Clone)]
pub struct TestArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    #[command(flatten)]
    pub targets: TargetArgs,
    #[command(flatten)]
    pub cross: CrossOpts,

    /// Only run tests whose name contains this string
    #[arg(value_name = "TESTNAME")]
    pub filter: Option<String>,

    /// Test only this package's library documentation
    #[arg(long)]
    pub doc: bool,

    /// Skip documentation tests
    #[arg(long)]
    pub no_doc: bool,

    /// Compile, but don't run tests
    #[arg(long)]
    pub no_run: bool,

    /// Run every test binary even after one fails
    #[arg(long)]
    pub no_fail_fast: bool,

    /// Number of threads each test binary uses (forwarded as `--test-threads`)
    #[arg(long, value_name = "N")]
    pub test_threads: Option<usize>,

    /// Skip test binaries that passed before and whose binary, package files,
    /// arguments and RUST_/CARGO_/IGEL_ environment are unchanged. Only for
    /// tests that depend on nothing else (no network, clock or files outside the
    /// package). Failures are never remembered. `--cached=false` forces a run
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true
    )]
    pub cached: Option<bool>,

    /// Arguments passed to the test binaries (after `--`)
    #[arg(last = true, value_name = "ARGS")]
    pub test_args: Vec<String>,

    /// Run only the tests that failed in the last run (every test when none
    /// did): the test binaries they are in, by exact name
    #[arg(long)]
    pub failed: bool,

    /// Run a failed test binary's failed tests again, up to N times; a test
    /// that then passes is reported as flaky instead of failing the run
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub retries: u32,

    /// Also write the results as JUnit XML (for CI test reports)
    #[arg(long, value_name = "PATH")]
    pub junit: Option<PathBuf>,

    /// `tests`: split test binaries into batches of tests scheduled across
    /// all cores, each batch its own process; `binaries`: one process per
    /// binary (default: `test.schedule`, else `binaries`)
    #[arg(long, value_name = "HOW", value_parser = ["binaries", "tests"])]
    pub schedule: Option<String>,

    /// How many test binaries run at once (default: `test.jobs`, else 1, as
    /// with Cargo: tests in different binaries may share ports, files or a
    /// database; with `--schedule tests`, one per core). Compilation of the
    /// rest continues meanwhile either way
    #[arg(long, value_name = "N")]
    pub test_jobs: Option<usize>,

    /// Stop a test binary (or doc-test run) that runs longer than this
    /// (`90s`, `10m`; default: `test.timeout`, else no limit) and report it
    /// as failed, instead of hanging the run
    #[arg(long, value_name = "DURATION")]
    pub timeout: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    #[command(flatten)]
    pub cross: CrossOpts,

    /// The binary to run
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,

    /// The example to run
    #[arg(long, value_name = "NAME", conflicts_with = "bin")]
    pub example: Option<String>,

    /// Arguments passed to the program (after `--`)
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct FmtArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,

    /// Only format these packages
    #[arg(short = 'p', long = "package")]
    pub package: Vec<String>,

    /// Format every workspace member
    #[arg(long, alias = "all")]
    pub workspace: bool,

    /// Check formatting without writing changes; exits non-zero if any file
    /// would be reformatted
    #[arg(long)]
    pub check: bool,

    /// Arguments for rustfmt (after `--`), e.g. `-- --check`
    #[arg(last = true, value_name = "ARGS")]
    pub rustfmt_args: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct DocArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    #[command(flatten)]
    pub cross: CrossOpts,

    /// Don't build documentation for dependencies
    #[arg(long)]
    pub no_deps: bool,

    /// Open the docs in a browser afterwards
    #[arg(long)]
    pub open: bool,

    /// Document private items too
    #[arg(long)]
    pub document_private_items: bool,

    /// Document only the library (what is documented anyway; accepted for
    /// compatibility with Cargo)
    #[arg(long)]
    pub lib: bool,
}

#[derive(Args, Debug, Clone)]
pub struct RustdocArgs {
    #[command(flatten)]
    pub doc: DocArgs,
    /// Extra arguments for rustdoc on the selected packages (after `--`)
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct RustcArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    /// Extra arguments for rustc on the selected targets (after `--`)
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct FixArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    /// Fix even with uncommitted changes in the working directory
    #[arg(long)]
    pub allow_dirty: bool,
    /// Fix even with staged (but otherwise no uncommitted) changes
    #[arg(long)]
    pub allow_staged: bool,
    /// Fix even when the package is not under version control
    #[arg(long)]
    pub allow_no_vcs: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ClippyArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    /// Apply clippy's suggested fixes to the source
    #[arg(long)]
    pub fix: bool,
    /// Lint flags for the workspace's own crates, e.g. `-- -D warnings`
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct ManifestArgs {
    /// Path to the manifest (Cargo.toml or Igel.toml), or a directory holding one
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,

    /// Use only locally cached registry data
    #[arg(long)]
    pub offline: bool,
}

#[derive(Args, Debug, Clone)]
pub struct NewArgs {
    /// Directory (and package name) to create
    pub path: PathBuf,
    /// Create a library instead of a binary
    #[arg(long)]
    pub lib: bool,
    /// Create a binary (the default)
    #[arg(long, conflicts_with = "lib")]
    pub bin: bool,
    /// Package name (default: the directory name)
    #[arg(long)]
    pub name: Option<String>,
    /// Rust edition
    #[arg(long, default_value = "2024", value_parser = ["2015", "2018", "2021", "2024"])]
    pub edition: String,
    /// Don't create a `.gitignore`
    #[arg(long)]
    pub no_gitignore: bool,
    /// Which file holds the manifest: `cargo` (Cargo.toml, works with plain
    /// `cargo` too) or `igl` (Igel.toml, Igel only)
    #[arg(long, default_value = "cargo", value_parser = ["cargo", "igl"])]
    pub manifest: String,
}

#[derive(Args, Debug, Clone)]
pub struct InitArgs {
    /// Directory to initialize (default: the current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,
    #[arg(long)]
    pub lib: bool,
    #[arg(long, conflicts_with = "lib")]
    pub bin: bool,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, default_value = "2024", value_parser = ["2015", "2018", "2021", "2024"])]
    pub edition: String,
    #[arg(long)]
    pub no_gitignore: bool,
    /// `cargo` (Cargo.toml) or `igl` (Igel.toml)
    #[arg(long, default_value = "cargo", value_parser = ["cargo", "igl"])]
    pub manifest: String,
}

#[derive(Args, Debug, Clone)]
pub struct AddArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,

    /// Crates to add (`serde`, or `serde@1.0` with a version requirement)
    #[arg(required = true, value_name = "DEP")]
    pub crate_specs: Vec<String>,

    /// Explicit version requirement (e.g. `^1.0.210`)
    #[arg(long, short = 'V')]
    pub version: Option<String>,

    /// Comma-separated list of features to enable
    #[arg(long, short = 'F', value_delimiter = ',')]
    pub features: Vec<String>,

    /// Disable default features
    #[arg(long)]
    pub no_default_features: bool,

    /// Add as development dependency (`[dev-dependencies]`)
    #[arg(long, short = 'D')]
    pub dev: bool,

    /// Add as build dependency (`[build-dependencies]`)
    #[arg(long, short = 'B')]
    pub build: bool,

    /// Mark dependency as optional
    #[arg(long)]
    pub optional: bool,

    /// Add a path dependency
    #[arg(long, value_name = "PATH", conflicts_with = "git")]
    pub path: Option<PathBuf>,

    /// Add a git dependency
    #[arg(long, value_name = "URL")]
    pub git: Option<String>,
    /// Branch of the git dependency
    #[arg(long, requires = "git")]
    pub branch: Option<String>,
    /// Tag of the git dependency
    #[arg(long, requires = "git")]
    pub tag: Option<String>,
    /// Commit of the git dependency
    #[arg(long, requires = "git")]
    pub rev: Option<String>,

    /// Name the dependency differently in this crate (`package = "..."`)
    #[arg(long, value_name = "NAME")]
    pub rename: Option<String>,

    /// Add it only for this platform (`cfg(unix)` or a target triple)
    #[arg(long, value_name = "TARGET")]
    pub target: Option<String>,

    /// Registry to take it from (a name from `[registries]`)
    #[arg(long, value_name = "NAME")]
    pub registry: Option<String>,

    /// Show what would be added without changing the manifest
    #[arg(long)]
    pub dry_run: bool,

    /// Package to modify in a workspace
    #[arg(short = 'p', long = "package")]
    pub package: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct RemoveArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Crates to remove
    #[arg(required = true)]
    pub names: Vec<String>,
    /// Only remove from `[dev-dependencies]`
    #[arg(long, short = 'D')]
    pub dev: bool,
    /// Only remove from `[build-dependencies]`
    #[arg(long, short = 'B')]
    pub build: bool,
    /// Package to modify in a workspace
    #[arg(short = 'p', long = "package")]
    pub package: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct UpdateArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Packages to update (`NAME` or `NAME@VERSION`); all when none are given
    pub packages: Vec<String>,
    /// Package to update (the same as naming it)
    #[arg(short = 'p', long = "package", value_name = "SPEC")]
    pub package: Vec<String>,
    /// Update the package to exactly this version
    #[arg(long, value_name = "VERSION")]
    pub precise: Option<String>,
    /// Show what would change without writing the lockfile
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct OutdatedArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Only these workspace members (all of them when none are given)
    #[arg(short = 'p', long = "package", value_name = "SPEC")]
    pub package: Vec<String>,
    /// Exit with status 1 when anything is outdated (for CI)
    #[arg(long)]
    pub exit_code: bool,
    /// `human` (a table) or `json`
    #[arg(long, default_value = "human", value_parser = ["human", "json"])]
    pub format: String,
}

#[derive(Args, Debug, Clone)]
pub struct GraphArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    #[command(flatten)]
    pub targets: TargetArgs,
    /// `dot` (Graphviz), `mermaid` (renders in GitHub/GitLab markdown) or `json`
    #[arg(long, default_value = "dot", value_parser = ["dot", "mermaid", "json"])]
    pub format: String,
    /// The compilation units a build would use, with their keys and whether
    /// the store has them, instead of packages
    #[arg(long)]
    pub units: bool,
    /// Include the workspace packages' development dependencies
    #[arg(long)]
    pub dev: bool,
    /// Only the workspace's own packages and the edges between them
    #[arg(long)]
    pub workspace_only: bool,
    /// Write to this file instead of stdout
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct AuditArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Accept an advisory (repeatable; also `audit.ignore`)
    #[arg(long, value_name = "ID")]
    pub ignore: Vec<String>,
    /// Also fail on these warnings (repeatable): `warnings` (all),
    /// `unmaintained`, `unsound`, `notice`, `yanked`
    #[arg(long, value_name = "KIND", value_parser = ["warnings", "unmaintained", "unsound", "notice", "yanked"])]
    pub deny: Vec<String>,
    /// Advisories from this zip or directory of OSV JSON files
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,
    /// Use the cached advisory database without checking for updates
    #[arg(long)]
    pub no_fetch: bool,
    /// Do not check for yanked releases
    #[arg(long)]
    pub no_yanked: bool,
    /// `human` or `json`
    #[arg(long, default_value = "human", value_parser = ["human", "json"])]
    pub format: String,
    /// Every path to each package, and the advisories' details
    #[arg(short, long)]
    pub verbose: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CoverageArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    #[command(flatten)]
    pub targets: TargetArgs,
    /// Only run tests whose names contain this
    #[arg(value_name = "TESTNAME")]
    pub filter: Option<String>,
    /// Write an HTML report (to `<target>/coverage/html`, or `--output-dir`)
    #[arg(long)]
    pub html: bool,
    /// Write an LCOV report (to `--output-path`, or `<target>/coverage/lcov.info`)
    #[arg(long)]
    pub lcov: bool,
    /// Write llvm-cov's JSON export
    #[arg(long)]
    pub json: bool,
    /// Write a Cobertura XML report
    #[arg(long)]
    pub cobertura: bool,
    /// Where to write the LCOV/JSON/Cobertura report
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output_path: Option<PathBuf>,
    /// Where to write the HTML report
    #[arg(long, value_name = "DIR")]
    pub output_dir: Option<PathBuf>,
    /// Show every line of the files that are not fully covered
    #[arg(long)]
    pub show_missing_lines: bool,
    /// Fail when line coverage is below this percentage
    #[arg(long, value_name = "PERCENT")]
    pub fail_under: Option<f64>,
    /// Leave files whose path matches this regular expression out of the report
    #[arg(long, value_name = "REGEX")]
    pub ignore_filename_regex: Option<String>,
    /// Build and run the tests, but do not report (the profiles stay for a later report)
    #[arg(long)]
    pub no_report: bool,
    /// Keep going when a test fails, and report what ran
    #[arg(long)]
    pub no_fail_fast: bool,
    /// How many test binaries run at once (default: `test.jobs`, else 1)
    #[arg(long, value_name = "N")]
    pub test_jobs: Option<usize>,
    /// Stop a test binary that runs longer than this (`10m`; default:
    /// `test.timeout`, else no limit)
    #[arg(long, value_name = "DURATION")]
    pub timeout: Option<String>,
    /// Arguments after `--` go to the test binaries
    #[arg(last = true, value_name = "ARGS")]
    pub test_args: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct SbomArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Only these workspace members (all of them when none are given)
    #[arg(short = 'p', long = "package", value_name = "SPEC")]
    pub package: Vec<String>,
    /// `cyclonedx` (1.5) or `spdx` (2.3), both JSON
    #[arg(long, default_value = "cyclonedx", value_parser = ["cyclonedx", "spdx"])]
    pub format: String,
    /// Write to this file instead of stdout
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,
    /// Include the members' development dependencies (not shipped)
    #[arg(long)]
    pub dev: bool,
}

#[derive(Args, Debug, Clone)]
pub struct UnusedArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// Remove the unused dependencies from the manifests
    #[arg(long)]
    pub fix: bool,
}

#[derive(Args, Debug, Clone)]
pub struct UpgradeArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Dependencies to upgrade (every registry dependency when none are given)
    pub dependencies: Vec<String>,
    /// Only the manifests of these workspace members
    #[arg(short = 'p', long = "package", value_name = "SPEC")]
    pub package: Vec<String>,
    /// Also move to new major versions (0.x: new minor versions), which may
    /// need code changes
    #[arg(long)]
    pub incompatible: bool,
    /// Also rewrite `=` requirements
    #[arg(long)]
    pub pinned: bool,
    /// Consider releases that need a newer Rust than the package's
    /// `rust-version`
    #[arg(long)]
    pub ignore_rust_version: bool,
    /// Show what would change without writing anything
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct TreeArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,

    /// Packages to show the tree of
    #[arg(short = 'p', long = "package")]
    pub package: Vec<String>,

    /// Show every workspace member
    #[arg(long, alias = "all")]
    pub workspace: bool,

    /// Maximum tree depth to display
    #[arg(long)]
    pub depth: Option<usize>,

    /// Show features enabled for each dependency
    #[arg(long, short = 'f', alias = "features")]
    pub show_features: bool,

    /// Show only packages present in more than one version
    #[arg(long, short = 'd', short_alias = 'D')]
    pub duplicates: bool,

    /// Invert the tree: show what depends on this package (with `-e features`:
    /// which package turns on each of its features)
    #[arg(long, short = 'i', value_name = "SPEC")]
    pub invert: Option<String>,

    /// Dependency kinds to follow: `normal`, `build`, `dev`, `all` (the
    /// default), `no-normal`, `no-build`, `no-dev`, and `features` to show
    /// features as nodes (comma-separated or repeated), as in Cargo
    #[arg(long, short = 'e', value_name = "KINDS")]
    pub edges: Vec<String>,

    /// How lines start: `indent` (tree lines), `depth` (the depth as a
    /// number) or `none`
    #[arg(long, default_value = "indent", value_parser = ["indent", "depth", "none"])]
    pub prefix: String,

    /// Only dependencies that apply to this target triple (default: this
    /// machine's), or `all`
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,

    /// Also show dev-dependencies (the default now, as in Cargo)
    #[arg(long, hide = true)]
    pub dev: bool,

    /// Instead of the tree, list every crate that runs code on this machine
    /// while building (build scripts, proc-macros) or links native code
    #[arg(long)]
    pub exec_surface: bool,

    /// With --exec-surface: `human` or `json`
    #[arg(long, default_value = "human", value_parser = ["human", "json"])]
    pub format: String,
}

impl CommonArgs {
    pub fn defaults_for(m: &ManifestArgs) -> CommonArgs {
        use clap::{FromArgMatches, Parser};
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            common: CommonArgs,
        }
        let mut argv = vec!["igl".to_string()];
        if let Some(p) = &m.manifest_path {
            argv.push("--manifest-path".into());
            argv.push(p.display().to_string());
        }
        let matches = <Wrapper as clap::CommandFactory>::command().get_matches_from(argv);
        let mut common = Wrapper::from_arg_matches(&matches)
            .expect("the defaults parse")
            .common;
        common.offline = m.offline;
        common
    }
}

#[derive(Args, Debug, Clone)]
pub struct FeaturesArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// A dependency (`name`) to explain; the workspace's packages without
    pub package: Option<String>,
    /// Also list the features that are off
    #[arg(long, short = 'a')]
    pub all: bool,
    /// The platform the build is for (default: this machine)
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct WhyArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Package name to trace
    pub package: String,
}

#[derive(Args, Debug, Clone)]
pub struct MetadataArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Output format version (only 1 is supported)
    #[arg(long, default_value = "1")]
    pub format_version: u32,
    /// Only workspace members, no dependency information
    #[arg(long)]
    pub no_deps: bool,
}

#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// Crates to install from the registry (`name` or `name@version`); with
    /// `--git`, the package to install from the repository
    #[arg(value_name = "CRATE")]
    pub crates: Vec<String>,
    /// Version requirement to install (a bare version means exactly it)
    #[arg(long, value_name = "VERSION", alias = "vers")]
    pub version: Option<String>,
    /// Install the package at this path
    #[arg(long, value_name = "PATH", conflicts_with = "git")]
    pub path: Option<PathBuf>,
    /// Install from this git repository
    #[arg(long, value_name = "URL")]
    pub git: Option<String>,
    /// Branch to use with `--git`
    #[arg(long, requires = "git")]
    pub branch: Option<String>,
    /// Tag to use with `--git`
    #[arg(long, requires = "git")]
    pub tag: Option<String>,
    /// Commit to use with `--git`
    #[arg(long, requires = "git")]
    pub rev: Option<String>,
    /// Registry to install from (a name from `[registries]`)
    #[arg(long, value_name = "NAME")]
    pub registry: Option<String>,
    /// Installation root: binaries go into `<DIR>/bin` (default: `$CARGO_INSTALL_ROOT`, else `$CARGO_HOME` or `~/.cargo`)
    #[arg(long, value_name = "DIR")]
    pub root: Option<PathBuf>,
    /// Install only this binary
    #[arg(long, value_name = "NAME")]
    pub bin: Vec<String>,
    /// Reinstall even if the same version is installed, overwriting binaries
    #[arg(short = 'f', long)]
    pub force: bool,
    /// List installed packages and their binaries
    #[arg(long)]
    pub list: bool,
}

#[derive(Args, Debug, Clone)]
pub struct UninstallArgs {
    /// Packages to uninstall
    #[arg(value_name = "PACKAGE")]
    pub packages: Vec<String>,
    /// Installation root the packages were installed into
    #[arg(long, value_name = "DIR")]
    pub root: Option<PathBuf>,
    /// Only uninstall this binary
    #[arg(long, value_name = "NAME")]
    pub bin: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct CleanArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Only clean release artifacts
    #[arg(short = 'r', long)]
    pub release: bool,
    /// Only clean this profile's artifacts
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// Directory to clean instead of the default target directory
    #[arg(long, value_name = "DIR")]
    pub target_dir: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommands {
    /// List every setting with its effective value and where it came from
    List {
        /// Only show settings from the global file
        #[arg(long, conflicts_with = "project")]
        global: bool,
        /// Only show settings from this project's Igel.toml
        #[arg(long)]
        project: bool,
        /// Show which file or variable each value comes from
        #[arg(long)]
        show_origin: bool,
    },
    /// Print one setting's effective value
    Get {
        key: String,
        /// Read from the global file only
        #[arg(long, conflicts_with = "project")]
        global: bool,
        /// Read from this project's Igel.toml only
        #[arg(long)]
        project: bool,
    },
    /// Set a value (in this project's Igel.toml unless --global)
    Set {
        key: String,
        value: String,
        /// Write to `~/.igel/config.toml`, applying to every project
        #[arg(long, conflicts_with = "project")]
        global: bool,
        /// Write to this project's Igel.toml (the default inside a project)
        #[arg(long)]
        project: bool,
    },
    /// Remove a value so it falls back to the next layer
    Unset {
        key: String,
        #[arg(long, conflicts_with = "project")]
        global: bool,
        #[arg(long)]
        project: bool,
    },
    /// Print the paths of the settings files
    Path,
}

#[derive(Subcommand, Debug)]
pub enum CasCommands {
    /// Show CAS store statistics and cache hit rates
    Stats,
    /// Garbage collect the CAS store by size or age
    Gc(CasGcArgs),
    /// Keep what this workspace's builds need through every cleanup
    Pin(Box<CasPinArgs>),
    /// Stop keeping this workspace's entries
    Unpin,
    /// List pinned workspaces
    Pins,
    /// Show which packages take the space, and how much has gone unused
    Du(CasDuArgs),
    /// Write the store entries a build of the workspace needs into one file
    /// (a CI cache step saves it; `igl cas import` restores it)
    Export(Box<CasExportArgs>),
    /// Add the entries of a file written by `igl cas export` to the store
    Import {
        /// The file (`.tar` or `.tar.gz`)
        path: PathBuf,
    },
}

#[derive(Args, Debug, Clone)]
pub struct CasExportArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    /// The file to write: `.tar`, or `.tar.gz` to compress it
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: PathBuf,
    /// Which commands' entries to include (repeatable): `build`, `check`,
    /// `test`
    #[arg(long = "for", value_name = "COMMAND", default_value = "build", value_parser = ["build", "check", "test"])]
    pub for_commands: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct CasDuArgs {
    /// How many of the biggest packages to list
    #[arg(long, default_value_t = 15)]
    pub top: usize,
}

#[derive(Args, Debug, Clone)]
pub struct CasGcArgs {
    /// Maximum store size (bytes, or e.g. `10GB`)
    #[arg(long)]
    pub max_size: Option<String>,

    /// Remove entries no build has used for this long (`30d`, `12h`, or
    /// seconds)
    #[arg(long, value_name = "DURATION", value_parser = parse_age)]
    pub max_age: Option<u64>,

    /// Compress (zstd) entries no build has used for this long
    #[arg(long, value_name = "DURATION", value_parser = parse_age)]
    pub compress_idle: Option<u64>,
}

#[derive(Args, Debug, Clone)]
pub struct CasPinArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    /// Which commands' entries to keep (repeatable): `build`, `check`, `test`
    #[arg(long = "for", value_name = "COMMAND", default_values_t = ["build".to_string(), "check".to_string(), "test".to_string()], value_parser = ["build", "check", "test"])]
    pub for_commands: Vec<String>,
}

fn parse_age(s: &str) -> Result<u64, String> {
    igel_common::time::parse_duration(s)
        .ok_or_else(|| format!("`{s}` is not a duration such as `30d`, `12h` or `3600`"))
}

#[derive(Subcommand, Debug)]
pub enum LockfileCommands {
    /// Export lockfile to Cargo.lock or JSON
    Export {
        #[arg(long, default_value = "cargo", value_parser = ["cargo", "toml", "json"])]
        format: String,
        /// Write to this file instead of stdout (`-` for stdout)
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },
    /// Migrate from Cargo.lock to Igel binary lockfile
    Migrate,
}

#[derive(Subcommand, Debug, Clone)]
pub enum SelfCommands {
    /// Update Igel to the latest (or a specified) release
    #[command(after_help = crate::help::examples(&[
        ("igl self update", "update to the latest release"),
        ("igl self update --check", "check for newer releases without installing"),
        ("igl self update --version v2027.0.0", "install a specific release"),
        ("igl self update --force", "reinstall even if already up to date"),
    ]))]
    Update(SelfUpdateArgs),

    /// Uninstall Igel from the system
    #[command(after_help = crate::help::examples(&[
        ("igl self uninstall", "remove the igl binary"),
        ("igl self uninstall --purge", "also remove cache and configuration (~/.igel)"),
    ]))]
    Uninstall(SelfUninstallArgs),
}

#[derive(Args, Debug, Clone, Default)]
pub struct SelfUpdateArgs {
    /// Update to a specific release (e.g. `v2027.1.0`, `2027.1.0` or `2027.1`)
    #[arg(short = 'v', long)]
    pub version: Option<String>,

    /// Check if a newer version is available without installing it
    #[arg(long)]
    pub check: bool,

    /// Force reinstallation even if already on the latest version
    #[arg(short, long)]
    pub force: bool,

    /// Path to the binary to replace (defaults to currently executing binary)
    #[arg(long)]
    pub to: Option<PathBuf>,

    /// GitHub repository to download from (defaults to `runepm/igel` or $IGEL_REPOSITORY)
    #[arg(long)]
    pub repo: Option<String>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct SelfUninstallArgs {
    /// Also remove global cache and configuration (`~/.igel` or $IGEL_HOME)
    #[arg(long)]
    pub purge: bool,

    /// Skip confirmation prompt
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cargo_style_flags_parse() {
        let cli = Cli::try_parse_from([
            "igl",
            "build",
            "--release",
            "-p",
            "a",
            "-p",
            "b",
            "-F",
            "x,y",
            "--bin",
            "tool",
            "-j",
            "4",
            "--manifest-path",
            "x/Cargo.toml",
        ])
        .unwrap();
        let Commands::Build(b) = cli.command else {
            panic!()
        };
        assert!(b.common.release);
        assert_eq!(b.common.package, ["a", "b"]);
        assert_eq!(b.common.jobs, Some(4));
        assert_eq!(b.targets.bin, ["tool"]);
    }

    #[test]
    fn test_args_after_double_dash_go_to_the_binary() {
        let cli =
            Cli::try_parse_from(["igl", "test", "my_filter", "--", "--nocapture", "--ignored"])
                .unwrap();
        let Commands::Test(t) = cli.command else {
            panic!()
        };
        assert_eq!(t.filter.as_deref(), Some("my_filter"));
        assert_eq!(t.test_args, ["--nocapture", "--ignored"]);
    }

    #[test]
    fn rebuild_all_accepts_optional_bool() {
        let on = Cli::try_parse_from(["igl", "build", "--rebuild-all"]).unwrap();
        let off = Cli::try_parse_from(["igl", "build", "--rebuild-all=false"]).unwrap();
        let unset = Cli::try_parse_from(["igl", "build"]).unwrap();
        let get = |c: Cli| match c.command {
            Commands::Build(b) => b.common.rebuild_all,
            _ => unreachable!(),
        };
        assert_eq!(get(on), Some(true));
        assert_eq!(get(off), Some(false));
        assert_eq!(get(unset), None);
    }

    #[test]
    fn short_aliases() {
        for alias in ["b", "c", "t", "r"] {
            assert!(Cli::try_parse_from(["igl", alias]).is_ok(), "{alias}");
        }
    }

    #[test]
    fn self_commands_parse() {
        let cli = Cli::try_parse_from(["igl", "self", "update"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Self_(SelfCommands::Update(_))
        ));

        let cli = Cli::try_parse_from(["igl", "self", "update", "--check"]).unwrap();
        if let Commands::Self_(SelfCommands::Update(args)) = cli.command {
            assert!(args.check);
        } else {
            panic!();
        }

        let cli =
            Cli::try_parse_from(["igl", "self", "update", "-v", "v0.2.0", "--force"]).unwrap();
        if let Commands::Self_(SelfCommands::Update(args)) = cli.command {
            assert_eq!(args.version.as_deref(), Some("v0.2.0"));
            assert!(args.force);
        } else {
            panic!();
        }

        let cli = Cli::try_parse_from(["igl", "self-update", "--check"]).unwrap();
        if let Commands::SelfUpdate(args) = cli.command {
            assert!(args.check);
        } else {
            panic!();
        }

        let cli = Cli::try_parse_from(["igl", "self", "uninstall", "--purge", "-y"]).unwrap();
        if let Commands::Self_(SelfCommands::Uninstall(args)) = cli.command {
            assert!(args.purge);
            assert!(args.yes);
        } else {
            panic!();
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct WatchArgs {
    /// A `igl` command to run on every change, e.g. `-x check -x "test --lib"`
    /// (repeatable; they run in order and stop at the first failure).
    /// Default: `check`
    #[arg(short = 'x', long = "exec", value_name = "COMMAND")]
    pub exec: Vec<String>,

    /// Clear the screen before each run
    #[arg(short, long)]
    pub clear: bool,

    /// A file or directory (relative to the project root) whose changes do not
    /// trigger a run, e.g. output written by your tests (repeatable). The build
    /// directory, `.git` and `Cargo.lock` are always ignored
    #[arg(short = 'i', long, value_name = "PATH")]
    pub ignore: Vec<PathBuf>,

    /// How often to look for changes, in milliseconds
    #[arg(long, value_name = "MS", default_value_t = 300)]
    pub interval_ms: u64,

    /// Stop after this many runs (used by tests)
    #[arg(long, hide = true)]
    pub max_runs: Option<u32>,
}

#[derive(Args, Debug, Clone)]
pub struct AffectedArgs {
    /// Compare against this git revision or range (`main`, `origin/main...HEAD`,
    /// `HEAD~3`). Uncommitted and untracked changes count too
    #[arg(long, value_name = "REV", default_value = "HEAD")]
    pub since: String,

    /// Output: `names` (one per line), `args` (`-p a -p b`, for
    /// `igl test $(igl affected --format args)`) or `json`
    #[arg(long, value_name = "FMT", default_value = "names", value_parser = ["names", "args", "json"])]
    pub format: String,

    /// Path to the manifest (Cargo.toml or Igel.toml), or a directory holding one
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,

    /// Only shard N of M (`2/4`): split the affected packages between CI
    /// machines, the same way on every one
    #[arg(long, value_name = "N/M")]
    pub partition: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ScriptArgs {
    /// Build the script with the release profile
    #[arg(long)]
    pub release: bool,

    /// Use only locally cached registry data
    #[arg(long)]
    pub offline: bool,

    /// The script (`hello.rs`)
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Arguments for the script
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    pub args: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct PackageArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Package to assemble (in a workspace)
    #[arg(short = 'p', long = "package")]
    pub package: Vec<String>,
    /// Every workspace member
    #[arg(long)]
    pub workspace: bool,
    /// Print the files that would be packaged, and nothing else
    #[arg(short = 'l', long)]
    pub list: bool,
    /// Don't build the packaged crate to check it
    #[arg(long)]
    pub no_verify: bool,
    /// Allow uncommitted changes in the working directory
    #[arg(long)]
    pub allow_dirty: bool,
}

#[derive(Args, Debug, Clone)]
pub struct PublishArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Package to publish (in a workspace)
    #[arg(short = 'p', long = "package")]
    pub package: Vec<String>,
    /// Do everything except the upload
    #[arg(long, short = 'n')]
    pub dry_run: bool,
    /// Registry to publish to (a name from `[registries]`)
    #[arg(long, value_name = "NAME")]
    pub registry: Option<String>,
    /// API token to use instead of the saved one
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,
    /// Don't build the packaged crate before uploading it
    #[arg(long)]
    pub no_verify: bool,
    /// Allow uncommitted changes in the working directory
    #[arg(long)]
    pub allow_dirty: bool,
}

#[derive(Args, Debug, Clone)]
pub struct LoginArgs {
    /// The token (read from standard input when omitted)
    pub token: Option<String>,
    /// Registry the token is for (a name from `[registries]`)
    #[arg(long, value_name = "NAME")]
    pub registry: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct YankArgs {
    /// `NAME@VERSION`, or `NAME` with `--version`
    #[arg(value_name = "CRATE")]
    pub krate: String,
    /// The version to yank
    #[arg(long, value_name = "VERSION", alias = "vers")]
    pub version: Option<String>,
    /// Undo a yank
    #[arg(long)]
    pub undo: bool,
    #[arg(long, value_name = "NAME")]
    pub registry: Option<String>,
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct OwnerArgs {
    /// The crate (default: the package in the current directory)
    #[arg(value_name = "CRATE")]
    pub krate: Option<String>,
    /// Invite these logins as owners
    #[arg(short = 'a', long, value_name = "LOGIN")]
    pub add: Vec<String>,
    /// Remove these owners
    #[arg(short = 'r', long, value_name = "LOGIN")]
    pub remove: Vec<String>,
    /// List the owners
    #[arg(short = 'l', long)]
    pub list: bool,
    #[arg(long, value_name = "NAME")]
    pub registry: Option<String>,
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct SearchArgs {
    /// What to search for
    #[arg(required = true, value_name = "QUERY")]
    pub query: Vec<String>,
    /// How many results to show (at most 100)
    #[arg(long, default_value = "10")]
    pub limit: u32,
    #[arg(long, value_name = "NAME")]
    pub registry: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct InfoArgs {
    /// `NAME` or `NAME@VERSION`
    #[arg(value_name = "CRATE")]
    pub krate: String,
    #[arg(long, value_name = "NAME")]
    pub registry: Option<String>,
    /// Use only the local index cache
    #[arg(long)]
    pub offline: bool,
}

#[derive(Args, Debug, Clone)]
pub struct VendorArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// Where to put the sources
    #[arg(default_value = "vendor")]
    pub path: PathBuf,
    /// Name directories `name-version` even when only one version is used
    #[arg(long)]
    pub versioned_dirs: bool,
    /// Keep what is already in the directory
    #[arg(long)]
    pub no_delete: bool,
}

#[derive(Args, Debug, Clone)]
pub struct PkgidArgs {
    #[command(flatten)]
    pub manifest: ManifestArgs,
    /// A package of the workspace or of its dependency graph (`name` or `name@version`)
    pub spec: Option<String>,
    #[arg(short = 'p', long = "package")]
    pub package: Option<String>,
}
