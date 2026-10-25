//! Forces a recompile when the embedded dashboard (`public/`) changes.
//!
//! `src/assets.rs` bakes the whole tree into the binary with
//! `include_dir!("$CARGO_MANIFEST_DIR/public")`, but cargo decides whether a
//! crate needs rebuilding from the files' **mtime**. A dashboard copied in with
//! PRESERVED timestamps — `robocopy /E`, `unzip`, `git checkout`, a release archive —
//! looks older than the last build, so cargo reports the crate as up to date and the
//! STALE embed is reused silently. That is exactly what happened when `public/` was
//! refreshed from `android/pencarimovie-server/app/src/main/assets/web`: the rebuilt
//! APK still served the old (non-ES5) `app.js` and the Android 5 WebView threw
//! `SyntaxError: Unexpected identifier`, so `#settings` / `#addon` could never open.
//!
//! Two rails:
//!   1. `cargo:rerun-if-changed` for the directory and every file in it, so a normal
//!      edit re-runs this script;
//!   2. a fingerprint of (relative path, size, mtime) exported as `rustc-env`. Cargo
//!      folds that value into the crate fingerprint, so even a copy that kept its old
//!      timestamps changes the value and forces the rebuild.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// FNV-1a over every file's path, length and mtime.
fn fold(acc: &mut u64, path: &Path, len: u64, mtime: u64) {
    let name = path.to_string_lossy();
    for b in name
        .as_bytes()
        .iter()
        .chain(len.to_le_bytes().iter())
        .chain(mtime.to_le_bytes().iter())
    {
        *acc ^= *b as u64;
        *acc = acc.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn walk(dir: &Path, acc: &mut u64) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let entries: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => {
            let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            // Stable order so the fingerprint does not depend on directory order.
            v.sort();
            v
        }
        Err(e) => {
            // A missing tree is a hard error: assets.rs would fail to compile anyway,
            // but say WHY instead of a bare include_dir! error.
            panic!("build.rs: cannot read {}: {}", dir.display(), e);
        }
    };
    for p in entries {
        if p.is_dir() {
            walk(&p, acc);
            continue;
        }
        println!("cargo:rerun-if-changed={}", p.display());
        let (len, mtime) = match fs::metadata(&p) {
            Ok(m) => (
                m.len(),
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            ),
            Err(_) => (0, 0),
        };
        fold(acc, &p, len, mtime);
    }
}

/// The build's own version, taken from git so every artifact is traceable.
///
/// Order: `PM_BUILD_VERSION` (release pipelines can pin it) → `git describe
/// --tags --always --dirty` → `CARGO_PKG_VERSION`. `git` is absent inside some
/// container build contexts, so a failure here falls through instead of breaking
/// the build.
fn build_version(manifest_dir: &Path) -> String {
    if let Ok(v) = std::env::var("PM_BUILD_VERSION") {
        let v = v.trim().trim_start_matches('v').to_string();
        if !v.is_empty() {
            return v;
        }
    }
    let described = std::process::Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .current_dir(manifest_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if !described.is_empty() {
        return described.trim_start_matches('v').to_string();
    }
    std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0-dev".to_string())
}

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The crate is the repository root, so `public/` is a sibling. It MUST match
    // include_dir!() in src/assets.rs.
    let public = manifest.join("public");
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
    walk(&public, &mut acc);
    println!("cargo:rerun-if-changed=build.rs");
    // A new COMMIT must move the version string as well. Without these, this
    // script does not re-run for a source-only commit, so `PM_BUILD_VERSION` keeps
    // reporting the commit that was HEAD the last time build.rs ran — a freshly
    // built binary then looks identical to the old one in `/api/version`.
    // (Measured while deploying: the new binary served the new stream rows while
    // still claiming `2.9.9-4-g9750c9f`.)
    for marker in [".git/HEAD", ".git/refs/heads", ".git/packed-refs"] {
        if manifest.join(marker).exists() {
            println!("cargo:rerun-if-changed={}", marker);
        }
    }
    println!("cargo:rustc-env=PM_PUBLIC_FINGERPRINT={:016x}", acc);
    // Exported as the engine version *and* as a fingerprint input: a new commit
    // must change the version string even when no tracked file was touched.
    let version = build_version(manifest);
    println!("cargo:rustc-env=PM_BUILD_VERSION={}", version);
    println!("cargo:rerun-if-env-changed=PM_BUILD_VERSION");
}
