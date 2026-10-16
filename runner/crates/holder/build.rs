//! `TMUXEDO_HOLDER_BUILD`: a fingerprint of the holder's own code (its sources,
//! its Cargo.toml and the workspace Cargo.lock), so a supervisor can tell
//! whether a running holder is the same code as a newly installed binary.
//!
//! The release version changes with every build; this changes only when the
//! holder itself does. tmuxedo for Mac restarts the holder (ending terminal
//! sessions) only when this differs (docs/design/mac-pairing.md §1), so an
//! ordinary update keeps every terminal alive.
//!
//! FNV-1a 64 over (path, bytes) pairs in sorted order: no dependencies, and the
//! same answer on every machine and toolchain for the same inputs.

use std::path::{Path, PathBuf};

fn fnv1a(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= u64::from(*b);
        *h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mut files = vec![];
    rs_files(&root.join("src"), &mut files);
    files.sort();
    files.push(root.join("Cargo.toml"));
    // The workspace lock pins the holder's dependencies (portable-pty, vt100, …).
    let lock = root.join("../../Cargo.lock");
    if lock.exists() {
        files.push(lock);
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap_or(f).to_string_lossy().replace('\\', "/");
        fnv1a(&mut h, rel.as_bytes());
        fnv1a(&mut h, &[0]);
        fnv1a(&mut h, &std::fs::read(f).unwrap_or_default());
        fnv1a(&mut h, &[0]);
        println!("cargo:rerun-if-changed={}", f.display());
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rustc-env=TMUXEDO_HOLDER_BUILD=h{h:016x}");
}
