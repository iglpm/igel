use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A builder for constructing `rustc` command lines.
#[derive(Debug, Clone)]
pub struct RustcCommandBuilder {
    source_root: PathBuf,
    flags: Vec<OsString>,
    envs: Vec<(String, String)>,
}

impl RustcCommandBuilder {
    /// Create a new builder for the given source root (e.g. `src/lib.rs`).
    pub fn new(source_root: PathBuf) -> Self {
        Self {
            source_root,
            flags: Vec::new(),
            envs: Vec::new(),
        }
    }

    /// Set the crate name (`--crate-name`).
    pub fn crate_name(mut self, name: &str) -> Self {
        self.flags.push("--crate-name".into());
        self.flags.push(name.into());
        self
    }

    /// Set the crate type (`--crate-type`).
    pub fn crate_type(mut self, ct: &str) -> Self {
        self.flags.push("--crate-type".into());
        self.flags.push(ct.into());
        self
    }

    /// Set the edition (`--edition`).
    pub fn edition(mut self, edition: &str) -> Self {
        self.flags.push("--edition".into());
        self.flags.push(edition.into());
        self
    }

    /// Set the output directory (`--out-dir`).
    pub fn out_dir(mut self, dir: &Path) -> Self {
        self.flags.push("--out-dir".into());
        self.flags.push(dir.into());
        self
    }

    /// Set the emission kinds (`--emit`).
    pub fn emit(mut self, kinds: &[&str]) -> Self {
        if kinds.is_empty() {
            return self;
        }
        self.flags.push("--emit".into());
        self.flags.push(kinds.join(",").into());
        self
    }

    /// Add an extern crate (`--extern`).
    pub fn extern_crate(mut self, name: &str, path: &Path) -> Self {
        self.flags.push("--extern".into());
        let mut arg = OsString::from(name);
        arg.push("=");
        arg.push(path);
        self.flags.push(arg);
        self
    }

    /// Add a bare extern crate (`--extern name`, e.g. for standard libraries like `proc_macro`).
    pub fn extern_crate_bare(mut self, name: &str) -> Self {
        self.flags.push("--extern".into());
        self.flags.push(name.into());
        self
    }

    /// Add a search path (`-L`).
    pub fn search_path(mut self, kind: &str, path: &Path) -> Self {
        self.flags.push("-L".into());
        let mut arg = OsString::from(kind);
        arg.push("=");
        arg.push(path);
        self.flags.push(arg);
        self
    }

    /// Add a generic cfg (`--cfg`).
    pub fn cfg(mut self, cfg: &str) -> Self {
        self.flags.push("--cfg".into());
        self.flags.push(cfg.into());
        self
    }

    /// Add a check-cfg (`--check-cfg`).
    pub fn check_cfg(mut self, check_cfg: &str) -> Self {
        self.flags.push("--check-cfg".into());
        self.flags.push(check_cfg.into());
        self
    }

    /// Add a feature, translating to `--cfg 'feature="name"'`.
    pub fn feature(mut self, name: &str) -> Self {
        self.flags.push("--cfg".into());
        self.flags.push(format!("feature=\"{}\"", name).into());
        self
    }

    /// Set the optimization level (`-C opt-level`).
    pub fn opt_level(mut self, level: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("opt-level={}", level).into());
        self
    }

    /// Set the debuginfo level (`-C debuginfo`).
    pub fn debuginfo(mut self, level: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("debuginfo={}", level).into());
        self
    }

    /// Set metadata hash (`-C metadata=...` and `-C extra-filename=...`).
    pub fn metadata_hash(mut self, hash: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("metadata={}", hash).into());
        self.flags.push("-C".into());
        self.flags.push(format!("extra-filename=-{}", hash).into());
        self
    }

    /// Set lint cap level (`--cap-lints`).
    pub fn cap_lints(mut self, level: &str) -> Self {
        self.flags.push("--cap-lints".into());
        self.flags.push(level.into());
        self
    }

    /// Set target triple (`--target`).
    pub fn target(mut self, triple: &str) -> Self {
        self.flags.push("--target".into());
        self.flags.push(triple.into());
        self
    }

    /// Enable incremental compilation (`-C incremental`).
    pub fn incremental(mut self, dir: &Path) -> Self {
        self.flags.push("-C".into());
        let mut arg = OsString::from("incremental=");
        arg.push(dir);
        self.flags.push(arg);
        self
    }

    /// Pass an arbitrary `-C <flag>` (e.g. `strip=symbols`).
    pub fn codegen_flag(mut self, flag: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(flag.into());
        self
    }

    /// Set the number of codegen units (`-C codegen-units`).
    pub fn codegen_units(mut self, n: u32) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("codegen-units={}", n).into());
        self
    }

    /// Set LTO mode (`-C lto`).
    pub fn lto(mut self, mode: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("lto={}", mode).into());
        self
    }

    /// Set panic strategy (`-C panic`).
    pub fn panic(mut self, strategy: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("panic={}", strategy).into());
        self
    }

    /// Set an environment variable for compilation.
    pub fn env(mut self, key: &str, val: &str) -> Self {
        self.envs.push((key.to_string(), val.to_string()));
        self
    }

    /// Link a library (`-l`).
    pub fn link_lib(mut self, kind: Option<&str>, name: &str) -> Self {
        self.flags.push("-l".into());
        if let Some(k) = kind {
            self.flags.push(format!("{}={}", k, name).into());
        } else {
            self.flags.push(name.into());
        }
        self
    }

    /// Add a link search path (`-L`).
    pub fn link_search(mut self, kind: Option<&str>, path: &Path) -> Self {
        self.flags.push("-L".into());
        if let Some(k) = kind {
            let mut arg = OsString::from(k);
            arg.push("=");
            arg.push(path);
            self.flags.push(arg);
        } else {
            self.flags.push(path.into());
        }
        self
    }

    /// Output JSON diagnostics and artifacts, with ANSI colors in the rendered
    /// diagnostics only when `color` is set.
    pub fn json_output(mut self, color: bool) -> Self {
        self.flags.push("--error-format=json".into());
        self.flags.push(
            if color {
                "--json=diagnostic-rendered-ansi,artifacts,future-incompat"
            } else {
                "--json=artifacts,future-incompat"
            }
            .into(),
        );
        self
    }

    /// Compile as a test harness binary (`--test`): rustc auto-generates a
    /// `main()` that discovers and runs every `#[test]` item and implies
    /// `cfg(test)`, in place of an explicit `--crate-type`/`--emit`.
    pub fn test_harness(mut self) -> Self {
        self.flags.push("--test".into());
        self
    }

    /// Add arbitrary extra flags.
    pub fn extra_flags(mut self, flags: Vec<String>) -> Self {
        for flag in flags {
            self.flags.push(flag.into());
        }
        self
    }

    /// Construct a `std::process::Command` from this builder using a specific rustc path.
    pub fn build_with_rustc(self, rustc_path: &Path) -> Command {
        let mut cmd = Command::new(rustc_path);
        cmd.arg(&self.source_root);
        cmd.args(&self.flags);
        for (k, v) in self.envs {
            cmd.env(k, v);
        }
        cmd
    }

    /// Construct a `std::process::Command` from this builder with default "rustc".
    pub fn build(self) -> Command {
        self.build_with_rustc(Path::new("rustc"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rustc_command_builder() {
        let builder = RustcCommandBuilder::new(PathBuf::from("src/lib.rs"))
            .crate_name("my_crate")
            .edition("2021")
            .emit(&["metadata", "link"])
            .feature("std")
            .json_output(true);

        let cmd = builder.build();
        let prog = cmd.get_program();
        assert_eq!(prog, "rustc");
    }
}
