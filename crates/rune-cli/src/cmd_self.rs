//! `rune self update` and `rune self uninstall`.
//!
//! Downloads the latest (or a requested) release archive from GitHub (or a mirror),
//! checks the SHA-256 against the published checksum, extracts the binary,
//! and atomically replaces the currently running executable.

use crate::args::{SelfUninstallArgs, SelfUpdateArgs};
use crate::ui;
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Updates the Rune binary to the latest or specified release.
pub fn update(args: SelfUpdateArgs) -> Result<()> {
    let target = current_target()?;
    let current_version = env!("CARGO_PKG_VERSION");
    let current_exe = match args.to {
        Some(to) => to,
        None => std::env::current_exe()
            .context("cannot determine current executable path")?
            .canonicalize()
            .unwrap_or_else(|_| std::env::current_exe().unwrap()),
    };

    let repo = args
        .repo
        .as_deref()
        .or(option_env!("RUNE_REPOSITORY"))
        .unwrap_or("runepm/rune");

    // Rune's own network settings (CA bundles, proxy, `http.*`); a whole
    // release must fit in the timeout on a slow connection.
    let client = rune_registry::http::client(
        reqwest::blocking::Client::builder()
            .user_agent(format!("rune/{current_version}"))
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(600)),
    );

    let download_base_env = std::env::var("RUNE_DOWNLOAD_BASE").ok();

    // Determine target release tag and download base
    let (target_tag, base_url) = if let Some(base) = download_base_env {
        let tag = args.version.unwrap_or_else(|| "latest".to_string());
        (tag, base.trim_end_matches('/').to_string())
    } else if let Some(ver) = args.version {
        let tag = if ver.starts_with('v') || ver == "latest" {
            ver
        } else {
            format!("v{ver}")
        };
        let base = if tag == "latest" {
            format!("https://github.com/{repo}/releases/latest/download")
        } else {
            format!("https://github.com/{repo}/releases/download/{tag}")
        };
        (tag, base)
    } else {
        // Query GitHub for the latest release
        ui::action("checking", format!("latest release from {repo}"));
        let (tag, base) = resolve_latest_release(&client, repo)?;
        (tag, base)
    };

    // Check version if target tag has a semver format
    let clean_tag = target_tag.trim_start_matches('v');
    if let Ok(target_semver) = semver::Version::parse(clean_tag)
        && let Ok(curr_semver) = semver::Version::parse(current_version)
        && target_semver <= curr_semver
        && !args.force
    {
        if args.check {
            ui::action("ok", format!("rune is up to date (v{current_version})"));
        } else {
            ui::action(
                "ok",
                format!("rune is already up to date (v{current_version})"),
            );
        }
        return Ok(());
    }

    if args.check {
        ui::action(
            "update",
            format!("a newer release is available: {target_tag} (current: v{current_version})"),
        );
        ui::hint("run `rune self update` to install it");
        return Ok(());
    }

    // Prepare archive names
    let archive_name = format!("rune-{target}.tar.gz");
    let archive_url = format!("{base_url}/{archive_name}");
    let sha256_url = format!("{base_url}/{archive_name}.sha256");

    ui::action("downloading", format!("Rune {target_tag} ({target})"));

    // Download checksum file
    let sha256_resp = client
        .get(&sha256_url)
        .send()
        .with_context(|| format!("failed to request checksum from {sha256_url}"))?;
    if !sha256_resp.status().is_success() {
        bail!(
            "failed to download checksum: HTTP {} from {}",
            sha256_resp.status(),
            sha256_url
        );
    }
    let sha256_text = sha256_resp
        .text()
        .context("failed to read checksum response")?;
    let expected_hash = sha256_text
        .split_whitespace()
        .next()
        .context("invalid checksum file format")?
        .to_lowercase();

    // Download archive to a temp file
    let mut archive_temp = tempfile::Builder::new()
        .prefix("rune-archive-")
        .suffix(".tar.gz")
        .tempfile()
        .context("failed to create temporary archive file")?;

    let mut archive_resp = client
        .get(&archive_url)
        .send()
        .with_context(|| format!("failed to request archive from {archive_url}"))?;
    if !archive_resp.status().is_success() {
        bail!(
            "failed to download archive: HTTP {} from {}",
            archive_resp.status(),
            archive_url
        );
    }

    let mut buffer = [0u8; 65536];
    loop {
        let n = archive_resp
            .read(&mut buffer)
            .context("failed to read archive stream")?;
        if n == 0 {
            break;
        }
        archive_temp
            .write_all(&buffer[..n])
            .context("failed to write archive data")?;
    }
    archive_temp
        .flush()
        .context("failed to flush archive data")?;

    // Verify SHA-256
    let actual_hash = rune_registry::download::sha256_file(archive_temp.path())
        .context("failed to compute archive checksum")?
        .to_lowercase();

    if expected_hash != actual_hash {
        bail!("checksum mismatch: expected {expected_hash}, got {actual_hash}");
    }
    ui::action("verifying", "checksum matches");

    // Extract archive
    let extract_dir = tempfile::Builder::new()
        .prefix("rune-extract-")
        .tempdir()
        .context("failed to create temporary extraction directory")?;

    let tar_gz = std::fs::File::open(archive_temp.path())
        .context("failed to open downloaded archive for extraction")?;
    let gz = flate2::read::GzDecoder::new(tar_gz);
    let mut archive = tar::Archive::new(gz);
    archive
        .unpack(extract_dir.path())
        .context("failed to unpack release archive")?;

    // Locate binary inside extracted archive
    let bin_name = if cfg!(windows) { "rune.exe" } else { "rune" };
    let expected_bin = extract_dir
        .path()
        .join(format!("rune-{target}"))
        .join(bin_name);

    let new_bin = if expected_bin.exists() {
        expected_bin
    } else {
        // Fallback: search recursively for the binary
        find_file_recursively(extract_dir.path(), bin_name)
            .with_context(|| format!("could not find `{bin_name}` in extracted archive"))?
    };

    // Ensure executable permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new_bin, std::fs::Permissions::from_mode(0o755))
            .context("failed to set executable permissions on unpacked binary")?;
    }

    // Replace current executable
    replace_executable(&new_bin, &current_exe)?;

    ui::action(
        "updated",
        format!(
            "Rune v{current_version} -> {target_tag} ({})",
            current_exe.display()
        ),
    );

    Ok(())
}

/// Uninstalls the Rune binary from the system.
pub fn uninstall(args: SelfUninstallArgs) -> Result<()> {
    let current_exe = std::env::current_exe()
        .context("cannot determine current executable path")?
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_exe().unwrap());

    if !args.yes {
        print!(
            "Are you sure you want to uninstall Rune from {}? [y/N] ",
            current_exe.display()
        );
        std::io::stdout().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            ui::action("cancelled", "uninstallation aborted");
            return Ok(());
        }
    }

    // Remove the binary
    if current_exe.exists() {
        std::fs::remove_file(&current_exe)
            .with_context(|| format!("failed to remove binary `{}`", current_exe.display()))?;
        ui::action("removed", current_exe.display());
    }

    // Purge configuration and cache if requested
    let rune_home = rune_common::settings::rune_home();
    if args.purge {
        if rune_home.exists() && !looks_like_rune_home(&rune_home) {
            bail!(
                "not removing `{}`: it does not look like Rune's own directory \
                 (check RUNE_HOME); remove what you want gone by hand",
                rune_home.display()
            );
        }
        if rune_home.exists() {
            std::fs::remove_dir_all(&rune_home).with_context(|| {
                format!(
                    "failed to remove configuration directory `{}`",
                    rune_home.display()
                )
            })?;
            ui::action(
                "removed",
                format!("{} (cache and configuration)", rune_home.display()),
            );
        }
    } else {
        ui::action(
            "kept",
            format!(
                "the store, caches and settings in {} (`--purge` removes them)",
                rune_home.display()
            ),
        );
    }

    ui::action("uninstalled", "Rune has been removed from your system");
    Ok(())
}

/// Whether `dir` is a directory `--purge` may delete: one Rune made (it
/// holds Rune's store, caches or settings), and never the home directory or
/// a file system root, whatever `RUNE_HOME` says.
fn looks_like_rune_home(dir: &Path) -> bool {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if canonical.parent().is_none() {
        return false;
    }
    if let Some(home) = home::home_dir()
        && canonical == home.canonicalize().unwrap_or(home)
    {
        return false;
    }
    [
        "cas",
        "registry",
        "cache",
        "config.toml",
        "bin",
        "git",
        "advisory-db",
    ]
    .iter()
    .any(|entry| canonical.join(entry).exists())
}

/// Resolves the latest release tag and download base URL.
fn resolve_latest_release(
    client: &reqwest::blocking::Client,
    repo: &str,
) -> Result<(String, String)> {
    let api_url = format!("https://api.github.com/repos/{repo}/releases/latest");
    if let Ok(resp) = client
        .get(&api_url)
        .header(reqwest::header::ACCEPT, "application/vnd.github.v3+json")
        .send()
        && resp.status().is_success()
        && let Ok(json) = resp.json::<serde_json::Value>()
        && let Some(tag) = json.get("tag_name").and_then(|v| v.as_str())
    {
        let base = format!("https://github.com/{repo}/releases/download/{tag}");
        return Ok((tag.to_string(), base));
    }

    // Fallback: check redirect location of releases/latest
    let redirect_url = format!("https://github.com/{repo}/releases/latest");
    if let Ok(resp) = client.get(&redirect_url).send() {
        let final_url = resp.url().as_str();
        if let Some(pos) = final_url.rfind("/tag/") {
            let tag = &final_url[pos + 5..];
            let tag = tag.trim_end_matches('/');
            if !tag.is_empty() {
                let base = format!("https://github.com/{repo}/releases/download/{tag}");
                return Ok((tag.to_string(), base));
            }
        }
    }

    // Ultimate fallback to releases/latest/download
    let base = format!("https://github.com/{repo}/releases/latest/download");
    Ok(("latest".to_string(), base))
}

/// Detects the target triple for the current machine.
fn current_target() -> Result<String> {
    let build_target = env!("RUNE_BUILD_TARGET");
    if !build_target.is_empty() {
        return Ok(build_target.to_string());
    }

    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => bail!("unsupported architecture: {other}"),
    };

    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        "windows" => "pc-windows-msvc",
        other => bail!("unsupported operating system: {other}"),
    };

    Ok(format!("{arch}-{os}"))
}

/// Recursively searches for a file by name.
fn find_file_recursively(dir: &Path, file_name: &str) -> Option<PathBuf> {
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_name() == file_name && entry.file_type().is_file() {
            return Some(entry.into_path());
        }
    }
    None
}

/// Atomically replaces the destination executable with the source file.
fn replace_executable(src: &Path, dst: &Path) -> Result<()> {
    let parent = dst
        .parent()
        .context("destination executable has no parent directory")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let staging = parent.join(format!(".rune-update-{}.tmp", std::process::id()));
        std::fs::copy(src, &staging).with_context(|| {
            format!(
                "cannot copy new binary to staging path `{}`",
                staging.display()
            )
        })?;

        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).ok();

        if let Err(e) = std::fs::rename(&staging, dst) {
            let _ = std::fs::remove_file(&staging);
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                bail!(
                    "permission denied updating `{}`: you may need to run `sudo rune self update`",
                    dst.display()
                );
            }
            return Err(e).with_context(|| format!("failed to replace `{}`", dst.display()));
        }
    }

    #[cfg(windows)]
    {
        let pid = std::process::id();
        let backup = parent.join(format!(".rune-old-{pid}.exe"));
        let staging = parent.join(format!(".rune-new-{pid}.exe"));

        std::fs::copy(src, &staging).with_context(|| {
            format!(
                "cannot copy new binary to staging path `{}`",
                staging.display()
            )
        })?;

        if dst.exists() {
            std::fs::rename(dst, &backup).with_context(|| {
                format!(
                    "cannot rename active executable `{}` to backup `{}`",
                    dst.display(),
                    backup.display()
                )
            })?;
        }

        if let Err(e) = std::fs::rename(&staging, dst) {
            if backup.exists() {
                let _ = std::fs::rename(&backup, dst);
            }
            let _ = std::fs::remove_file(&staging);
            return Err(e)
                .with_context(|| format!("failed to install new binary to `{}`", dst.display()));
        }

        let _ = std::fs::remove_file(&backup);
    }

    #[cfg(not(any(unix, windows)))]
    {
        std::fs::copy(src, dst)
            .with_context(|| format!("cannot copy new binary to `{}`", dst.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_only_deletes_a_directory_rune_made() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!looks_like_rune_home(dir.path()), "an empty directory");
        std::fs::create_dir(dir.path().join("cas")).unwrap();
        assert!(looks_like_rune_home(dir.path()));
        assert!(!looks_like_rune_home(Path::new("/")));
        if let Some(home) = home::home_dir() {
            assert!(!looks_like_rune_home(&home), "the home directory");
        }
    }
}
