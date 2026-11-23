// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `voxel-init builder`: launch-time bring-up of the build host, run by
//! `voxel builder up` from the staged agent at every boot. Network, root ssh,
//! an optional checkout switch, and the record `voxel builder status` reads.

use crate::sys::{bring_up_network, capture, note, run, run_quiet, setup_ssh};
use anyhow::{Result, bail};
use std::fs;

const CARGO_BAY: &str = "/opt/cargo-bay";
const SRC: &str = "/work/omicron";
const RECORD: &str = "/opt/oxide/voxel/builder.toml";

pub fn bring_up() -> Result<()> {
    // Delete the address object baked into the image. The external mode at
    // launch may differ from the bake's.
    run_quiet("ipadm", &["delete-addr", "vioif0/v4"]);
    bring_up_network()?;
    setup_ssh();
    // The cargo-bay pull adds files and never removes them. The request is
    // deleted after use to keep it from reapplying at every boot.
    let rev_file = format!("{CARGO_BAY}/omicron-rev");
    if let Ok(rev) = fs::read_to_string(&rev_file) {
        let result = checkout(rev.trim());
        let _ = fs::remove_file(&rev_file);
        let _ = fs::remove_file(format!("{CARGO_BAY}/omicron-force"));
        result?;
    }
    write_record()?;
    let ip = capture("ipadm", &["show-addr", "-p", "-o", "addr", "vioif0/v4"])
        .unwrap_or_default();
    note(format!("builder ready at {ip}"));
    Ok(())
}

/// Switch the checkout to `rev`. Refused on a dirty tree unless the host
/// staged `omicron-force`.
fn checkout(rev: &str) -> Result<()> {
    let dirty = capture("git", &["-C", SRC, "status", "--porcelain"])
        .is_some_and(|s| !s.is_empty());
    let force = fs::metadata(format!("{CARGO_BAY}/omicron-force")).is_ok();
    if dirty && !force {
        bail!(
            "the omicron checkout has uncommitted changes; pass --force to \
             discard them or sync them out first"
        );
    }
    note(format!("checking out omicron {rev}"));
    // A fetch failure is tolerated. The rev may already be local.
    let _ = run("git", &["-C", SRC, "fetch", "--all", "--tags", "-q"]);
    if dirty && !run("git", &["-C", SRC, "checkout", "-q", "--", "."]) {
        bail!("discard local changes in {SRC}");
    }
    if !run("git", &["-C", SRC, "checkout", "-q", rev]) {
        bail!("checkout omicron {rev}");
    }
    Ok(())
}

/// `/opt/oxide/voxel/builder.toml`: the checkout's commit and the image this
/// disk came from.
fn write_record() -> Result<()> {
    let commit = capture("git", &["-C", SRC, "rev-parse", "HEAD"])
        .unwrap_or_else(|| "unknown".into());
    let image = fs::read_to_string(format!("{CARGO_BAY}/builder-image"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    fs::create_dir_all("/opt/oxide/voxel")
        .map_err(|e| anyhow::anyhow!("mkdir /opt/oxide/voxel: {e}"))?;
    fs::write(RECORD, format!("omicron = \"{commit}\"\nimage = \"{image}\"\n"))
        .map_err(|e| anyhow::anyhow!("write {RECORD}: {e}"))?;
    Ok(())
}
