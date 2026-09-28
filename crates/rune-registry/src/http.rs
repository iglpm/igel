//! The HTTP client every registry request goes through, configured like
//! Cargo's (`http.cainfo`, `http.proxy`, `http.timeout`, `http.user-agent`,
//! `net.retry`; see [`rune_common::net`]).

use reqwest::blocking::{Client, ClientBuilder, RequestBuilder, Response};
use rune_common::net::{self, HttpConfig};
use std::time::Duration;
use tracing::{debug, warn};

/// The default `User-Agent`.
pub const USER_AGENT: &str = concat!("rune/", env!("CARGO_PKG_VERSION"));

/// Applies the process's network settings to a client builder.
pub fn configure(mut builder: ClientBuilder, http: &HttpConfig) -> ClientBuilder {
    builder = builder.user_agent(http.user_agent.as_deref().unwrap_or(USER_AGENT));
    for (path, pem) in http.ca_bundles() {
        match pem.map(|pem| reqwest::Certificate::from_pem_bundle(&pem)) {
            Ok(Ok(certs)) => {
                for cert in certs {
                    builder = builder.add_root_certificate(cert);
                }
            }
            Ok(Err(e)) => warn!("ignoring CA bundle {}: {e}", path.display()),
            Err(e) => warn!("cannot read CA bundle {}: {e}", path.display()),
        }
    }
    if let Some(proxy) = &http.proxy {
        match reqwest::Proxy::all(proxy) {
            Ok(p) => builder = builder.proxy(p),
            Err(e) => warn!("ignoring http.proxy `{proxy}`: {e}"),
        }
    }
    if let Some(t) = http.timeout {
        builder = builder.timeout(t);
    }
    builder
}

/// A blocking client with the process's network settings on top of `builder`.
pub fn client(builder: ClientBuilder) -> Client {
    configure(builder, net::global())
        .build()
        .unwrap_or_else(|e| {
            warn!("HTTP client configuration rejected ({e}); using defaults");
            Client::new()
        })
}

/// A client created on first use: building one loads trust stores and starts
/// an I/O thread, which a build that never touches the network should not pay
/// for.
pub struct LazyClient {
    cell: std::sync::OnceLock<Client>,
    setup: fn(ClientBuilder) -> ClientBuilder,
}

impl LazyClient {
    /// `setup` adds the caller's own settings (pool size, timeouts) on top of
    /// the process-wide ones.
    pub const fn new(setup: fn(ClientBuilder) -> ClientBuilder) -> Self {
        Self {
            cell: std::sync::OnceLock::new(),
            setup,
        }
    }

    pub fn client(&self) -> &Client {
        self.cell
            .get_or_init(|| client((self.setup)(Client::builder())))
    }
}

impl std::ops::Deref for LazyClient {
    type Target = Client;
    fn deref(&self) -> &Client {
        self.client()
    }
}

impl Clone for LazyClient {
    fn clone(&self) -> Self {
        let cell = std::sync::OnceLock::new();
        if let Some(c) = self.cell.get() {
            let _ = cell.set(c.clone());
        }
        Self {
            cell,
            setup: self.setup,
        }
    }
}

/// Why a request failed: worth another attempt, or final.
fn transient(result: &reqwest::Result<Response>) -> bool {
    match result {
        Ok(resp) => net::is_retryable_status(resp.status().as_u16()),
        Err(e) => e.is_timeout() || e.is_connect() || e.is_request() || e.is_body(),
    }
}

/// Sends an idempotent request (a GET), retrying transient failures —
/// connection errors, timeouts, `429` and `5xx` — up to `net.retry` times with
/// a growing delay. `make` builds a fresh request for every attempt.
pub fn send_with_retry(make: impl Fn() -> RequestBuilder) -> reqwest::Result<Response> {
    send_with_retry_n(make, net::global().retries, net::retry_delay)
}

fn send_with_retry_n(
    make: impl Fn() -> RequestBuilder,
    retries: u32,
    delay: impl Fn(u32) -> Duration,
) -> reqwest::Result<Response> {
    let mut attempt = 0;
    loop {
        let result = make().send();
        if attempt >= retries || !transient(&result) {
            return result;
        }
        attempt += 1;
        match &result {
            Ok(resp) => debug!(
                "{} answered {}; retry {attempt}/{retries}",
                resp.url(),
                resp.status()
            ),
            Err(e) => debug!("request failed ({e}); retry {attempt}/{retries}"),
        }
        std::thread::sleep(delay(attempt));
    }
}

/// A one-line explanation of a failed request for users: the innermost cause
/// (e.g. "invalid peer certificate: UnknownIssuer") plus, for certificate
/// problems, how to trust a custom CA.
pub fn describe(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    let mut innermost = None;
    while let Some(s) = source {
        innermost = Some(s.to_string());
        source = s.source();
    }
    if let Some(inner) = innermost
        && !msg.contains(&inner)
    {
        msg = format!("{msg}: {inner}");
    }
    let lower = msg.to_ascii_lowercase();
    if lower.contains("certificate") || lower.contains("unknownissuer") {
        msg.push_str(
            " (if a proxy or private CA re-signs TLS traffic, point `http.cainfo` in \
             .cargo/config.toml, or CARGO_HTTP_CAINFO / SSL_CERT_FILE, at its CA bundle)",
        );
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A server that answers the first `fail` requests with `503`, then `200`.
    fn flaky_server(fail: usize) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let n = h.fetch_add(1, Ordering::SeqCst);
                let reply = if n < fail {
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"
                };
                let _ = s.write_all(reply.as_bytes());
            }
        });
        (url, hits)
    }

    #[test]
    fn transient_failures_are_retried_until_they_succeed() {
        let (url, hits) = flaky_server(2);
        let c = Client::new();
        let resp = send_with_retry_n(|| c.get(&url), 3, |_| Duration::ZERO).unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn retries_stop_at_the_configured_count() {
        let (url, hits) = flaky_server(10);
        let c = Client::new();
        let resp = send_with_retry_n(|| c.get(&url), 1, |_| Duration::ZERO).unwrap();
        assert_eq!(resp.status().as_u16(), 503);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_refused_connection_is_reported_with_its_cause() {
        // Bind and drop: nothing listens on the port any more.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let c = Client::new();
        let e = send_with_retry_n(
            || c.get(format!("http://127.0.0.1:{port}/")),
            1,
            |_| Duration::ZERO,
        )
        .unwrap_err();
        assert!(e.is_connect(), "{e:?}");
        assert!(describe(&e).len() > e.to_string().len() || describe(&e).contains("refused"));
    }
}
