//! Compiling and running `build.rs`, and understanding what it reports.
//!
//! A build script is a separate program that runs *on the host* before its
//! package compiles. Cargo defines its contract precisely: the environment it
//! sees (`TARGET`, `HOST`, `OUT_DIR`, `CARGO_CFG_*`, `CARGO_FEATURE_*`,
//! `CARGO_PKG_*`, `DEP_*`, ...) and the `cargo::` lines it may print. Crates
//! from `cc`, `bindgen` and `pkg-config` users to `ring` and `openssl-sys`
//! depend on every detail, so this module follows the contract rather than an
//! approximation of it.

use rune_common::{BuildScriptOutput, LinkArg, LinkArgScope, LinkLib, LinkSearch, PackageMeta};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tracing::{debug, warn};

#[derive(thiserror::Error, Debug)]
pub enum BuildScriptError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Build script compilation failed with exit code: {code}{}", fmt_output(.output))]
    CompilationFailed { code: i32, output: String },
    #[error("Build script execution failed with exit code: {code}{}", fmt_stderr(.stderr))]
    ExecutionFailed { code: i32, stderr: String },
    #[error("Build script reported an error:\n{}", .0.join("\n"))]
    ReportedErrors(Vec<String>),
}

fn fmt_output(output: &str) -> String {
    if output.trim().is_empty() {
        String::new()
    } else {
        format!("\n{}", output.trim_end())
    }
}

fn fmt_stderr(stderr: &str) -> String {
    if stderr.trim().is_empty() {
        String::new()
    } else {
        format!("\n--- stderr\n{}", stderr.trim_end())
    }
}

pub type Result<T> = std::result::Result<T, BuildScriptError>;

pub struct BuildScriptRunner;

/// Environment for build script execution.
#[derive(Debug, Clone)]
pub struct BuildScriptEnv {
    pub manifest_dir: PathBuf,
    pub out_dir: PathBuf,
    /// `TARGET`: the triple the *package* is built for.
    pub target: String,
    /// `HOST`: the machine running the build.
    pub host: String,
    pub opt_level: String,
    /// `DEBUG`.
    pub debug: bool,
    pub debug_assertions: bool,
    /// `PROFILE`: `release` or `debug`.
    pub profile: String,
    pub num_jobs: u32,
    pub rustc: PathBuf,
    pub rustdoc: PathBuf,
    pub rustc_wrapper: Option<PathBuf>,
    pub rustc_workspace_wrapper: Option<PathBuf>,
    /// `RUSTC_LINKER`.
    pub linker: Option<PathBuf>,
    pub pkg_name: String,
    pub pkg_version: String,
    /// `CARGO_PKG_AUTHORS`, `_DESCRIPTION`, ...
    pub meta: PackageMeta,
    pub features: Vec<String>,
    /// `CARGO_CFG_*`, computed from the compiler's cfg set for `target`.
    pub target_cfg_env: Vec<(String, String)>,
    /// `(links, key, value)` → `DEP_<LINKS>_<KEY>=<value>`.
    pub dep_metadata: Vec<(String, String, String)>,
    /// The crate's own `links` manifest field, if set. Cargo exposes this to
    /// the build script as `CARGO_MANIFEST_LINKS`; crates like `ring` assert
    /// it matches an expected value and unconditionally `.unwrap()` it.
    pub links: Option<String>,
    /// The effective rustflags (`CARGO_ENCODED_RUSTFLAGS`).
    pub rustflags: Vec<String>,
    /// `[env]` entries from configuration: `(name, value, force)`.
    pub config_env: Vec<(String, String, bool)>,
}

impl BuildScriptEnv {
    /// The variables both compiling and running a script see, mirroring what
    /// Cargo sets on the package's own `rustc` invocation.
    fn apply_package_env(&self, cmd: &mut Command) {
        cmd.env("CARGO_PKG_NAME", &self.pkg_name);
        cmd.env("CARGO_PKG_VERSION", &self.pkg_version);
        cmd.env(
            "CARGO_MANIFEST_DIR",
            rune_common::fsutil::simplified(&self.manifest_dir),
        );
        cmd.env(
            "CARGO_MANIFEST_PATH",
            rune_common::fsutil::simplified(&rune_common::manifest_file::manifest_in(
                &self.manifest_dir,
            )),
        );
        let core = self
            .pkg_version
            .split(['-', '+'])
            .next()
            .unwrap_or(&self.pkg_version);
        let mut parts = core.split('.');
        cmd.env("CARGO_PKG_VERSION_MAJOR", parts.next().unwrap_or("0"));
        cmd.env("CARGO_PKG_VERSION_MINOR", parts.next().unwrap_or("0"));
        cmd.env("CARGO_PKG_VERSION_PATCH", parts.next().unwrap_or("0"));
        cmd.env(
            "CARGO_PKG_VERSION_PRE",
            self.pkg_version
                .split_once('-')
                .map(|(_, pre)| pre.split('+').next().unwrap_or(pre))
                .unwrap_or(""),
        );
        cmd.env("CARGO_PKG_AUTHORS", &self.meta.authors);
        cmd.env("CARGO_PKG_DESCRIPTION", &self.meta.description);
        cmd.env("CARGO_PKG_HOMEPAGE", &self.meta.homepage);
        cmd.env("CARGO_PKG_REPOSITORY", &self.meta.repository);
        cmd.env("CARGO_PKG_LICENSE", &self.meta.license);
        cmd.env("CARGO_PKG_LICENSE_FILE", &self.meta.license_file);
        cmd.env("CARGO_PKG_RUST_VERSION", &self.meta.rust_version);
        cmd.env("CARGO_PKG_README", &self.meta.readme);
        if let Some(links) = &self.links {
            cmd.env("CARGO_MANIFEST_LINKS", links);
        }
        for (k, v, force) in &self.config_env {
            if *force || std::env::var_os(k).is_none() {
                cmd.env(k, v);
            }
        }
    }
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
            Command::new(rustc_path),
            build_rs_path,
            out_dir,
            deps_dir,
            edition,
            extern_deps,
            env,
            &|_| {},
        )
    }

    /// Like [`Self::compile`], but takes the compiler command (so wrappers
    /// configured for the build apply) and lets the caller adjust it before it
    /// runs (e.g. to hand it a jobserver).
    #[allow(clippy::too_many_arguments)]
    pub fn compile_with(
        cmd: Command,
        build_rs_path: &Path,
        out_dir: &Path,
        deps_dir: &Path,
        edition: &str,
        extern_deps: &[(String, PathBuf)],
        env: Option<&BuildScriptEnv>,
        configure: &dyn Fn(&mut Command),
    ) -> Result<PathBuf> {
        Self::compile_reporting(
            cmd,
            build_rs_path,
            out_dir,
            deps_dir,
            edition,
            extern_deps,
            env,
            configure,
        )
        .map(|(bin, _)| bin)
    }

    /// Like [`Self::compile_with`], also returning what the compiler printed
    /// on success (warnings). Its output is captured, not passed through: on
    /// failure it is part of the error, so it is shown once, with the failure,
    /// rather than written into whatever else is on the terminal.
    #[allow(clippy::too_many_arguments)]
    pub fn compile_reporting(
        mut cmd: Command,
        build_rs_path: &Path,
        out_dir: &Path,
        deps_dir: &Path,
        edition: &str,
        extern_deps: &[(String, PathBuf)],
        env: Option<&BuildScriptEnv>,
        configure: &dyn Fn(&mut Command),
    ) -> Result<(PathBuf, String)> {
        let _ = std::fs::create_dir_all(out_dir);
        cmd.arg(rune_common::fsutil::simplified(build_rs_path));
        cmd.arg("--crate-name").arg("build_script_build");
        cmd.arg("--crate-type").arg("bin");
        cmd.arg("--edition").arg(edition);
        cmd.arg("--out-dir")
            .arg(rune_common::fsutil::simplified(out_dir));
        // The dep-info lists the files the script is compiled from, which is
        // what decides whether the compiled script can be reused.
        cmd.arg("--emit=dep-info,link");

        // Add dependency search path
        cmd.arg("-L")
            .arg(format!("dependency={}", deps_dir.display()));

        // Add explicit extern dependencies
        for (crate_name, path) in extern_deps {
            cmd.arg("--extern")
                .arg(format!("{crate_name}={}", path.display()));
        }

        if let Some(e) = env {
            // `build.rs` is part of the same package as the crate it builds,
            // and is compiled with that package's own active feature set —
            // plenty of real-world build scripts branch on
            // `cfg!(feature = "...")` directly (e.g. `aws-lc-rs`'s picks its
            // backend crate this way) rather than only reading the
            // `CARGO_FEATURE_*` env vars set at *run* time.
            for feature in &e.features {
                cmd.arg("--cfg").arg(format!("feature=\"{feature}\""));
            }
            // A build script is a host program: it gets the host's flags
            // and linker, not the device's.
            cmd.args(&e.rustflags);
            if let Some(linker) = &e.linker {
                cmd.arg("-C").arg(format!("linker={}", linker.display()));
            }
            e.apply_package_env(&mut cmd);
        }

        configure(&mut cmd);
        debug!("Compiling build script: {:?}", cmd);
        let out = cmd.output()?;
        let printed = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );

        if !out.status.success() {
            return Err(BuildScriptError::CompilationFailed {
                code: out.status.code().unwrap_or(1),
                output: printed,
            });
        }

        let binary = out_dir.join(if cfg!(windows) {
            "build_script_build.exe"
        } else {
            "build_script_build"
        });

        Ok((binary, printed))
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
        Self::execute_command(Command::new(binary), env, configure)
    }

    /// Like [`Self::execute_with`], for a command that runs the script
    /// (through a sandbox, say) and gets the script's arguments and
    /// environment added.
    pub fn execute_command(
        mut cmd: Command,
        env: &BuildScriptEnv,
        configure: &dyn Fn(&mut Command),
    ) -> Result<BuildScriptOutput> {
        configure(&mut cmd);

        // Cargo always runs a build script with its crate's manifest
        // directory as the current directory, and build scripts routinely
        // rely on that to resolve relative paths (e.g. openssl-sys's
        // `build/expando.c`).
        cmd.current_dir(rune_common::fsutil::simplified(&env.manifest_dir));

        env.apply_package_env(&mut cmd);
        cmd.env("OUT_DIR", rune_common::fsutil::simplified(&env.out_dir));
        cmd.env("TARGET", &env.target);
        cmd.env("HOST", &env.host);
        cmd.env("OPT_LEVEL", &env.opt_level);
        cmd.env("DEBUG", if env.debug { "true" } else { "false" });
        cmd.env("PROFILE", &env.profile);
        cmd.env("NUM_JOBS", env.num_jobs.to_string());
        cmd.env("RUSTC", &env.rustc);
        cmd.env("RUSTDOC", &env.rustdoc);
        match &env.rustc_wrapper {
            Some(w) => cmd.env("RUSTC_WRAPPER", w),
            None => cmd.env_remove("RUSTC_WRAPPER"),
        };
        match &env.rustc_workspace_wrapper {
            Some(w) => cmd.env("RUSTC_WORKSPACE_WRAPPER", w),
            None => cmd.env_remove("RUSTC_WORKSPACE_WRAPPER"),
        };
        if let Some(linker) = &env.linker {
            cmd.env("RUSTC_LINKER", linker);
        }
        // Flags meant for the compiler are handed over encoded so that
        // arguments containing spaces survive; the plain variable would apply
        // them to any nested cargo/rustc invocation the script makes.
        cmd.env("CARGO_ENCODED_RUSTFLAGS", env.rustflags.join("\x1f"));
        cmd.env_remove("RUSTFLAGS");

        // `CARGO_CFG_*` describes the *target*, not the machine Rune runs on.
        for (k, v) in &env.target_cfg_env {
            cmd.env(k, v);
        }
        for feature in &env.features {
            let feature_env = format!("CARGO_FEATURE_{}", feature.to_uppercase().replace('-', "_"));
            cmd.env(feature_env, "1");
        }
        for (links, key, val) in &env.dep_metadata {
            let env_key = format!(
                "DEP_{}_{}",
                links.to_uppercase().replace('-', "_"),
                key.to_uppercase().replace('-', "_")
            );
            cmd.env(env_key, val);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        debug!("Executing build script: {:?}", cmd);
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        // Drain stderr concurrently: a chatty script would otherwise fill the
        // pipe and stall while we're still reading stdout.
        let stderr_reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });

        let mut output = BuildScriptOutput::default();
        let mut raw_stdout = String::new();
        for line_res in BufReader::new(stdout).lines() {
            let line = line_res?;
            raw_stdout.push_str(&line);
            raw_stdout.push('\n');
            if let Some(stripped) = line.strip_prefix("cargo::") {
                Self::parse_instruction(stripped, true, &mut output);
            } else if let Some(stripped) = line.strip_prefix("cargo:") {
                Self::parse_instruction(stripped, false, &mut output);
            }
        }

        let status = child.wait()?;
        let stderr_text = stderr_reader.join().unwrap_or_default();

        // Keep what the script said next to its output directory, like Cargo,
        // so a failed build can be inspected afterwards.
        if let Some(dir) = env.out_dir.parent() {
            let _ = std::fs::write(dir.join("output"), &raw_stdout);
            let _ = std::fs::write(dir.join("stderr"), &stderr_text);
        }

        if !status.success() {
            return Err(BuildScriptError::ExecutionFailed {
                code: status.code().unwrap_or(1),
                stderr: stderr_text,
            });
        }
        if !output.errors.is_empty() {
            return Err(BuildScriptError::ReportedErrors(output.errors));
        }

        Ok(output)
    }

    /// Parses one directive. `modern` is the `cargo::key=value` spelling, in
    /// which unknown keys are errors and metadata must be spelled
    /// `cargo::metadata=key=value`; the legacy `cargo:key=value` treats every
    /// unknown key as metadata.
    pub fn parse_instruction(instruction: &str, modern: bool, output: &mut BuildScriptOutput) {
        let Some((key, value)) = instruction.split_once('=') else {
            return;
        };
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
            "rustc-flags" => Self::parse_rustc_flags(value, output),
            "rustc-cfg" => output.cfgs.push(value.to_string()),
            "rustc-check-cfg" => output.check_cfgs.push(value.to_string()),
            "rustc-env" => {
                if let Some((env_key, env_val)) = value.split_once('=') {
                    output
                        .env_vars
                        .push((env_key.trim().to_string(), env_val.to_string()));
                }
            }
            "rustc-link-arg" => output.link_args.push(LinkArg {
                scope: LinkArgScope::All,
                arg: value.to_string(),
            }),
            "rustc-cdylib-link-arg" | "rustc-link-arg-cdylib" => output.link_args.push(LinkArg {
                scope: LinkArgScope::Cdylib,
                arg: value.to_string(),
            }),
            "rustc-link-arg-bins" => output.link_args.push(LinkArg {
                scope: LinkArgScope::Bins,
                arg: value.to_string(),
            }),
            "rustc-link-arg-bin" => {
                if let Some((name, arg)) = value.split_once('=') {
                    output.link_args.push(LinkArg {
                        scope: LinkArgScope::Bin(name.trim().to_string()),
                        arg: arg.to_string(),
                    });
                }
            }
            "rustc-link-arg-tests" => output.link_args.push(LinkArg {
                scope: LinkArgScope::Tests,
                arg: value.to_string(),
            }),
            "rustc-link-arg-examples" => output.link_args.push(LinkArg {
                scope: LinkArgScope::Examples,
                arg: value.to_string(),
            }),
            "rustc-link-arg-benches" => output.link_args.push(LinkArg {
                scope: LinkArgScope::Benches,
                arg: value.to_string(),
            }),
            "rerun-if-changed" => output.rerun_if_changed.push(PathBuf::from(value)),
            "rerun-if-env-changed" => output.rerun_if_env_changed.push(value.to_string()),
            "warning" => {
                output.warnings.push(value.to_string());
                debug!("build script warning: {}", value);
            }
            "error" if modern => output.errors.push(value.to_string()),
            "metadata" if modern => {
                if let Some((k, v)) = value.split_once('=') {
                    output
                        .metadata
                        .push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            // Legacy spelling: everything else is `cargo:KEY=VALUE` metadata.
            _ if !modern => output.metadata.push((key.to_string(), value.to_string())),
            _ => warn!("Unknown build script directive `cargo::{key}`"),
        }
    }

    /// `cargo::rustc-flags=-l foo -L /path`: only `-l` and `-L` are allowed.
    fn parse_rustc_flags(value: &str, output: &mut BuildScriptOutput) {
        let mut tokens = value.split_whitespace();
        while let Some(tok) = tokens.next() {
            let (flag, rest) = if tok == "-l" || tok == "-L" {
                (tok, tokens.next().unwrap_or(""))
            } else if let Some(r) = tok.strip_prefix("-l") {
                ("-l", r)
            } else if let Some(r) = tok.strip_prefix("-L") {
                ("-L", r)
            } else {
                warn!("Ignoring unsupported `rustc-flags` argument `{tok}`");
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            if flag == "-l" {
                match rest.split_once('=') {
                    Some((kind, name)) => output.link_libs.push(LinkLib {
                        kind: Some(kind.to_string()),
                        name: name.to_string(),
                    }),
                    None => output.link_libs.push(LinkLib {
                        kind: None,
                        name: rest.to_string(),
                    }),
                }
            } else {
                match rest.split_once('=') {
                    Some((kind, path)) => output.link_search.push(LinkSearch {
                        kind: Some(kind.to_string()),
                        path: PathBuf::from(path),
                    }),
                    None => output.link_search.push(LinkSearch {
                        kind: None,
                        path: PathBuf::from(rest),
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(lines: &[&str]) -> BuildScriptOutput {
        let mut out = BuildScriptOutput::default();
        for line in lines {
            if let Some(rest) = line.strip_prefix("cargo::") {
                BuildScriptRunner::parse_instruction(rest, true, &mut out);
            } else if let Some(rest) = line.strip_prefix("cargo:") {
                BuildScriptRunner::parse_instruction(rest, false, &mut out);
            }
        }
        out
    }

    #[test]
    fn link_libraries_search_paths_and_flags() {
        let out = parse(&[
            "cargo::rustc-link-lib=static=foo",
            "cargo::rustc-link-lib=bar",
            "cargo::rustc-link-search=native=/opt/lib",
            "cargo:rustc-link-search=/plain",
            "cargo::rustc-flags=-l baz -L /flags/dir -Lnative=/x -lstatic:+whole-archive=q",
        ]);
        assert_eq!(out.link_libs.len(), 4);
        assert_eq!(out.link_libs[0].kind.as_deref(), Some("static"));
        assert_eq!(out.link_libs[1].kind, None);
        assert_eq!(out.link_libs[2].name, "baz");
        assert_eq!(
            out.link_libs[3].kind.as_deref(),
            Some("static:+whole-archive")
        );
        assert_eq!(out.link_search.len(), 4);
        assert_eq!(out.link_search[3].kind.as_deref(), Some("native"));
        assert_eq!(out.link_search[2].path, PathBuf::from("/flags/dir"));
    }

    #[test]
    fn link_args_are_scoped() {
        let out = parse(&[
            "cargo::rustc-link-arg=-all",
            "cargo::rustc-cdylib-link-arg=-cd",
            "cargo::rustc-link-arg-bins=-bins",
            "cargo::rustc-link-arg-bin=tool=-only-tool",
            "cargo::rustc-link-arg-tests=-t",
            "cargo::rustc-link-arg-examples=-e",
            "cargo::rustc-link-arg-benches=-b",
        ]);
        let scopes: Vec<_> = out.link_args.iter().map(|a| a.scope.clone()).collect();
        assert_eq!(
            scopes,
            [
                LinkArgScope::All,
                LinkArgScope::Cdylib,
                LinkArgScope::Bins,
                LinkArgScope::Bin("tool".into()),
                LinkArgScope::Tests,
                LinkArgScope::Examples,
                LinkArgScope::Benches,
            ]
        );
        assert_eq!(out.link_args[3].arg, "-only-tool");
    }

    #[test]
    fn metadata_spellings() {
        let out = parse(&[
            "cargo:include=/legacy",
            "cargo::metadata=root=/modern",
            "cargo::not-a-thing=1",
        ]);
        assert_eq!(
            out.metadata,
            [
                ("include".to_string(), "/legacy".to_string()),
                ("root".to_string(), "/modern".to_string())
            ]
        );
    }

    #[test]
    fn cfg_env_warning_error_and_rerun() {
        let out = parse(&[
            "cargo::rustc-cfg=has_foo",
            "cargo::rustc-cfg=feature=\"bar\"",
            "cargo::rustc-check-cfg=cfg(has_foo)",
            "cargo::rustc-env=GREETING=hello world",
            "cargo::warning=careful",
            "cargo::error=broken",
            "cargo::rerun-if-changed=build/x.c",
            "cargo::rerun-if-env-changed=CC",
            "not a directive",
        ]);
        assert_eq!(out.cfgs, ["has_foo", "feature=\"bar\""]);
        assert_eq!(out.check_cfgs, ["cfg(has_foo)"]);
        assert_eq!(
            out.env_vars,
            [("GREETING".to_string(), "hello world".to_string())]
        );
        assert_eq!(out.warnings, ["careful"]);
        assert_eq!(out.errors, ["broken"]);
        assert_eq!(out.rerun_if_changed, [PathBuf::from("build/x.c")]);
        assert_eq!(out.rerun_if_env_changed, ["CC"]);
    }
}
