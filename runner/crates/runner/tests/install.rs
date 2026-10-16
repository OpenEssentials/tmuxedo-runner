//! runner/install.sh as a real `sh` process against a local `/dl/` (the
//! same layout the downloads image serves): with `--no-enrol --no-service`
//! it installs the binary for this OS/arch after checking its sha256
//! against manifest.json, runs `tmuxedo-runner version`, and never calls the
//! API; a checksum mismatch is refused and nothing is installed.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");
const INSTALL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.sh");

fn key() -> String {
    let os = if cfg!(target_os = "macos") { "darwin" } else { "linux" };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    format!("{os}-{arch}")
}

/// Serves `<dir>/<path>` for GET /dl/<path>; records every request path.
fn serve(dir: &Path) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(vec![]));
    let (seen2, dir) = (seen.clone(), dir.to_path_buf());
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            let _ = r.read_line(&mut line);
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            seen2.lock().unwrap().push(path.clone());
            let body = path.strip_prefix("/dl/").and_then(|p| std::fs::read(dir.join(p)).ok());
            let _ = match body {
                Some(b) => write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    b.len()
                )
                .and_then(|_| s.write_all(&b)),
                None => write!(
                    s,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            };
        }
    });
    (url, seen)
}

fn manifest(dir: &Path, sha: &str) {
    std::fs::write(
        dir.join("manifest.json"),
        format!(
            "{{\n  \"version\": \"main-r000099-test\",\n  \"files\": {{\n    \"{}\": {{\"url\": \"tmuxedo-runner-{}\", \"sha256\": \"{sha}\"}}\n  }}\n}}\n",
            key(),
            key()
        ),
    )
    .unwrap();
}

fn install(server: &str, home: &Path, args: &[&str]) -> std::process::Output {
    let mut script = String::new();
    std::fs::File::open(INSTALL)
        .unwrap()
        .read_to_string(&mut script)
        .unwrap();
    // As `curl … | sh -s -- <args>`.
    let mut child = Command::new("sh")
        .arg("-s")
        .arg("--")
        .args(args)
        .env("TMUXEDO_SERVER", server)
        .env("HOME", home)
        .env("TMUXEDO_BIN_DIR", home.join("bin"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn installer_verifies_and_installs_without_enrolling() {
    let dl = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let bin = std::fs::read(BIN).unwrap();
    std::fs::write(dl.path().join(format!("tmuxedo-runner-{}", key())), &bin).unwrap();
    manifest(dl.path(), &tmuxedo_runner::util::sha256_hex(&bin));
    let (url, seen) = serve(dl.path());

    let out = install(&url, home.path(), &["--no-enrol", "--no-service"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    let installed = home.path().join("bin/tmuxedo-runner");
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        bin,
        "the verified binary is installed"
    );
    assert!(err.contains("main-r000099-test"), "{err}");
    assert!(
        err.contains("skipped enrolment") && err.contains("skipped the service install"),
        "{err}"
    );
    let paths = seen.lock().unwrap().clone();
    assert!(paths.iter().all(|p| p.starts_with("/dl/")), "no API calls: {paths:?}");

    // A tampered binary is refused and does not replace the installed one.
    std::fs::write(
        dl.path().join(format!("tmuxedo-runner-{}", key())),
        b"#!/bin/sh\necho evil\n",
    )
    .unwrap();
    let out = install(&url, home.path(), &["--no-enrol", "--no-service"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"));
    assert_eq!(std::fs::read(&installed).unwrap(), bin);

    let out = install(&url, home.path(), &["--bogus"]);
    assert_eq!(out.status.code(), Some(2));

    // The CLI and the deprecated names are symlinks to it.
    for alias in ["tmuxedo", "aimux-runner", "aimux"] {
        let p = home.path().join("bin").join(alias);
        assert_eq!(std::fs::read_link(&p).unwrap(), Path::new("tmuxedo-runner"), "{alias}");
    }
}

#[test]
fn installer_upgrades_an_aimux_era_install() {
    let dl = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let bin = std::fs::read(BIN).unwrap();
    std::fs::write(dl.path().join(format!("tmuxedo-runner-{}", key())), &bin).unwrap();
    manifest(dl.path(), &tmuxedo_runner::util::sha256_hex(&bin));
    let (url, _) = serve(dl.path());
    let bindir = home.path().join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    std::fs::write(bindir.join("aimux-runner"), b"old binary").unwrap();

    // The deprecated AIMUX_* names still steer the installer.
    let mut script = String::new();
    std::fs::File::open(INSTALL)
        .unwrap()
        .read_to_string(&mut script)
        .unwrap();
    let mut child = Command::new("sh")
        .args(["-s", "--", "--no-enrol", "--no-service"])
        .env("AIMUX_SERVER", &url)
        .env("HOME", home.path())
        .env("AIMUX_BIN_DIR", &bindir)
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    assert_eq!(std::fs::read(bindir.join("tmuxedo-runner")).unwrap(), bin);
    for alias in ["aimux-runner", "tmuxedo", "aimux"] {
        let p = bindir.join(alias);
        assert!(p.is_symlink(), "{alias} is a symlink");
        assert_eq!(std::fs::read(&p).unwrap(), bin, "{alias} runs the new binary");
    }
    // Through the deprecated name the binary is still the runner.
    let v = Command::new(bindir.join("aimux-runner"))
        .arg("version")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&v.stdout).starts_with("tmuxedo-runner "),
        "{v:?}"
    );
}
