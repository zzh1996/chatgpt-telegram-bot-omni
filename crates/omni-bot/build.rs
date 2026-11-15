//! The git commit the bot is built from, for `/ping` (`OMNI_GIT_COMMIT`).
//!
//! In order: `OMNI_GIT_COMMIT` from the environment (a build without `.git`);
//! the commit `.git/HEAD` points to, read from the files (the Docker build has
//! no `git` program; `.dockerignore` lets `.git/HEAD` and the refs through),
//! with `-dirty` when `git` is there and reports uncommitted changes to
//! tracked files; else `unknown`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=OMNI_GIT_COMMIT");
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let version = match std::env::var("OMNI_GIT_COMMIT").ok().filter(|v| !v.trim().is_empty()) {
        Some(v) => v.trim().to_string(),
        None => from_git(&root).unwrap_or_else(|| "unknown".into()),
    };
    println!("cargo:rustc-env=OMNI_GIT_COMMIT={version}");
}

/// The short commit of `HEAD`, read from `.git` without the `git` program.
fn from_git(root: &Path) -> Option<String> {
    let git = root.join(".git");
    let head_path = git.join("HEAD");
    println!("cargo:rerun-if-changed={}", head_path.display());
    let head = std::fs::read_to_string(&head_path).ok()?;
    let head = head.trim();
    let commit = match head.strip_prefix("ref: ") {
        Some(r) => {
            // A branch: its ref file, or its line in packed-refs.
            let ref_path = git.join(r);
            let packed = git.join("packed-refs");
            println!("cargo:rerun-if-changed={}", ref_path.display());
            println!("cargo:rerun-if-changed={}", packed.display());
            match std::fs::read_to_string(&ref_path) {
                Ok(c) => c.trim().to_string(),
                Err(_) => std::fs::read_to_string(&packed)
                    .ok()?
                    .lines()
                    .find_map(|l| l.strip_suffix(r).map(|c| c.trim().to_string()))?,
            }
        }
        None => head.to_string(),
    };
    if commit.len() < 7 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let short = &commit[..7];
    // Uncommitted changes to tracked files (only where `git` is installed); a
    // source change reruns this check.
    for dir in ["crates", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={}", root.join(dir).display());
    }
    println!("cargo:rerun-if-changed={}", git.join("index").display());
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.is_empty());
    Some(if dirty { format!("{short}-dirty") } else { short.to_string() })
}
