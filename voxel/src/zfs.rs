// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The zfs operations voxel performs on image datasets, through pfexec.
//! Dataset names are validated before they reach `destroy -r`.

use anyhow::{Context, Result, bail};
use std::process::Command;

/// A zfs dataset voxel may operate on, validated on construction. These names
/// feed `pfexec zfs destroy -r`; a truncated one would widen to the parent.
pub(crate) struct Dataset(String);

impl Dataset {
    pub(crate) fn new(path: String) -> Result<Self> {
        let components: Vec<&str> = path.split('/').collect();
        if components.len() < 3
            || components.iter().any(|c| c.is_empty())
            || path.contains(|c: char| c.is_whitespace())
            || path.contains('@')
        {
            bail!(
                "refusing to operate on zfs dataset {path:?}: expected a \
                 <pool>/<...>/<name> path with no empty components"
            );
        }
        Ok(Self(path))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The only constructor for a snapshot name. Every destructive verb on a
    /// snapshot therefore starts from a validated dataset.
    pub(crate) fn snapshot(&self, name: &str) -> Snapshot {
        Snapshot(format!("{}@{name}", self.0))
    }
}

/// `<dataset>@<name>`, made by [`Dataset::snapshot`].
pub(crate) struct Snapshot(String);

impl Snapshot {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn exists(&self) -> bool {
        zfs_exists(&self.0)
    }
}

/// `pfexec zfs <args>`, failing with zfs's own stderr.
fn zfs(args: &[&str]) -> Result<()> {
    let out = Command::new("pfexec")
        .arg("zfs")
        .args(args)
        .output()
        .with_context(|| format!("run zfs {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "zfs {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Whether a dataset or snapshot is present.
pub(crate) fn zfs_exists(name: &str) -> bool {
    Command::new("zfs")
        .args(["list", "-H", "-o", "name", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(crate) fn exists(ds: &Dataset) -> bool {
    zfs_exists(ds.as_str())
}

pub(crate) fn rename(from: &Dataset, to: &Dataset) -> Result<()> {
    zfs(&["rename", from.as_str(), to.as_str()])
}

/// `zfs destroy -r` of a dataset and its snapshots, skipped when absent.
/// Never `-R`. A snapshot another dataset was cloned from makes the destroy
/// fail instead of taking the clone with it.
pub(crate) fn destroy_dataset_if_present(ds: &Dataset) -> Result<()> {
    if !exists(ds) {
        return Ok(());
    }
    zfs(&["destroy", "-r", ds.as_str()])
}

/// `zfs destroy -r` of one snapshot, skipped when absent. Fails while a
/// clone depends on it.
pub(crate) fn destroy_snapshot_if_present(snap: &Snapshot) -> Result<()> {
    if !snap.exists() {
        return Ok(());
    }
    zfs(&["destroy", "-r", snap.as_str()])
}

pub(crate) fn snapshot(ds: &Dataset, name: &str) -> Result<()> {
    zfs(&["snapshot", ds.snapshot(name).as_str()])
}

/// Stream a snapshot into a new dataset with a full send. The result carries
/// no dependency on the base it was cloned from.
pub(crate) fn send_recv(from: &Snapshot, to: &Dataset) -> Result<()> {
    let send = Command::new("pfexec")
        .args(["zfs", "send", from.as_str()])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("spawn zfs send")?;
    let recv = Command::new("pfexec")
        .args(["zfs", "recv", to.as_str()])
        .stdin(send.stdout.ok_or_else(|| anyhow::anyhow!("zfs send stdout"))?)
        .status()
        .context("run zfs recv")?;
    if !recv.success() {
        bail!("zfs send {} | zfs recv {} failed", from.as_str(), to.as_str());
    }
    Ok(())
}
