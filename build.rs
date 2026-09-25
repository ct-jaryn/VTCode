#![allow(
    missing_docs,
    reason = "Intentional compatibility, platform, or test-only suppression."
)]

fn main() {
    let git_output = std::process::Command::new("git").args(["rev-parse", "--git-dir"]).output().ok();
    let git_dir = git_output.as_ref().and_then(|output| {
        std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(|s| s.strip_suffix('\n').or_else(|| s.strip_suffix("\r\n")))
    });

    // Tell cargo to rebuild if the head or any relevant refs change.
    if let Some(git_dir) = git_dir {
        let git_path = std::path::Path::new(git_dir);
        let refs_path = git_path.join("refs");
        if git_path.join("HEAD").exists() {
            println!("cargo:rerun-if-changed={git_dir}/HEAD");
        }
        if git_path.join("packed-refs").exists() {
            println!("cargo:rerun-if-changed={git_dir}/packed-refs");
        }
        if refs_path.join("heads").exists() {
            println!("cargo:rerun-if-changed={git_dir}/refs/heads");
        }
        if refs_path.join("tags").exists() {
            println!("cargo:rerun-if-changed={git_dir}/refs/tags");
        }
    }

    let git_output = std::process::Command::new("git")
        .args(["describe", "--always", "--tags", "--long", "--dirty"])
        .output()
        .ok();
    let git_info = git_output
        .as_ref()
        .and_then(|output| std::str::from_utf8(&output.stdout).ok().map(str::trim));
    let cargo_pkg_version = env!("CARGO_PKG_VERSION");

    // Default git_describe to cargo_pkg_version
    let mut git_describe = String::from(cargo_pkg_version);

    if let Some(git_info) = git_info {
        // If the `git_info` contains `CARGO_PKG_VERSION`, we simply use `git_info` as it is.
        // Otherwise, prepend `CARGO_PKG_VERSION` to `git_info`.
        if git_info.contains(cargo_pkg_version) {
            // Remove the 'g' before the commit sha
            let git_info = &git_info.replace('g', "");
            git_describe = git_info.to_string();
        } else {
            git_describe = format!("v{cargo_pkg_version}-{git_info}");
        }
    }

    println!("cargo:rustc-env=VT_CODE_GIT_INFO={git_describe}");

    // macOS: the `vtcode` binary's `__eh_frame` exceeds ld64's 16 MiB
    // compact-unwind limit, so every dev link warns `__eh_frame section too
    // large ... performance of exception handling might be affected`
    // (rust-lang/rust#159105). Passing `-no_compact_unwind` selects the DWARF
    // fallback explicitly — the same fallback ld uses after warning — so the
    // warning disappears with no behavior change. Release builds use
    // `panic = "abort"` and strip symbols, so unwind tables are unaffected.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-no_compact_unwind");
    }
}
