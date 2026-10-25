//! The registry web API: search, crate info, publishing, yanking and owners.

use crate::source::{CRATES_IO, defs};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    Config(String),
    #[error("the registry rejected the request: {0}")]
    Rejected(String),
    #[error("could not reach the registry: {}", crate::http::describe(.0))]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Index(#[from] crate::index::IndexError),
}

pub type Result<T> = std::result::Result<T, ApiError>;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub downloads: u64,
}

pub struct RegistryApi {
    base: String,
    token: Option<String>,
    client: reqwest::blocking::Client,
    pub name: String,
}

impl RegistryApi {
    pub fn for_index(index: &str) -> Result<Self> {
        let defs = defs();
        let client = crate::http::client(
            reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(300)),
        );
        let token = defs.api_token(index)?;
        let name = defs.name_of(index);
        let base = if index == CRATES_IO {
            std::env::var("IGEL_CRATES_IO_API").unwrap_or_else(|_| "https://crates.io".into())
        } else {
            let url = index
                .strip_prefix("sparse+")
                .ok_or_else(|| {
                    ApiError::Config(format!(
                        "registry `{name}` has no web API (only sparse registries do)"
                    ))
                })?
                .trim_end_matches('/');
            let mut req = client.get(format!("{url}/config.json"));
            if let Some(t) = &token {
                req = req.header("Authorization", t);
            }
            let config: Value = req.send()?.error_for_status()?.json()?;
            config["api"]
                .as_str()
                .ok_or_else(|| {
                    ApiError::Config(format!(
                        "registry `{name}` does not offer a web API (no `api` in its config.json)"
                    ))
                })?
                .to_string()
        };
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            token,
            client,
            name,
        })
    }

    pub fn with_token(mut self, token: &str) -> Self {
        self.token = Some(token.to_string());
        self
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn token(&self) -> Result<&str> {
        self.token.as_deref().ok_or_else(|| {
            ApiError::Config(format!(
                "no token for registry `{}`: run `igl login{}` or set {}",
                self.name,
                if self.name == "crates-io" {
                    String::new()
                } else {
                    format!(" --registry {}", self.name)
                },
                if self.name == "crates-io" {
                    "CARGO_REGISTRY_TOKEN".to_string()
                } else {
                    format!(
                        "CARGO_REGISTRIES_{}_TOKEN",
                        self.name.to_ascii_uppercase().replace('-', "_")
                    )
                }
            ))
        })
    }

    fn call(&self, req: reqwest::blocking::RequestBuilder) -> Result<Value> {
        let resp = req.send()?;
        let status = resp.status();
        let text = resp.text()?;
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if let Some(errors) = body["errors"].as_array() {
            let details: Vec<String> = errors
                .iter()
                .filter_map(|e| e["detail"].as_str().map(str::to_string))
                .collect();
            return Err(ApiError::Rejected(details.join("; ")));
        }
        if !status.is_success() {
            return Err(ApiError::Rejected(format!(
                "{status}{}",
                if text.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", text.trim())
                }
            )));
        }
        Ok(body)
    }

    pub fn search(&self, query: &str, limit: u32) -> Result<(Vec<SearchHit>, u64)> {
        let body = self.call(
            self.client
                .get(format!("{}/api/v1/crates", self.base))
                .query(&[("q", query), ("per_page", &limit.to_string())]),
        )?;
        let hits = body["crates"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| SearchHit {
                        name: c["name"].as_str().unwrap_or_default().to_string(),
                        version: c["max_stable_version"]
                            .as_str()
                            .or_else(|| c["max_version"].as_str())
                            .unwrap_or_default()
                            .to_string(),
                        description: c["description"]
                            .as_str()
                            .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" ")),
                        downloads: c["downloads"].as_u64().unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((hits, body["meta"]["total"].as_u64().unwrap_or(0)))
    }

    pub fn crate_info(&self, name: &str) -> Result<Value> {
        self.call(
            self.client
                .get(format!("{}/api/v1/crates/{name}", self.base)),
        )
    }

    pub fn publish(&self, metadata: &Value, tarball: &[u8]) -> Result<Vec<String>> {
        let json = serde_json::to_vec(metadata).expect("metadata is JSON");
        let mut body = Vec::with_capacity(8 + json.len() + tarball.len());
        body.extend_from_slice(&(json.len() as u32).to_le_bytes());
        body.extend_from_slice(&json);
        body.extend_from_slice(&(tarball.len() as u32).to_le_bytes());
        body.extend_from_slice(tarball);
        let resp = self.call(
            self.client
                .put(format!("{}/api/v1/crates/new", self.base))
                .header("Authorization", self.token()?)
                .header("Content-Type", "application/octet-stream")
                .body(body),
        )?;
        let mut warnings = Vec::new();
        for (kind, label) in [
            ("invalid_categories", "invalid category"),
            ("invalid_badges", "invalid badge"),
            ("other", ""),
        ] {
            for w in resp["warnings"][kind].as_array().into_iter().flatten() {
                let w = w.as_str().unwrap_or_default();
                warnings.push(if label.is_empty() {
                    w.to_string()
                } else {
                    format!("{label}: {w}")
                });
            }
        }
        Ok(warnings)
    }

    pub fn yank(&self, name: &str, version: &str, undo: bool) -> Result<()> {
        let url = format!(
            "{}/api/v1/crates/{name}/{version}/{}",
            self.base,
            if undo { "unyank" } else { "yank" }
        );
        let req = if undo {
            self.client.put(url)
        } else {
            self.client.delete(url)
        };
        self.call(req.header("Authorization", self.token()?))?;
        Ok(())
    }

    pub fn owners(&self, name: &str) -> Result<Vec<String>> {
        let body = self.call(
            self.client
                .get(format!("{}/api/v1/crates/{name}/owners", self.base))
                .header("Authorization", self.token()?),
        )?;
        Ok(body["users"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|u| {
                let login = u["login"].as_str().unwrap_or_default();
                match u["name"].as_str() {
                    Some(n) => format!("{login} ({n})"),
                    None => login.to_string(),
                }
            })
            .collect())
    }

    pub fn change_owners(&self, name: &str, logins: &[String], remove: bool) -> Result<String> {
        let url = format!("{}/api/v1/crates/{name}/owners", self.base);
        let req = if remove {
            self.client.delete(url)
        } else {
            self.client.put(url)
        };
        let body = self.call(
            req.header("Authorization", self.token()?)
                .json(&serde_json::json!({ "users": logins })),
        )?;
        Ok(body["msg"].as_str().unwrap_or_default().to_string())
    }
}
