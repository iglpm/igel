use crate::settings::Settings;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct IgelConfig {
    pub project_root: PathBuf,
    pub igel_dir: PathBuf,
    pub target_dir: PathBuf,
    pub fingerprint_dir: PathBuf,
    pub cas_dir: PathBuf,
    pub jobs: usize,
    pub settings: Settings,
}

/// A third more jobs than CPUs fills the gaps where compilers wait; the jobserver bounds total threads.
pub fn default_jobs() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    cpus + cpus / 3
}

fn absolute_in(base: &Path, path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    crate::fsutil::simplified(&path).to_path_buf()
}

impl IgelConfig {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let project_root = crate::fsutil::simplified(&project_root.into()).to_path_buf();
        let igel_dir = project_root.join(".igel");
        let target_dir = igel_dir.join("target");
        let fingerprint_dir = igel_dir.join("fingerprints");

        let cas_dir = home::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".igel")
            .join("cas");
        let cas_dir = match std::env::var_os("IGEL_HOME").filter(|h| !h.is_empty()) {
            Some(home) => PathBuf::from(home).join("cas"),
            None => cas_dir,
        };
        let cas_dir = absolute_in(&std::env::current_dir().unwrap_or_default(), &cas_dir);

        Self {
            project_root,
            igel_dir,
            target_dir,
            fingerprint_dir,
            cas_dir,
            jobs: default_jobs(),
            settings: Settings::default(),
        }
    }

    pub fn with_settings(mut self, settings: Settings) -> Self {
        // The compiler runs in other directories, so every path must be absolute.
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

    pub fn profile_target_dir(&self, profile_name: &str) -> PathBuf {
        let dir_name = match profile_name {
            "dev" | "test" => "debug",
            "bench" => "release",
            other => other,
        };
        self.target_dir.join(dir_name)
    }

    pub fn deps_dir(&self, profile_name: &str) -> PathBuf {
        self.profile_target_dir(profile_name).join("deps")
    }

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

    pub fn build_script_out_dir_by_key(&self, profile_name: &str, key: &str) -> PathBuf {
        self.profile_target_dir(profile_name)
            .join("build")
            .join(key)
            .join("out")
    }

    pub fn build_script_bin_dir_by_key(&self, profile_name: &str, key: &str) -> PathBuf {
        self.profile_target_dir(profile_name)
            .join("build")
            .join(key)
            .join("script")
    }

    pub fn ensure_dirs(&self, profile_name: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.fingerprint_dir)?;
        std::fs::create_dir_all(self.deps_dir(profile_name))?;
        std::fs::create_dir_all(self.profile_target_dir(profile_name).join("build"))?;
        std::fs::create_dir_all(self.cas_dir.join("objects"))?;
        crate::fsutil::tag_cache_dir(&self.target_dir);
        crate::fsutil::tag_cache_dir(&self.cas_dir);
        Ok(())
    }

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
        let config = IgelConfig::new("/tmp/myproject");
        assert_eq!(config.igel_dir, PathBuf::from("/tmp/myproject/.igel"));
        assert_eq!(
            config.deps_dir("dev"),
            PathBuf::from("/tmp/myproject/.igel/target/debug/deps")
        );
        assert_eq!(
            config.deps_dir("release"),
            PathBuf::from("/tmp/myproject/.igel/target/release/deps")
        );
    }

    #[test]
    fn directories_handed_to_the_compiler_are_absolute() {
        use crate::settings::{Origin, Value};
        let root = std::env::temp_dir().join("igel-config-test");
        let mut settings = Settings::default();
        let origin = || Origin::Env("TEST".into());
        settings.override_value(
            "build.target-dir",
            Value::Path("out/build".into()),
            origin(),
        );
        settings.override_value("cas.dir", Value::Path("shared-cas".into()), origin());
        let config = IgelConfig::new(&root).with_settings(settings);
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
        let config = IgelConfig::new("/tmp/myproject");
        let dir = config.build_script_out_dir("dev", "my-crate", "abc123");
        assert_eq!(
            dir,
            PathBuf::from("/tmp/myproject/.igel/target/debug/build/my-crate-abc123/out")
        );
    }
}
