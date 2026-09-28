//! Network settings shared by every HTTP client Rune creates (registry index,
//! downloads, registry web API, the shared store).
//!
//! These are Cargo's `[http]` and `[net]` configuration keys, with the same
//! environment overrides, so a machine set up for Cargo — a corporate proxy
//! that re-signs TLS, a private CA, a slow link — works for Rune unchanged:
//!
//! | Key | Environment | Meaning |
//! | --- | --- | --- |
//! | `http.cainfo` | `CARGO_HTTP_CAINFO` | Extra CA bundle (PEM) to trust |
//! | `http.proxy` | `CARGO_HTTP_PROXY` | Proxy for every request (else `HTTPS_PROXY`, …) |
//! | `http.timeout` | `CARGO_HTTP_TIMEOUT` | Seconds before a stalled request fails |
//! | `http.user-agent` | `CARGO_HTTP_USER_AGENT` | `User-Agent` header |
//! | `net.retry` | `CARGO_NET_RETRY` | Retries of a failed request (default 3) |
//!
//! `SSL_CERT_FILE` is honoured as well (as by curl and OpenSSL), and the
//! operating system's trust store is used next to the bundled roots.

use crate::cargo_config::CargoConfig;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

/// Cargo's default for `net.retry`.
pub const DEFAULT_RETRIES: u32 = 3;

/// The effective network configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpConfig {
    /// PEM bundles whose certificates are trusted in addition to the
    /// built-in and system roots.
    pub ca_files: Vec<PathBuf>,
    /// Explicit proxy URL (`http.proxy`); `None` leaves reqwest's own
    /// `HTTPS_PROXY`/`NO_PROXY` handling in charge.
    pub proxy: Option<String>,
    /// Overall per-request timeout (`http.timeout`).
    pub timeout: Option<Duration>,
    /// `http.user-agent`.
    pub user_agent: Option<String>,
    /// How often a request that failed for a transient reason is retried.
    pub retries: u32,
}

impl HttpConfig {
    /// Settings from configuration files and the environment.
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

    /// Settings from the environment only (no project configuration found).
    pub fn from_env() -> Self {
        Self::from_config(&CargoConfig::empty())
    }

    /// The PEM text of every configured CA file that could be read, with its
    /// path (unreadable files are reported by the caller, not silently
    /// skipped).
    pub fn ca_bundles(&self) -> Vec<(PathBuf, std::io::Result<Vec<u8>>)> {
        self.ca_files
            .iter()
            .map(|p| (p.clone(), std::fs::read(p)))
            .collect()
    }
}

static GLOBAL: OnceLock<HttpConfig> = OnceLock::new();

/// Fixes the network settings for this process (called once the workspace's
/// configuration is known). Later calls are ignored.
pub fn configure(config: HttpConfig) {
    let _ = GLOBAL.set(config);
}

/// The process's network settings: those passed to [`configure`], or else
/// what the environment says.
pub fn global() -> &'static HttpConfig {
    GLOBAL.get_or_init(HttpConfig::from_env)
}

/// Whether an HTTP status is worth retrying: the server was overloaded or
/// briefly unavailable, not "no such thing" or "not allowed".
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

/// Delay before retry number `attempt` (1-based): 250 ms, 500 ms, 1 s, …
/// capped at 4 s, like Cargo's growing back-off.
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
        // The `CARGO_HTTP_*` variables win over files, as in Cargo.
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
