//! Embed the current checkout commit in the administrative binary.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim_end_matches('\n');
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.to_owned())
}

fn main() {
    for entry in ["HEAD", "refs", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", entry]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let hash = git(&["rev-parse", "--short", "HEAD"])
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=MEMORY_HOOKS_GIT_HASH={hash}");
}
