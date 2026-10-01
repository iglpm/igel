//! Credential providers, as Cargo defines them: where a registry token comes
//! from, and where `rune login` puts it.
//!
//! * `cargo:token` — `token` in configuration, else `credentials.toml`
//!   (the default);
//! * `cargo:token-from-stdout <cmd> <args…>` — the first line a command prints;
//! * `cargo:macos-keychain` — the macOS keychain (service
//!   `cargo-registry:<index-url>`, as Cargo's provider stores it);
//! * `cargo:libsecret` — the Secret Service on Linux (`secret-tool`);
//! * `cargo:wincred` — Windows Credential Manager (not available in Rune yet);
//! * anything else — an external provider program speaking Cargo's JSON
//!   credential protocol (`<program> <args…> --cargo-plugin`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    Token,
    TokenFromStdout(Vec<String>),
    MacosKeychain,
    Libsecret,
    Wincred,
    External(Vec<String>),
}

impl Provider {
    /// Parses a `credential-provider` value: `"cargo:token"`, `"cmd arg"`,
    /// or `["cmd", "arg"]`.
    pub fn parse(value: &toml::Value) -> Option<Provider> {
        let parts: Vec<String> = match value {
            toml::Value::String(s) => s.split_whitespace().map(str::to_string).collect(),
            toml::Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => return None,
        };
        let (first, rest) = parts.split_first()?;
        Some(match first.as_str() {
            "cargo:token" => Provider::Token,
            "cargo:token-from-stdout" => Provider::TokenFromStdout(rest.to_vec()),
            "cargo:macos-keychain" => Provider::MacosKeychain,
            "cargo:libsecret" => Provider::Libsecret,
            "cargo:wincred" => Provider::Wincred,
            _ => Provider::External(parts),
        })
    }
}

/// The registry a credential is for, as providers see it.
pub struct Registry<'a> {
    /// How configuration names it (`crates-io` for crates.io).
    pub name: &'a str,
    /// The index URL Cargo gives providers: crates.io's canonical
    /// `https://github.com/rust-lang/crates.io-index`, `sparse+…` for sparse
    /// registries, the plain URL for git-based ones.
    pub index_url: &'a str,
}

/// The token a provider has for `registry`, `Ok(None)` when it has none.
/// `config_token` is `[registries.NAME] token` from configuration, which
/// `cargo:token` prefers over `credentials.toml`.
pub fn get(
    provider: &Provider,
    registry: &Registry,
    config_token: Option<&str>,
    credentials_file_token: impl FnOnce() -> Option<String>,
) -> Result<Option<String>, String> {
    match provider {
        Provider::Token => Ok(config_token
            .map(str::to_string)
            .or_else(credentials_file_token)),
        Provider::TokenFromStdout(cmd) => {
            let (prog, args) = cmd
                .split_first()
                .ok_or("`cargo:token-from-stdout` needs a command")?;
            let out = Command::new(prog)
                .args(args)
                .env("CARGO_REGISTRY_INDEX_URL", registry.index_url)
                .env("CARGO_REGISTRY_NAME_OPT", registry.name)
                .output()
                .map_err(|e| format!("could not run credential command `{prog}`: {e}"))?;
            if !out.status.success() {
                return Err(format!("credential command `{prog}` failed"));
            }
            Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .map(|l| l.trim().to_string())
                .filter(|t| !t.is_empty()))
        }
        Provider::MacosKeychain => {
            let out = Command::new("security")
                .args([
                    "find-generic-password",
                    "-s",
                    &keychain_service(registry.index_url),
                    "-w",
                ])
                .output()
                .map_err(|e| format!("could not run `security`: {e}"))?;
            Ok(out
                .status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .filter(|t| !t.is_empty()))
        }
        Provider::Libsecret => {
            let out = Command::new("secret-tool")
                .args(["lookup", "url", registry.index_url])
                .output()
                .map_err(|e| format!("`cargo:libsecret` needs `secret-tool`: {e}"))?;
            Ok(out
                .status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .filter(|t| !t.is_empty()))
        }
        Provider::Wincred => Err("`cargo:wincred` is not supported by Rune yet".into()),
        Provider::External(cmd) => {
            let reply = external(
                cmd,
                registry,
                serde_json::json!({ "kind": "get", "operation": "read" }),
            )?;
            match reply {
                Reply::Ok(v) => Ok(v["token"].as_str().map(str::to_string)),
                Reply::NotFound => Ok(None),
            }
        }
    }
}

/// Stores `token` with a provider (`rune login`). `Ok(false)` means the
/// provider does not store tokens itself (`cargo:token` writes
/// `credentials.toml`, which the caller does).
pub fn store(provider: &Provider, registry: &Registry, token: &str) -> Result<bool, String> {
    match provider {
        Provider::Token => Ok(false),
        Provider::TokenFromStdout(_) => Err(
            "`cargo:token-from-stdout` cannot store a token; save it where its command reads it"
                .into(),
        ),
        Provider::MacosKeychain => {
            let service = keychain_service(registry.index_url);
            run_ok(
                Command::new("security").args([
                    "add-generic-password",
                    "-U",
                    "-s",
                    &service,
                    "-a",
                    "",
                    "-w",
                    token,
                ]),
                "security",
            )?;
            Ok(true)
        }
        Provider::Libsecret => {
            let mut child = Command::new("secret-tool")
                .args([
                    "store",
                    "--label",
                    &format!("cargo-registry:{}", registry.index_url),
                    "url",
                    registry.index_url,
                ])
                .stdin(Stdio::piped())
                .spawn()
                .map_err(|e| format!("`cargo:libsecret` needs `secret-tool`: {e}"))?;
            child
                .stdin
                .take()
                .ok_or("no stdin")?
                .write_all(token.as_bytes())
                .map_err(|e| e.to_string())?;
            if !child.wait().map_err(|e| e.to_string())?.success() {
                return Err("`secret-tool store` failed".into());
            }
            Ok(true)
        }
        Provider::Wincred => Err("`cargo:wincred` is not supported by Rune yet".into()),
        Provider::External(cmd) => {
            external(
                cmd,
                registry,
                serde_json::json!({ "kind": "login", "token": token }),
            )?;
            Ok(true)
        }
    }
}

/// Removes the token from a provider (`rune logout`); `Ok(false)` as for
/// [`store`].
pub fn erase(provider: &Provider, registry: &Registry) -> Result<bool, String> {
    match provider {
        Provider::Token | Provider::TokenFromStdout(_) => Ok(false),
        Provider::MacosKeychain => {
            let _ = Command::new("security")
                .args([
                    "delete-generic-password",
                    "-s",
                    &keychain_service(registry.index_url),
                ])
                .output();
            Ok(true)
        }
        Provider::Libsecret => {
            let _ = Command::new("secret-tool")
                .args(["clear", "url", registry.index_url])
                .output();
            Ok(true)
        }
        Provider::Wincred => Err("`cargo:wincred` is not supported by Rune yet".into()),
        Provider::External(cmd) => {
            external(cmd, registry, serde_json::json!({ "kind": "logout" }))?;
            Ok(true)
        }
    }
}

fn keychain_service(index_url: &str) -> String {
    format!("cargo-registry:{index_url}")
}

fn run_ok(cmd: &mut Command, what: &str) -> Result<(), String> {
    let out = cmd
        .output()
        .map_err(|e| format!("could not run `{what}`: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{what}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

enum Reply {
    Ok(serde_json::Value),
    NotFound,
}

/// One request to an external provider, per Cargo's credential protocol
/// (version 1): the provider greets with `{"v":[1]}`, reads one request line
/// and answers `{"Ok": …}` or `{"Err": …}`.
fn external(
    cmd: &[String],
    registry: &Registry,
    request: serde_json::Value,
) -> Result<Reply, String> {
    let (prog, args) = cmd.split_first().ok_or("empty credential provider")?;
    let mut child = Command::new(prog)
        .args(args)
        .arg("--cargo-plugin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("could not run credential provider `{prog}`: {e}"))?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut hello = String::new();
    stdout
        .read_line(&mut hello)
        .map_err(|e| format!("credential provider `{prog}`: {e}"))?;
    let versions: serde_json::Value = serde_json::from_str(hello.trim())
        .map_err(|_| format!("credential provider `{prog}` did not greet with its versions"))?;
    if !versions["v"]
        .as_array()
        .is_some_and(|v| v.iter().any(|x| x == 1))
    {
        return Err(format!(
            "credential provider `{prog}` does not speak protocol version 1"
        ));
    }
    let mut req = serde_json::json!({
        "v": 1,
        "registry": { "index-url": registry.index_url, "name": registry.name },
        "args": [],
    });
    if let (Some(obj), Some(extra)) = (req.as_object_mut(), request.as_object()) {
        obj.extend(extra.clone());
    }
    writeln!(stdin, "{req}").map_err(|e| format!("credential provider `{prog}`: {e}"))?;
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .map_err(|e| format!("credential provider `{prog}`: {e}"))?;
    drop(stdin);
    let _ = child.wait();
    let reply: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|_| format!("credential provider `{prog}` gave an invalid answer: {line}"))?;
    if let Some(ok) = reply.get("Ok") {
        return Ok(Reply::Ok(ok.clone()));
    }
    let err = &reply["Err"];
    match err["kind"].as_str() {
        Some("not-found") | Some("url-not-supported") => Ok(Reply::NotFound),
        _ => Err(format!(
            "credential provider `{prog}` failed: {}",
            err["message"].as_str().unwrap_or("no message")
        )),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn providers_parse_as_cargo_spells_them() {
        let p = |s: &str| Provider::parse(&toml::Value::String(s.into())).unwrap();
        assert_eq!(p("cargo:token"), Provider::Token);
        assert_eq!(p("cargo:macos-keychain"), Provider::MacosKeychain);
        assert_eq!(
            p("cargo:token-from-stdout pass show x"),
            Provider::TokenFromStdout(vec!["pass".into(), "show".into(), "x".into()])
        );
        assert_eq!(
            p("/bin/my-provider --flag"),
            Provider::External(vec!["/bin/my-provider".into(), "--flag".into()])
        );
    }

    #[test]
    fn external_providers_speak_the_json_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("provider");
        // Greets, reads one request, answers with a token that names the
        // registry it was asked about.
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"v\":[1]}'\nread req\ncase \"$req\" in\n  *'\"kind\":\"get\"'*) echo '{\"Ok\":{\"kind\":\"get\",\"token\":\"tok-for-corp\",\"cache\":\"session\",\"operation_independent\":true}}' ;;\n  *) echo '{\"Err\":{\"kind\":\"other\",\"message\":\"only get\"}}' ;;\nesac\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let provider = Provider::External(vec![script.to_string_lossy().into_owned()]);
        let registry = Registry {
            name: "corp",
            index_url: "sparse+https://corp.example/index/",
        };
        assert_eq!(
            get(&provider, &registry, None, || None).unwrap().as_deref(),
            Some("tok-for-corp")
        );
        let err = store(&provider, &registry, "t").unwrap_err();
        assert!(err.contains("only get"), "{err}");
    }
}
