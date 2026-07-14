//! Build-time bootstrap for the project's git hooks.
//!
//! Client-side hooks can't be truly mandatory (a fresh clone never runs versioned
//! hooks, and `--no-verify` bypasses them — that's what CI + branch protection are
//! for). This just removes the friction: on the first `cargo build` in a clone it
//! points git at the versioned `.githooks/` directory, so contributors get the
//! pre-commit checks without installing the external `pre-commit` tool or running
//! any manual setup step.
//!
//! It is idempotent and strictly best-effort: it never fails the build.

use std::path::Path;
use std::process::Command;

const HOOKS_DIR: &str = ".githooks";

fn main() {
    // Only re-run when this script changes, so it doesn't run on every build.
    println!("cargo:rerun-if-changed=build.rs");

    // Nothing to wire up outside a git work tree (published crate, vendored source).
    // `.git` is a directory in a normal clone and a file in a linked worktree.
    if !Path::new(".git").exists() {
        return;
    }

    // Skip the write if git is already pointed at our hooks dir, to avoid log noise.
    let current = Command::new("git")
        .args(["config", "--local", "core.hooksPath"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());

    if current.as_deref() == Some(HOOKS_DIR) {
        return;
    }

    let installed = Command::new("git")
        .args(["config", "--local", "core.hooksPath", HOOKS_DIR])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    if installed {
        println!("cargo:warning=git hooks enabled (core.hooksPath -> {HOOKS_DIR})");
    }
}
