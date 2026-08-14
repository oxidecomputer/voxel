//! Exposes the omicron sha voxel's git dependencies resolve to, and fails the
//! build if the omicron deps pin different revs. Empty for path dependencies.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let lock = manifest_dir.join("../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let text = fs::read_to_string(&lock)
        .unwrap_or_else(|e| panic!("read {}: {e}", lock.display()));
    let revs = omicron_git_shas(&text);
    if revs.len() > 1 {
        panic!(
            "omicron git dependencies resolve to different revs: {revs:?}; \
             pin every omicron dep in Cargo.toml to the same rev"
        );
    }
    // Empty when omicron is a path dependency (no git sha); image-create reads
    // an empty rev as "no pin known" and skips its mismatch warning.
    let rev = revs.into_iter().next().unwrap_or_default();
    println!("cargo:rustc-env=RACK_INIT_CONFIG_OMICRON_REV={rev}");
}

/// Distinct git shas of omicron-sourced packages in a Cargo.lock.
fn omicron_git_shas(lock: &str) -> BTreeSet<String> {
    lock.lines()
        .filter_map(|line| {
            let source = line.trim().strip_prefix("source = \"git+")?;
            if !source.contains("/omicron") {
                return None;
            }
            source
                .split('#')
                .nth(1)
                .map(|sha| sha.trim_end_matches('"').to_string())
        })
        .collect()
}
