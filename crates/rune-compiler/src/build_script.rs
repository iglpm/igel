use rune_common::{BuildScriptOutput, LinkLib, LinkSearch};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tracing::{debug, warn};

#[derive(thiserror::Error, Debug)]
pub enum BuildScriptError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Build script compilation failed with exit code: {0}")]
    CompilationFailed(i32),
    #[error("Build script execution failed with exit code: {0}")]
    ExecutionFailed(i32),
}

pub type Result<T> = std::result::Result<T, BuildScriptError>;

pub struct BuildScriptRunner;

/// Environment for build script execution.
#[derive(Debug, Clone)]
pub struct BuildScriptEnv {
    pub manifest_dir: PathBuf,
    pub out_dir: PathBuf,
    pub target: String,
    pub host: String,
    pub opt_level: String,
    pub debug: String,
    pub profile: String,
    pub num_jobs: u32,
    pub rustc: PathBuf,
    pub pkg_name: String,
    pub pkg_version: String,
    pub features: Vec<String>,
    pub cfgs: Vec<(String, Option<String>)>, // CARGO_CFG_* vars
    pub dep_metadata: Vec<(String, String, String)>, // DEP_<LINKS>_<KEY>=<VALUE>
    /// The crate's own `links` manifest field, if set. Cargo exposes this to
    /// the build script as `CARGO_MANIFEST_LINKS`; crates like `ring` assert
    /// it matches an expected value and unconditionally `.unwrap()` it.
    pub links: Option<String>,
}

impl BuildScriptRunner {
    /// Compile a build.rs into a binary.
    pub fn compile(
        rustc_path: &Path,
        build_rs_path: &Path,
        out_dir: &Path,
        deps_dir: &Path,
        edition: &str,
        extern_deps: &[(String, PathBuf)],
        env: Option<&BuildScriptEnv>,
    ) -> Result<PathBuf> {
        Self::compile_with(
            rustc_path,
            build_rs_path,
            out_dir,
            deps_dir,
            edition,
            extern_deps,
            env,
            &|_| {},
        )
    }

    /// Like [`Self::compile`], but lets the caller adjust the `rustc` command
    /// before it runs (e.g. to hand it a jobserver).
    #[allow(clippy::too_many_arguments)]
    pub fn compile_with(
        rustc_path: &Path,
        build_rs_path: &Path,
        out_dir: &Path,
        deps_dir: &Path,
        edition: &str,
        extern_deps: &[(String, PathBuf)],
        env: Option<&BuildScriptEnv>,
        configure: &dyn Fn(&mut Command),
    ) -> Result<PathBuf> {
        let mut cmd = Command::new(rustc_path);
        cmd.arg(build_rs_path);
        cmd.arg("--crate-name").arg("build_script_build");
        cmd.arg("--crate-type").arg("bin");
        cmd.arg("--edition").arg(edition);
        cmd.arg("--out-dir").arg(out_dir);

        // Add dependency search path
        cmd.arg("-L")
            .arg(format!("dependency={}", deps_dir.display()));

        // Add explicit extern dependencies
        for (crate_name, path) in extern_deps {
            cmd.arg("--extern")
                .arg(format!("{crate_name}={}", path.display()));
        }

        // `build.rs` is part of the same package as the crate it builds, and
        // is compiled with that package's own active feature set — plenty of
        // real-world build scripts branch on `cfg!(feature = "...")`
        // directly (e.g. `aws-lc-rs`'s picks its backend crate this way)
        // rather than only reading the `CARGO_FEATURE_*` env vars set at
        // *run* time. Only setting those env vars and never passing the
        // matching `--cfg feature="..."` at *compile* time left every such
        // check permanently false regardless of which features were
        // actually active, since `cfg!()` is resolved when `build.rs` itself
        // is compiled, not when it runs.
        if let Some(e) = env {
            for feature in &e.features {
                cmd.arg("--cfg").arg(format!("feature=\"{feature}\""));
            }
        }

        if let Some(e) = env {
            cmd.env("CARGO_PKG_NAME", &e.pkg_name);
            cmd.env("CARGO_PKG_VERSION", &e.pkg_version);
            cmd.env("CARGO_MANIFEST_DIR", &e.manifest_dir);
            let parts: Vec<&str> = e.pkg_version.split('.').collect();
            let major = parts.first().copied().unwrap_or("0");
            let minor = parts.get(1).copied().unwrap_or("0");
            let patch_full = parts.get(2).copied().unwrap_or("0");
            let patch = patch_full.split('-').next().unwrap_or(patch_full);
            let pre = if let Some((_, pre_str)) = patch_full.split_once('-') {
                pre_str
            } else {
                ""
            };
            cmd.env("CARGO_PKG_VERSION_MAJOR", major);
            cmd.env("CARGO_PKG_VERSION_MINOR", minor);
            cmd.env("CARGO_PKG_VERSION_PATCH", patch);
            cmd.env("CARGO_PKG_VERSION_PRE", pre);
        }
        cmd.env("CARGO_PKG_AUTHORS", "");
        cmd.env("CARGO_PKG_DESCRIPTION", "");
        cmd.env("CARGO_PKG_HOMEPAGE", "");
        cmd.env("CARGO_PKG_REPOSITORY", "");
        cmd.env("CARGO_PKG_LICENSE", "");
        cmd.env("CARGO_PKG_LICENSE_FILE", "");
        cmd.env("CARGO_PKG_RUST_VERSION", "");
        cmd.env("CARGO_PKG_README", "");

        configure(&mut cmd);
        debug!("Compiling build script: {:?}", cmd);
        let status = cmd.status()?;

        if !status.success() {
            return Err(BuildScriptError::CompilationFailed(
                status.code().unwrap_or(1),
            ));
        }

        let binary = out_dir.join(if cfg!(windows) {
            "build_script_build.exe"
        } else {
            "build_script_build"
        });

        Ok(binary)
    }

    /// Execute a compiled build script and parse its stdout.
    pub fn execute(binary: &Path, env: &BuildScriptEnv) -> Result<BuildScriptOutput> {
        Self::execute_with(binary, env, &|_| {})
    }

    /// Like [`Self::execute`], but lets the caller adjust the command before it
    /// runs — notably to pass a jobserver so C compilers driven by the script
    /// (via the `cc` crate) share the build's parallelism budget.
    pub fn execute_with(
        binary: &Path,
        env: &BuildScriptEnv,
        configure: &dyn Fn(&mut Command),
    ) -> Result<BuildScriptOutput> {
        let mut cmd = Command::new(binary);
        configure(&mut cmd);

        // Cargo always runs a build script with its crate's manifest
        // directory as the current directory, and build scripts routinely
        // rely on that to resolve relative paths (e.g. openssl-sys's
        // `build/expando.c`). Without this, such scripts fail with
        // "No such file or directory" against whatever directory `rune`
        // itself happened to be invoked from.
        cmd.current_dir(&env.manifest_dir);

        // Standard cargo environment variables
        cmd.env("CARGO_MANIFEST_DIR", &env.manifest_dir);
        cmd.env("OUT_DIR", &env.out_dir);
        cmd.env("TARGET", &env.target);
        cmd.env("HOST", &env.host);
        cmd.env("OPT_LEVEL", &env.opt_level);
        cmd.env("DEBUG", &env.debug);
        cmd.env("PROFILE", &env.profile);
        cmd.env("NUM_JOBS", env.num_jobs.to_string());
        cmd.env("RUSTC", &env.rustc);
        cmd.env("CARGO_PKG_NAME", &env.pkg_name);
        cmd.env("CARGO_PKG_VERSION", &env.pkg_version);
        if let Some(links) = &env.links {
            cmd.env("CARGO_MANIFEST_LINKS", links);
        }

        let parts: Vec<&str> = env.pkg_version.split('.').collect();
        let major = parts.first().copied().unwrap_or("0");
        let minor = parts.get(1).copied().unwrap_or("0");
        let patch_full = parts.get(2).copied().unwrap_or("0");
        let patch = patch_full.split('-').next().unwrap_or(patch_full);
        let pre = if let Some((_, pre_str)) = patch_full.split_once('-') {
            pre_str
        } else {
            ""
        };
        cmd.env("CARGO_PKG_VERSION_MAJOR", major);
        cmd.env("CARGO_PKG_VERSION_MINOR", minor);
        cmd.env("CARGO_PKG_VERSION_PATCH", patch);
        cmd.env("CARGO_PKG_VERSION_PRE", pre);
        cmd.env("CARGO_PKG_AUTHORS", "");
        cmd.env("CARGO_PKG_DESCRIPTION", "");
        cmd.env("CARGO_PKG_HOMEPAGE", "");
        cmd.env("CARGO_PKG_REPOSITORY", "");
        cmd.env("CARGO_PKG_LICENSE", "");
        cmd.env("CARGO_PKG_LICENSE_FILE", "");
        cmd.env("CARGO_PKG_RUST_VERSION", "");
        cmd.env("CARGO_PKG_README", "");

        // Set standard target configuration environment variables
        let arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else if cfg!(target_arch = "arm") {
            "arm"
        } else if cfg!(target_arch = "x86") {
            "x86"
        } else if cfg!(target_arch = "riscv64") {
            "riscv64"
        } else if cfg!(target_arch = "wasm32") {
            "wasm32"
        } else {
            "x86_64"
        };
        cmd.env("CARGO_CFG_TARGET_ARCH", arch);

        let os = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "freebsd") {
            "freebsd"
        } else {
            std::env::consts::OS
        };
        cmd.env("CARGO_CFG_TARGET_OS", os);

        let family = if cfg!(target_family = "unix") {
            "unix"
        } else if cfg!(target_family = "windows") {
            "windows"
        } else if cfg!(target_family = "wasm") {
            "wasm"
        } else {
            ""
        };
        if !family.is_empty() {
            cmd.env("CARGO_CFG_TARGET_FAMILY", family);
            if family == "unix" {
                cmd.env("CARGO_CFG_UNIX", "");
            } else if family == "windows" {
                cmd.env("CARGO_CFG_WINDOWS", "");
            }
        }

        let env_target = if cfg!(target_env = "gnu") {
            "gnu"
        } else if cfg!(target_env = "msvc") {
            "msvc"
        } else if cfg!(target_env = "musl") {
            "musl"
        } else {
            ""
        };
        cmd.env("CARGO_CFG_TARGET_ENV", env_target);

        // Cargo always sets this too (e.g. "unknown" for
        // x86_64-unknown-linux-gnu, "apple" for macOS, "pc" for Windows).
        // Missing entirely — rather than merely being wrong for an
        // unanticipated target — made any build script that reads it
        // unconditionally (as `libm`'s does) panic outright on `NotPresent`
        // instead of just risking a wrong cfg decision.
        let vendor = if cfg!(target_vendor = "apple") {
            "apple"
        } else if cfg!(target_vendor = "pc") {
            "pc"
        } else {
            "unknown"
        };
        cmd.env("CARGO_CFG_TARGET_VENDOR", vendor);

        let ptr_width = if cfg!(target_pointer_width = "64") {
            "64"
        } else if cfg!(target_pointer_width = "32") {
            "32"
        } else {
            "64"
        };
        cmd.env("CARGO_CFG_TARGET_POINTER_WIDTH", ptr_width);

        let endian = if cfg!(target_endian = "little") {
            "little"
        } else {
            "big"
        };
        cmd.env("CARGO_CFG_TARGET_ENDIAN", endian);

        // Cargo always sets this (to a possibly-empty comma list); crates
        // that check it defensively still warn on it being entirely absent.
        cmd.env(
            "CARGO_CFG_TARGET_FEATURE",
            target_features(&env.target).join(","),
        );

        for feature in &env.features {
            let feature_env = format!("CARGO_FEATURE_{}", feature.to_uppercase().replace('-', "_"));
            cmd.env(feature_env, "1");
        }

        for (key, val_opt) in &env.cfgs {
            let env_key = format!("CARGO_CFG_{}", key.to_uppercase().replace('-', "_"));
            if let Some(val) = val_opt {
                cmd.env(env_key, val);
            } else {
                cmd.env(env_key, "");
            }
        }

        for (links, key, val) in &env.dep_metadata {
            let env_key = format!(
                "DEP_{}_{}",
                links.to_uppercase().replace('-', "_"),
                key.to_uppercase().replace('-', "_")
            );
            cmd.env(env_key, val);
        }

        cmd.stdout(Stdio::piped());

        debug!("Executing build script: {:?}", cmd);
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);

        let mut output = BuildScriptOutput::default();

        for line_res in reader.lines() {
            let line = line_res?;
            if let Some(stripped) = line.strip_prefix("cargo::") {
                Self::parse_instruction(stripped, &mut output);
            } else if let Some(stripped) = line.strip_prefix("cargo:") {
                Self::parse_instruction(stripped, &mut output);
            }
        }

        let status = child.wait()?;
        if !status.success() {
            return Err(BuildScriptError::ExecutionFailed(
                status.code().unwrap_or(1),
            ));
        }

        Ok(output)
    }
}

/// Enabled target features for `triple`, as `rustc --print=cfg` reports
/// them, cached per triple since this only depends on the (fixed, per-build)
/// target and never changes within a single `rune` invocation.
fn target_features(triple: &str) -> Vec<String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    if let Some(cached) = cache.lock().unwrap().get(triple) {
        return cached.clone();
    }

    let features = Command::new("rustc")
        .arg("--print=cfg")
        .arg("--target")
        .arg(triple)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|line| line.strip_prefix("target_feature=\""))
                .filter_map(|rest| rest.strip_suffix('"'))
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    cache
        .lock()
        .unwrap()
        .insert(triple.to_string(), features.clone());
    features
}

impl BuildScriptRunner {
    fn parse_instruction(instruction: &str, output: &mut BuildScriptOutput) {
        if let Some((key, value)) = instruction.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "rustc-link-lib" => {
                    if let Some((kind, name)) = value.split_once('=') {
                        output.link_libs.push(LinkLib {
                            kind: Some(kind.trim().to_string()),
                            name: name.trim().to_string(),
                        });
                    } else {
                        output.link_libs.push(LinkLib {
                            kind: None,
                            name: value.to_string(),
                        });
                    }
                }
                "rustc-link-search" => {
                    if let Some((kind, path)) = value.split_once('=') {
                        output.link_search.push(LinkSearch {
                            kind: Some(kind.trim().to_string()),
                            path: PathBuf::from(path.trim()),
                        });
                    } else {
                        output.link_search.push(LinkSearch {
                            kind: None,
                            path: PathBuf::from(value),
                        });
                    }
                }
                "rustc-cfg" => {
                    output.cfgs.push(value.to_string());
                }
                "rustc-check-cfg" => {
                    output.check_cfgs.push(value.to_string());
                }
                "rustc-env" => {
                    if let Some((env_key, env_val)) = value.split_once('=') {
                        output
                            .env_vars
                            .push((env_key.trim().to_string(), env_val.trim().to_string()));
                    }
                }
                "rerun-if-changed" => {
                    output.rerun_if_changed.push(PathBuf::from(value));
                }
                "rerun-if-env-changed" => {
                    output.rerun_if_env_changed.push(value.to_string());
                }
                "warning" => {
                    output.warnings.push(value.to_string());
                    warn!("Build script warning: {}", value);
                }
                _ => {
                    // Custom metadata
                    output.metadata.push((key.to_string(), value.to_string()));
                }
            }
        } else {
            if instruction.trim() == "warning" {
                // handled by empty warning logic if needed
            }
        }
    }
}
