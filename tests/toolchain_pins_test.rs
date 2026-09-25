#![allow(
    missing_docs,
    clippy::expect_used,
    reason = "Intentional compatibility, platform, test, or API-shape suppression."
)]
//! Guards that the repository pins exactly one Rust toolchain everywhere.
//!
//! `rust-toolchain.toml` is the single source of truth. Any other manifest or
//! build file that pins a different version silently reintroduces a second
//! toolchain: `cargo` would activate one compiler while CI, Docker, Clippy, or
//! `cargo-msrv` reason about another. This test fails loudly when a pin drifts,
//! complementing `scripts/check_compatibility.sh --msrv` (the human-facing
//! report) with an always-on `cargo nextest run` guard.

use std::fs;
use std::path::{Path, PathBuf};

/// Directories whose Cargo manifests are intentionally not pinned to the
/// workspace channel (vendored forks, fuzz harnesses, editor extensions).
const EXCLUDED_DIRS: &[&str] = &[
    "target",
    ".worktrees",
    "patches",
    "fuzz",
    "extensions",
    ".git",
    "node_modules",
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Extract the value of a `key = "value"` TOML line, ignoring whitespace.
fn toml_string_value(contents: &str, key: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let trimmed = line.trim_start();
        let rest = trimmed.strip_prefix(key)?.trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        Some(rest.trim_matches(|c| c == '"' || c == '\'').to_string())
    })
}

/// The pinned channel from `rust-toolchain.toml`.
fn pinned_toolchain(root: &Path) -> String {
    let manifest = fs::read_to_string(root.join("rust-toolchain.toml")).expect("read rust-toolchain.toml");
    let channel = toml_string_value(&manifest, "channel").expect("rust-toolchain.toml must declare `channel`");
    assert!(
        channel.split('.').all(|part| part.chars().all(|c| c.is_ascii_digit())),
        "rust-toolchain.toml `channel` must be a concrete x.y.z version, got {channel:?}"
    );
    channel
}

fn collect_cargo_manifests(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if EXCLUDED_DIRS.contains(&name.as_ref()) {
                continue;
            }
            collect_cargo_manifests(&path, found);
        } else if file_type.is_file() && entry.file_name() == "Cargo.toml" {
            found.push(path);
        }
    }
}

/// Every workspace manifest that pins `rust-version` directly.
fn rust_version_pins(root: &Path) -> Vec<(PathBuf, String)> {
    let mut manifests = Vec::new();
    collect_cargo_manifests(root, &mut manifests);

    let mut pins = Vec::new();
    for manifest in manifests {
        let contents = fs::read_to_string(&manifest).expect("read Cargo.toml");
        // `rust-version.workspace = true` inherits the workspace pin and is
        // checked through the workspace manifest itself.
        if contents.contains("rust-version.workspace = true") {
            continue;
        }
        if let Some(version) = toml_string_value(&contents, "rust-version") {
            pins.push((manifest, version));
        }
    }
    assert!(pins.len() > 10, "expected the workspace to pin rust-version in many manifests");
    pins
}

/// Cases where a file embeds the toolchain version in a non-TOML format,
/// described as `(relative path, human label, extractor)`.
fn non_manifest_pins(root: &Path) -> Vec<(&'static str, String, String)> {
    let mut pins = Vec::new();

    let clippy = fs::read_to_string(root.join("clippy.toml")).expect("read clippy.toml");
    pins.push((
        "clippy.toml",
        "msrv".to_string(),
        toml_string_value(&clippy, "msrv").expect("clippy.toml must set `msrv`"),
    ));

    let dockerfile = fs::read_to_string(root.join("Dockerfile.build")).expect("read Dockerfile.build");
    let rust_base = dockerfile
        .lines()
        .find_map(|line| line.strip_prefix("FROM rust:"))
        .expect("Dockerfile.build must start from a rust image");
    let docker_version = rust_base
        .split_once('-')
        .map_or(rust_base, |(version, _)| version)
        .trim()
        .to_string();
    pins.push(("Dockerfile.build", "FROM rust".to_string(), docker_version));

    let ci = fs::read_to_string(root.join(".github/workflows/ci.yml")).expect("read ci.yml");
    let msrv_job = ci
        .split("msrv:")
        .nth(1)
        .expect("ci.yml must define an `msrv` job")
        .split("\n  nightly:")
        .next()
        .expect("msrv job must precede the nightly job");
    let ci_version = msrv_job
        .lines()
        .filter_map(|line| line.trim().strip_prefix("toolchain:"))
        .map(|value| value.trim().trim_matches('"').to_string())
        .next()
        .expect("ci.yml msrv job must pin a toolchain");
    pins.push((".github/workflows/ci.yml (msrv job)", "toolchain".to_string(), ci_version));

    pins
}

#[test]
fn every_rust_toolchain_pin_agrees() {
    let root = workspace_root();
    let pinned = pinned_toolchain(&root);

    let mut mismatches = rust_version_pins(&root)
        .into_iter()
        .filter(|(_, version)| version != &pinned)
        .map(|(path, version)| {
            let relative = path.strip_prefix(&root).unwrap_or(&path);
            format!("{} pins {version}", relative.display())
        })
        .collect::<Vec<_>>();

    mismatches.extend(
        non_manifest_pins(&root)
            .into_iter()
            .filter(|(_, _, version)| version != &pinned)
            .map(|(path, label, version)| format!("{path} {label} = {version}")),
    );

    assert!(
        mismatches.is_empty(),
        "rust-toolchain.toml pins {pinned}, but these disagree:\n  {}",
        mismatches.join("\n  ")
    );
}

#[test]
fn workspace_package_inherits_the_pinned_toolchain() {
    let root = workspace_root();
    let pinned = pinned_toolchain(&root);
    let workspace_manifest = fs::read_to_string(root.join("Cargo.toml")).expect("read workspace Cargo.toml");

    let rust_version =
        toml_string_value(&workspace_manifest, "rust-version").expect("workspace.package must declare rust-version");
    assert_eq!(rust_version, pinned, "workspace.package rust-version must match the pinned toolchain");

    let edition = toml_string_value(&workspace_manifest, "edition").expect("workspace.package must declare edition");
    assert_eq!(edition, "2024", "the workspace targets Rust edition 2024");
}
