//! Network settings shared by every HTTP client: Cargo's `[http]`/`[net]` keys and env overrides.

use crate::cargo_config::CargoConfig;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

pub const DEFAULT_RETRIES: u32 = 3;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpConfig {
    pub ca_files: Vec<PathBuf>,
    pub proxy: Option<String>,
    pub timeout: Option<Duration>,
    pub user_agent: Option<String>,
    pub retries: u32,
}

impl HttpConfig {
    pub fn from_config(config: &CargoConfig) -> Self {
        let mut ca_files = Vec::new();
        if let Some(p) = config.string(&["http", "cainfo"]).filter(|s| !s.is_empty()) {
            ca_files.push(PathBuf::from(p));
        }
        if let Some(p) = std::env::var_os("SSL_CERT_FILE").filter(|s| !s.is_empty()) {
            let p = PathBuf::from(p);
            if !ca_files.contains(&p) {
                ca_files.push(p);
            }
        }
        let timeout = config
            .string(&["http", "timeout"])
            .and_then(|s| s.trim().parse::<u64>().ok())
            .or_else(|| {
                config
                    .file_value(&["http", "timeout"])
                    .and_then(|v| v.as_integer())
                    .and_then(|i| u64::try_from(i).ok())
            })
            .filter(|&s| s > 0)
            .map(Duration::from_secs);
        let retries = config
            .string(&["net", "retry"])
            .and_then(|s| s.trim().parse::<u32>().ok())
            .or_else(|| {
                config
                    .file_value(&["net", "retry"])
                    .and_then(|v| v.as_integer())
                    .and_then(|i| u32::try_from(i).ok())
            })
            .unwrap_or(DEFAULT_RETRIES);
        Self {
            ca_files,
            proxy: config
                .string(&["http", "proxy"])
                .filter(|s| !s.trim().is_empty()),
            timeout,
            user_agent: config.string(&["http", "user-agent"]),
            retries,
        }
    }

    pub fn from_env() -> Self {
        Self::from_config(&CargoConfig::empty())
    }

    pub fn ca_bundles(&self) -> Vec<(PathBuf, std::io::Result<Vec<u8>>)> {
        self.ca_files
            .iter()
            .map(|p| (p.clone(), std::fs::read(p)))
            .collect()
    }
}

static GLOBAL: OnceLock<HttpConfig> = OnceLock::new();

pub fn configure(config: HttpConfig) {
    let _ = GLOBAL.set(config);
}

pub fn global() -> &'static HttpConfig {
    GLOBAL.get_or_init(HttpConfig::from_env)
}

pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

pub fn retry_delay(attempt: u32) -> Duration {
    let ms = 250u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(4));
    Duration::from_millis(ms.min(4_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_settings_come_from_configuration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
        std::fs::write(
            dir.path().join(".cargo/config.toml"),
            "[http]\ncainfo = \"/etc/corp-ca.pem\"\nproxy = \"http://proxy:3128\"\ntimeout = 7\n\
             user-agent = \"corp\"\n[net]\nretry = 5\n",
        )
        .unwrap();
        let cfg = CargoConfig::load(dir.path());
        let http = HttpConfig::from_config(&cfg);
        let env_or = |var: &str, file: &str| std::env::var(var).unwrap_or_else(|_| file.into());
        assert_eq!(
            http.ca_files[0],
            PathBuf::from(env_or("CARGO_HTTP_CAINFO", "/etc/corp-ca.pem"))
        );
        assert_eq!(
            http.proxy.as_deref(),
            Some(env_or("CARGO_HTTP_PROXY", "http://proxy:3128").as_str())
        );
        if std::env::var_os("CARGO_HTTP_TIMEOUT").is_none() {
            assert_eq!(http.timeout, Some(Duration::from_secs(7)));
        }
        if std::env::var_os("CARGO_HTTP_USER_AGENT").is_none() {
            assert_eq!(http.user_agent.as_deref(), Some("corp"));
        }
        if std::env::var_os("CARGO_NET_RETRY").is_none() {
            assert_eq!(http.retries, 5);
        }
    }

    #[test]
    fn defaults_retry_three_times_and_add_nothing() {
        if std::env::vars()
            .any(|(k, _)| k.starts_with("CARGO_HTTP_") || k.starts_with("CARGO_NET_"))
        {
            return; // the environment configures the network itself
        }
        let http = HttpConfig::from_config(&CargoConfig::empty());
        assert_eq!(http.retries, DEFAULT_RETRIES);
        assert!(http.proxy.is_none() && http.timeout.is_none());
    }

    #[test]
    fn only_transient_statuses_are_retried() {
        for s in [429, 500, 502, 503, 504, 408] {
            assert!(is_retryable_status(s), "{s}");
        }
        for s in [200, 304, 401, 403, 404, 410] {
            assert!(!is_retryable_status(s), "{s}");
        }
    }

    #[test]
    fn back_off_grows_and_is_capped() {
        assert_eq!(retry_delay(1), Duration::from_millis(250));
        assert_eq!(retry_delay(2), Duration::from_millis(500));
        assert!(retry_delay(3) > retry_delay(2));
        assert_eq!(retry_delay(30), Duration::from_millis(4_000));
    }
}
