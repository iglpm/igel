//! `igl self update` against a local download server. (Not named `self_update`: Windows treats
//! programs named "update"/"setup"/"install" as installers needing elevation.)

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

fn igel_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_igl"))
}

fn serve_dir(dir: &Path) -> (String, std::sync::mpsc::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let dir = dir.to_path_buf();

    std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        loop {
            if rx.try_recv().is_ok() {
                break;
            }
            if let Ok((mut stream, _)) = listener.accept() {
                // On Windows an accepted socket inherits the listener's non-blocking mode.
                stream.set_nonblocking(false).unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let req_str = String::from_utf8_lossy(&req);
                let first_line = req_str.lines().next().unwrap_or("");
                let path = first_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/');
                let file_path = dir.join(path);
                if file_path.exists() && file_path.is_file() {
                    let content = std::fs::read(&file_path).unwrap();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        content.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.write_all(&content);
                } else {
                    let response =
                        "HTTP/1.1 404 NOT FOUND\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes());
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });

    (format!("http://127.0.0.1:{port}"), tx)
}

#[test]
fn test_self_update_lifecycle() {
    let server_dir = tempfile::tempdir().unwrap();
    let target = if !env!("IGEL_BUILD_TARGET").is_empty() {
        env!("IGEL_BUILD_TARGET").to_string()
    } else {
        let arch = std::env::consts::ARCH;
        let os = match std::env::consts::OS {
            "macos" => "apple-darwin",
            "linux" => "unknown-linux-gnu",
            "windows" => "pc-windows-msvc",
            other => panic!("unsupported os: {other}"),
        };
        format!("{arch}-{os}")
    };

    let package_dir = tempfile::tempdir().unwrap();
    let inner_dir = package_dir.path().join(format!("igel-{target}"));
    std::fs::create_dir_all(&inner_dir).unwrap();

    let bin_name = if cfg!(windows) { "igl.exe" } else { "igl" };
    let mock_bin = inner_dir.join(bin_name);
    let mock_content = b"#!/bin/sh\necho \"igl 9999.9.9 (test)\"\n";
    std::fs::write(&mock_bin, mock_content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&mock_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let archive_name = format!("igel-{target}.tar.gz");
    let archive_path = server_dir.path().join(&archive_name);
    let tar_gz = std::fs::File::create(&archive_path).unwrap();
    let enc = flate2::write::GzEncoder::new(tar_gz, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(format!("igel-{target}"), &inner_dir)
        .unwrap();
    let enc = tar.into_inner().unwrap();
    enc.finish().unwrap();

    let hash = igel_registry::download::sha256_file(&archive_path).unwrap();
    let sha_path = server_dir.path().join(format!("{archive_name}.sha256"));
    std::fs::write(sha_path, format!("{hash}  {archive_name}\n")).unwrap();

    let (base_url, shutdown_tx) = serve_dir(server_dir.path());

    let client_dir = tempfile::tempdir().unwrap();
    let target_dest = client_dir.path().join(bin_name);
    std::fs::write(&target_dest, b"old binary").unwrap();

    let output = Command::new(igel_bin())
        .args([
            "self",
            "update",
            "--to",
            target_dest.to_str().unwrap(),
            "-v",
            "v9999.9.9",
        ])
        .env("IGEL_DOWNLOAD_BASE", &base_url)
        .output()
        .expect("failed to execute igl self update");

    let _ = shutdown_tx.send(());

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Command failed:\nstdout: {stdout}\nstderr: {stderr}"
    );

    assert!(stderr.contains("downloading"), "stderr: {stderr}");
    assert!(stderr.contains("checksum matches"), "stderr: {stderr}");
    assert!(stderr.contains("updated"), "stderr: {stderr}");

    let installed_bytes = std::fs::read(&target_dest).unwrap();
    assert_eq!(installed_bytes, mock_content);
}
