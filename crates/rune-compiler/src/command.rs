use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct RustcCommandBuilder {
    source_root: PathBuf,
    flags: Vec<OsString>,
    envs: Vec<(String, String)>,
    current_dir: Option<PathBuf>,
}

impl RustcCommandBuilder {
    pub fn new(source_root: PathBuf) -> Self {
        Self {
            source_root,
            flags: Vec::new(),
            envs: Vec::new(),
            current_dir: None,
        }
    }

    pub fn crate_name(mut self, name: &str) -> Self {
        self.flags.push("--crate-name".into());
        self.flags.push(name.into());
        self
    }

    pub fn crate_type(mut self, ct: &str) -> Self {
        self.flags.push("--crate-type".into());
        self.flags.push(ct.into());
        self
    }

    pub fn edition(mut self, edition: &str) -> Self {
        self.flags.push("--edition".into());
        self.flags.push(edition.into());
        self
    }

    pub fn out_dir(mut self, dir: &Path) -> Self {
        self.flags.push("--out-dir".into());
        self.flags.push(dir.into());
        self
    }

    pub fn emit(mut self, kinds: &[&str]) -> Self {
        if kinds.is_empty() {
            return self;
        }
        self.flags.push("--emit".into());
        self.flags.push(kinds.join(",").into());
        self
    }

    pub fn extern_crate(mut self, name: &str, path: &Path) -> Self {
        self.flags.push("--extern".into());
        let mut arg = OsString::from(name);
        arg.push("=");
        arg.push(path);
        self.flags.push(arg);
        self
    }

    pub fn extern_crate_bare(mut self, name: &str) -> Self {
        self.flags.push("--extern".into());
        self.flags.push(name.into());
        self
    }

    pub fn search_path(mut self, kind: &str, path: &Path) -> Self {
        self.flags.push("-L".into());
        let mut arg = OsString::from(kind);
        arg.push("=");
        arg.push(path);
        self.flags.push(arg);
        self
    }

    pub fn cfg(mut self, cfg: &str) -> Self {
        self.flags.push("--cfg".into());
        self.flags.push(cfg.into());
        self
    }

    pub fn check_cfg(mut self, check_cfg: &str) -> Self {
        self.flags.push("--check-cfg".into());
        self.flags.push(check_cfg.into());
        self
    }

    pub fn feature(mut self, name: &str) -> Self {
        self.flags.push("--cfg".into());
        self.flags.push(format!("feature=\"{}\"", name).into());
        self
    }

    pub fn opt_level(mut self, level: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("opt-level={}", level).into());
        self
    }

    pub fn debuginfo(mut self, level: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("debuginfo={}", level).into());
        self
    }

    pub fn metadata_hash(mut self, hash: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("metadata={}", hash).into());
        self.flags.push("-C".into());
        self.flags.push(format!("extra-filename=-{}", hash).into());
        self
    }

    pub fn cap_lints(mut self, level: &str) -> Self {
        self.flags.push("--cap-lints".into());
        self.flags.push(level.into());
        self
    }

    pub fn target(mut self, triple: &str) -> Self {
        self.flags.push("--target".into());
        self.flags.push(triple.into());
        self
    }

    pub fn incremental(mut self, dir: &Path) -> Self {
        self.flags.push("-C".into());
        let mut arg = OsString::from("incremental=");
        arg.push(dir);
        self.flags.push(arg);
        self
    }

    pub fn codegen_flag(mut self, flag: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(flag.into());
        self
    }

    pub fn codegen_units(mut self, n: u32) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("codegen-units={}", n).into());
        self
    }

    pub fn lto(mut self, mode: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("lto={}", mode).into());
        self
    }

    pub fn panic(mut self, strategy: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("panic={}", strategy).into());
        self
    }

    pub fn link_arg(mut self, arg: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("link-arg={arg}").into());
        self
    }

    pub fn linker(mut self, linker: &str) -> Self {
        self.flags.push("-C".into());
        self.flags.push(format!("linker={linker}").into());
        self
    }

    pub fn env(mut self, key: &str, val: &str) -> Self {
        self.envs.push((key.to_string(), val.to_string()));
        self
    }

    pub fn link_lib(mut self, kind: Option<&str>, name: &str) -> Self {
        self.flags.push("-l".into());
        if let Some(k) = kind {
            self.flags.push(format!("{}={}", k, name).into());
        } else {
            self.flags.push(name.into());
        }
        self
    }

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

    pub fn test_harness(mut self) -> Self {
        self.flags.push("--test".into());
        self
    }

    pub fn extra_flags(mut self, flags: Vec<String>) -> Self {
        for flag in flags {
            self.flags.push(flag.into());
        }
        self
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    pub fn into_args(self) -> Vec<OsString> {
        self.flags
    }

    pub fn build_with_rustc(self, rustc_path: &Path) -> Command {
        self.build_with_command(Command::new(rustc_path))
    }

    pub fn build_with_command(self, mut cmd: Command) -> Command {
        if let Some(dir) = &self.current_dir {
            cmd.current_dir(dir);
        }
        cmd.arg(&self.source_root);
        cmd.args(&self.flags);
        for (k, v) in self.envs {
            cmd.env(k, v);
        }
        cmd
    }

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
