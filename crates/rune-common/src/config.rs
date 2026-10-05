//! Global Rune configuration.
//!
//! Handles discovery of `.rune/` directories, CAS store paths,
//! and other global settings.

use crate::settings::Settings;
use std::path::{Path, PathBuf};

/// Global Rune configuration.
#[derive(Debug, Clone)]
pub struct RuneConfig {
    /// The project root directory (where the manifest lives).
    pub project_root: PathBuf,
    /// The Rune output directory (`.rune/` within project root).
    pub rune_dir: PathBuf,
    /// The target output directory (`.rune/target/`).
    pub target_dir: PathBuf,
    /// The fingerprint cache directory (`.rune/fingerprints/`).
    pub fingerprint_dir: PathBuf,
    /// The global CAS store directory (`~/.rune/cas/`).
    pub cas_dir: PathBuf,
    /// Number of parallel jobs (default: num_cpus).
    pub jobs: usize,
    /// Layered user settings (`rune config`): global file, `Rune.toml`, env.
    pub settings: Settings,
}

/// Default number of parallel jobs: a third more than the CPU count.
///
/// A compiler process spends part of its life waiting (on a build script, on
/// the linker, on disk) and the scheduler can only start a unit once its
/// dependencies' metadata exists; with exactly one job per core the machine
/// idles during those gaps. The jobserver shared with every `rustc` keeps the
/// *total* thread count in check, so a few extra processes only fill the
/// gaps. Measured on a 12-core machine building 197 crates: 12 jobs took 32.8s,
/// 16 jobs took 29.2s (Cargo, one job per core: 28.7s).
pub fn default_jobs() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    cpus + cpus / 3
}

/// `path` as an absolute path: itself if it is one, else below `base`.
fn absolute_in(base: &Path, path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    crate::fsutil::simplified(&path).to_path_buf()
}

impl RuneConfig {
    /// Create a new `RuneConfig` for the given project root.
    ///
    /// Creates the standard directory structure:
    /// - `.rune/target/{debug,release}/deps/`
    /// - `.rune/fingerprints/`
    /// - `~/.rune/cas/objects/`
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let project_root = crate::fsutil::simplified(&project_root.into()).to_path_buf();
        let rune_dir = project_root.join(".rune");
        let target_dir = rune_dir.join("target");
        let fingerprint_dir = rune_dir.join("fingerprints");

        let cas_dir = home::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".rune")
            .join("cas");
        let cas_dir = match std::env::var_os("RUNE_HOME").filter(|h| !h.is_empty()) {
            Some(home) => PathBuf::from(home).join("cas"),
            None => cas_dir,
        };
        let cas_dir = absolute_in(&std::env::current_dir().unwrap_or_default(), &cas_dir);

        Self {
            project_root,
            rune_dir,
            target_dir,
            fingerprint_dir,
            cas_dir,
            jobs: default_jobs(),
            settings: Settings::default(),
        }
    }

    /// Applies user settings: `build.target-dir`, `cas.dir`, and `build.jobs`
    /// take effect on the paths and job count carried by this config.
    pub fn with_settings(mut self, settings: Settings) -> Self {
        // The compiler runs in other directories (a registry crate's package
        // root), so every path it is handed must be absolute. (`Rune.toml`'s
        // relative paths arrive resolved against the project; one from the
        // environment or the command line is the current directory's.)
        let cwd = std::env::current_dir().unwrap_or_default();
        if let Some(dir) = settings.target_dir() {
            self.target_dir = absolute_in(&cwd, &dir);
        }
        if let Some(dir) = settings.cas_dir() {
            self.cas_dir = absolute_in(&cwd, &dir);
        }
        if let Some(jobs) = settings.jobs() {
            self.jobs = jobs;
        }
        self.settings = settings;
        self
    }

    /// Returns the output directory for the given profile.
    /// e.g., `.rune/target/debug/` or `.rune/target/release/`
    ///
    /// `profile_name` is the profile's *output directory name*
    /// (`Profile::output_dir_name`): `debug`, `release`, or a custom profile.
    pub fn profile_target_dir(&self, profile_name: &str) -> PathBuf {
        let dir_name = match profile_name {
            "dev" | "test" => "debug",
            "bench" => "release",
            other => other,
        };
        self.target_dir.join(dir_name)
    }

    /// Returns the deps directory for compiled dependencies.
    pub fn deps_dir(&self, profile_name: &str) -> PathBuf {
        self.profile_target_dir(profile_name).join("deps")
    }

    /// Returns the build script output directory for a crate.
    pub fn build_script_out_dir(
        &self,
        profile_name: &str,
        crate_name: &str,
        metadata_hash: &str,
    ) -> PathBuf {
        self.profile_target_dir(profile_name)
            .join("build")
            .join(format!("{crate_name}-{metadata_hash}"))
            .join("out")
    }

    /// Like [`Self::build_script_out_dir`], for a `crate-name-hash` directory key.
    pub fn build_script_out_dir_by_key(&self, profile_name: &str, key: &str) -> PathBuf {
        self.profile_target_dir(profile_name)
            .join("build")
            .join(key)
            .join("out")
    }

    /// Where a package's build script is compiled: beside its `OUT_DIR`, not
    /// in it, so that the directory the script fills (and the store keeps)
    /// holds only what the script generated.
    pub fn build_script_bin_dir_by_key(&self, profile_name: &str, key: &str) -> PathBuf {
        self.profile_target_dir(profile_name)
            .join("build")
            .join(key)
            .join("script")
    }

    /// Ensure all necessary directories exist.
    pub fn ensure_dirs(&self, profile_name: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.fingerprint_dir)?;
        std::fs::create_dir_all(self.deps_dir(profile_name))?;
        std::fs::create_dir_all(self.profile_target_dir(profile_name).join("build"))?;
        std::fs::create_dir_all(self.cas_dir.join("objects"))?;
        crate::fsutil::tag_cache_dir(&self.target_dir);
        crate::fsutil::tag_cache_dir(&self.cas_dir);
        Ok(())
    }

    /// Discover the project root by searching upward from the given
    /// path for a manifest (`Cargo.toml`, or a `Rune.toml` that is one).
    pub fn discover(start: &Path) -> Option<Self> {
        let mut current = start.to_path_buf();
        loop {
            if crate::manifest_file::has_manifest(&current) {
                return Some(Self::new(current));
            }
            if !current.pop() {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_paths() {
        let config = RuneConfig::new("/tmp/myproject");
        assert_eq!(config.rune_dir, PathBuf::from("/tmp/myproject/.rune"));
        assert_eq!(
            config.deps_dir("dev"),
            PathBuf::from("/tmp/myproject/.rune/target/debug/deps")
        );
        assert_eq!(
            config.deps_dir("release"),
            PathBuf::from("/tmp/myproject/.rune/target/release/deps")
        );
    }

    #[test]
    fn directories_handed_to_the_compiler_are_absolute() {
        use crate::settings::{Origin, Value};
        let root = std::env::temp_dir().join("rune-config-test");
        let mut settings = Settings::default();
        let origin = || Origin::Env("TEST".into());
        settings.override_value(
            "build.target-dir",
            Value::Path("out/build".into()),
            origin(),
        );
        settings.override_value("cas.dir", Value::Path("shared-cas".into()), origin());
        let config = RuneConfig::new(&root).with_settings(settings);
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(config.target_dir, cwd.join("out").join("build"));
        assert_eq!(config.cas_dir, cwd.join("shared-cas"));
        assert!(config.target_dir.is_absolute() && config.cas_dir.is_absolute());
    }

    #[test]
    fn default_jobs_slightly_exceeds_the_cpu_count() {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        assert!(default_jobs() >= cpus);
        assert!(default_jobs() <= cpus * 2);
    }

    #[test]
    fn test_build_script_out_dir() {
        let config = RuneConfig::new("/tmp/myproject");
        let dir = config.build_script_out_dir("dev", "my-crate", "abc123");
        assert_eq!(
            dir,
            PathBuf::from("/tmp/myproject/.rune/target/debug/build/my-crate-abc123/out")
        );
    }
}
