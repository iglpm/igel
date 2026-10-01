use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

fn rune_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rune"))
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
                let mut req = [0u8; 1024];
                let _ = stream.read(&mut req);
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
    let target = if !env!("RUNE_BUILD_TARGET").is_empty() {
        env!("RUNE_BUILD_TARGET").to_string()
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

    // Create a mock updated binary inside a folder structure
    let package_dir = tempfile::tempdir().unwrap();
    let inner_dir = package_dir.path().join(format!("rune-{target}"));
    std::fs::create_dir_all(&inner_dir).unwrap();

    let bin_name = if cfg!(windows) { "rune.exe" } else { "rune" };
    let mock_bin = inner_dir.join(bin_name);
    // Write a small executable shell script or file
    let mock_content = b"#!/bin/sh\necho \"rune 9.9.9 (test)\"\n";
    std::fs::write(&mock_bin, mock_content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&mock_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Package into tar.gz
    let archive_name = format!("rune-{target}.tar.gz");
    let archive_path = server_dir.path().join(&archive_name);
    let tar_gz = std::fs::File::create(&archive_path).unwrap();
    let enc = flate2::write::GzEncoder::new(tar_gz, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(format!("rune-{target}"), &inner_dir)
        .unwrap();
    let enc = tar.into_inner().unwrap();
    enc.finish().unwrap();

    // Compute SHA-256 and write .sha256 file
    let hash = rune_registry::download::sha256_file(&archive_path).unwrap();
    let sha_path = server_dir.path().join(format!("{archive_name}.sha256"));
    std::fs::write(sha_path, format!("{hash}  {archive_name}\n")).unwrap();

    // Start mock HTTP server
    let (base_url, shutdown_tx) = serve_dir(server_dir.path());

    // Prepare a destination directory with an old dummy binary
    let client_dir = tempfile::tempdir().unwrap();
    let target_dest = client_dir.path().join(bin_name);
    std::fs::write(&target_dest, b"old binary").unwrap();

    // Run `rune self update --to <target_dest> -v v9.9.9` with RUNE_DOWNLOAD_BASE set
    let output = Command::new(rune_bin())
        .args([
            "self",
            "update",
            "--to",
            target_dest.to_str().unwrap(),
            "-v",
            "v9.9.9",
        ])
        .env("RUNE_DOWNLOAD_BASE", &base_url)
        .output()
        .expect("failed to execute rune self update");

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

    // Verify the file was replaced with the mock binary content
    let installed_bytes = std::fs::read(&target_dest).unwrap();
    assert_eq!(installed_bytes, mock_content);
}
