//! Build script: embed the git short SHA as `SWITCHYARD_GIT_SHA`.
//!
//! Falls back to `"unknown"` when git is unavailable or the source is not
//! inside a repository. No `rerun-if-changed` is emitted for `.git` so
//! incremental builds are not invalidated by unrelated git operations.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=SWITCHYARD_GIT_SHA={sha}");
}
