//! Exposes the omicron sha the rack-init-config dependency resolves to, when it is a
//! git dependency. Empty for a path dependency.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let lock = manifest_dir.join("../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let rev = fs::read_to_string(&lock)
        .ok()
        .and_then(|text| rack_init_config_git_sha(&text))
        .unwrap_or_default();
    println!("cargo:rustc-env=RACK_INIT_CONFIG_OMICRON_REV={rev}");
}

/// The resolved git sha of the rack-init-config package in a Cargo.lock.
fn rack_init_config_git_sha(lock: &str) -> Option<String> {
    let mut in_rack_init_config = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_rack_init_config = false;
        } else if line == "name = \"rack-init-config\"" {
            in_rack_init_config = true;
        } else if in_rack_init_config && let Some(source) = line.strip_prefix("source = \"git+") {
            return source
                .split('#')
                .nth(1)
                .map(|sha| sha.trim_end_matches('"').to_string());
        }
    }
    None
}
