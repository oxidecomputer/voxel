// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Real NVMe disks for sled nodes, backed by host zvols.
//!
//! Each sled gets the gimlet complement (2 M.2, 5 U.2) as propolis `NvmeDisk`
//! devices instead of file-backed vdevs in the guest. omicron then treats them
//! as `RawDisk::Real`: it partitions the U.2s itself, reads the M.2 boot image
//! from a real slice, and none of the `SyntheticDisk` code path runs.
//!
//! Nothing here needs an omicron change. sled-agent's `ExternalDisks::Hardcoded`
//! carries a `disks: Vec<UnparsedDisk>` list that `poll_device_tree` injects on
//! the `NotAnOxideSled` path, which is the path a voxel sled already takes.
//!
//! The disk's identity rides in its NVMe serial number, which the guest reads
//! back via `nvmeadm`. That keeps voxel-init free of any shared layout table:
//! it discovers what a disk is by asking the disk. See [`DiskDesc::serial`].

use anyhow::{Context, Result, bail};
use libfalcon::{NodeRef, Runner};
use propolis_client::instance_spec::{
    ComponentV0, FileStorageBackend, NvmeDisk, PciPath, SpecKey,
};
use voxel_config::SledDesc;

/// Falcon puts a node's boot disk at PCI device 4 and allocates the components
/// it adds itself from 5 upward (boot iso, p9fs mounts, SoftNPU, each NIC), so
/// start clear of anything it can reach on a sled.
const FIRST_PCI_DEV: u8 = 16;

/// Gimlet-sized. The M.2s hold two repos of update artifacts (composites and
/// their derived splits are both retained), which 20 GiB pools run out of
/// mid-upgrade. Zvols are sparse, so this is a ceiling and not an allocation.
const M2_SIZE_GB: u64 = 32;
const U2_SIZE_GB: u64 = 20;

/// 4 KiB LBA, matching the M.2s and U.2s in a real gimlet.
const BLOCK_SIZE: u32 = 4096;

const M2_COUNT: usize = 2;
const U2_COUNT: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    M2,
    U2,
}

impl Variant {
    /// The tag in an NVMe serial number. voxel-init matches on these.
    fn tag(self) -> &'static str {
        match self {
            Self::M2 => "M2",
            Self::U2 => "U2",
        }
    }
}

/// One disk on one sled.
pub(crate) struct DiskDesc {
    pub(crate) variant: Variant,
    /// Index within the variant: M.2 0/1 are slots A/B, U.2 0..4 are bays.
    pub(crate) index: usize,
    pub(crate) size_gb: u64,
    pci_dev: u8,
}

impl DiskDesc {
    /// The NVMe serial number, and so also the `DiskIdentity.serial` the guest
    /// puts in its sled-agent config. Unique rack-wide (sled serials are), and
    /// self-describing so `nvmeadm list` in a wedged guest still says which
    /// disk is which. Must fit NVMe's 20 bytes.
    pub(crate) fn serial(&self, sled: &SledDesc) -> String {
        format!("{}-{}{}", sled.serial_number, self.variant.tag(), self.index)
    }

    /// Name of the disk's zvol under the deployment's falcon dataset.
    fn volume(&self, sled: &SledDesc) -> String {
        format!("{}-{}{}", sled.name, self.variant.tag(), self.index)
    }
}

/// The disk complement of one sled, in a stable order.
pub(crate) fn layout() -> Vec<DiskDesc> {
    let mut out = Vec::with_capacity(M2_COUNT + U2_COUNT);
    let mut pci_dev = FIRST_PCI_DEV;
    for (variant, count, size_gb) in [
        (Variant::M2, M2_COUNT, M2_SIZE_GB),
        (Variant::U2, U2_COUNT, U2_SIZE_GB),
    ] {
        for index in 0..count {
            out.push(DiskDesc { variant, index, size_gb, pci_dev });
            pci_dev += 1;
        }
    }
    out
}

/// NVMe Identify Controller reports a fixed-width serial; pad to the wire size.
fn serial_bytes(serial: &str) -> Result<[u8; 20]> {
    let bytes = serial.as_bytes();
    if bytes.len() > 20 {
        bail!("NVMe serial {serial:?} exceeds 20 bytes");
    }
    let mut out = [b' '; 20];
    out[..bytes.len()].copy_from_slice(bytes);
    Ok(out)
}

/// Create this launch's sled disks. They live under the deployment's own falcon
/// dataset, so `teardown`'s `zfs destroy -r` already reaps them and every launch
/// starts on blank media - which matters, since a stale U.2 carries the previous
/// rack's crucible and trust-quorum state.
pub(crate) fn create_zvols(
    dataset: &str,
    deployment: &str,
    sleds: &[SledDesc],
) -> Result<()> {
    for sled in sleds {
        for disk in layout() {
            let vol = format!(
                "{dataset}/topo/{deployment}/{}",
                disk.volume(sled)
            );
            // A launch over a half-torn-down rack would otherwise inherit the
            // old media.
            let _ = std::process::Command::new("zfs")
                .args(["destroy", &vol])
                .output();
            let size = format!("{}G", disk.size_gb);
            let out = std::process::Command::new("zfs")
                .args(["create", "-p", "-s", "-V", &size, &vol])
                .output()
                .with_context(|| format!("zfs create {vol}"))?;
            if !out.status.success() {
                bail!(
                    "zfs create {vol}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
    }
    Ok(())
}

/// Attach one sled's disks to its node's propolis spec.
///
/// `NodeRef`'s index is private to falcon, so the node is reached by name; a
/// `Runner::set_components` mirroring `set_smbios_type1` would be tidier.
pub(crate) fn attach(
    d: &mut Runner,
    dataset: &str,
    deployment: &str,
    sled: &SledDesc,
    _n: NodeRef,
) -> Result<()> {
    let node = d
        .deployment
        .nodes
        .iter_mut()
        .find(|x| x.name == sled.name)
        .with_context(|| format!("node {} not in deployment", sled.name))?;

    for disk in layout() {
        let vol = disk.volume(sled);
        let backend = SpecKey::Name(format!("{vol}_backing"));
        node.components.insert(
            backend.clone(),
            ComponentV0::FileStorageBackend(FileStorageBackend {
                // The raw (character) device, as falcon uses for boot disks.
                path: format!(
                    "/dev/zvol/rdsk/{dataset}/topo/{deployment}/{vol}"
                ),
                readonly: false,
                block_size: BLOCK_SIZE,
                workers: None,
            }),
        );
        node.components.insert(
            SpecKey::Name(vol),
            ComponentV0::NvmeDisk(NvmeDisk {
                backend_id: backend,
                pci_path: PciPath::new(0, disk.pci_dev, 0)
                    .context("PCI path for sled disk")?,
                serial_number: serial_bytes(&disk.serial(sled))?,
            }),
        );
    }
    Ok(())
}
