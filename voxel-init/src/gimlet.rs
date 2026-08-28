// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Gimlet (sled) bring-up—replaces `gimlet-launch.sh`. Runs in the voxel-cp
//! helios guest. The control plane is already installed (`/opt/oxide`); this
//! applies the per-launch / topology bits that can't be baked: ephemeral virtual
//! hardware, the detected underlay NICs, the generated sled + RSS configs, the
//! switch1 identity for the 2nd scrimlet, then activates the control plane (which
//! kicks RSS on the RSS node).

use crate::sys::{
    capture, note, read_external_net, replace_in_file, run, run_env, run_quiet,
    warn,
};
use anyhow::{Context, Result, bail};
use camino::Utf8Path;
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

const CARGO_BAY: &str = "/opt/cargo-bay";
const OMICRON: &str = "/opt/oxide/omicron";
// `<CARGO_BAY>/sled-config.toml` (kept literal: `concat!` can't expand a const).
const SLED_CFG: &str = "/opt/cargo-bay/sled-config.toml";
const PATCHED_CFG: &str = "/tmp/sled-config.toml";
/// Written at bake by `image create --from-tuf`; selects the native (no
/// omicron tooling) bring-up paths.
const TUF_MARKER: &str = "/opt/oxide/.voxel-tuf";

pub fn bring_up() -> Result<()> {
    setup_ssh();
    crash_dump();
    maybe_load_sidecar();

    // TUF images carry no omicron tooling; their virtual hardware and
    // activation are handled natively below.
    let tuf = Utf8Path::new(TUF_MARKER).exists();
    if !tuf {
        // The omicron CLI tools are baked into the image at /opt/oxide/omicron,
        // and xtask/omicron-package run relative to that tree.
        if !Utf8Path::new(OMICRON).exists() {
            bail!("{OMICRON} not baked into the image");
        }
        std::env::set_current_dir(OMICRON)
            .with_context(|| format!("cd {OMICRON}"))?;
    }

    let (underlay, other) = detect_underlay();
    patch_sled_config(&underlay, tuf)?;
    setup_external_networking(&other);
    setup_sp_net(&other);
    if tuf {
        setup_virtual_hardware_native()?;
    } else {
        setup_virtual_hardware();
    }
    preseed_install_datasets();
    inject_runtime_configs()?;
    unplumb_softnpu_source();
    maybe_start_switch_enforcer()?;

    if tuf {
        activate_native();
    } else {
        // Activate the (already-unpacked) control plane. On the RSS node this
        // kicks RSS. omicron-package reads XTASK_BIN / XTASK_DOWNLOADER_BIN
        // from the environment.
        let xtask_bin = format!("{OMICRON}/xtask");
        let xtask_dl = format!("{OMICRON}/xtask-downloader");
        if !run_env(
            "./omicron-package",
            &["activate"],
            &[("XTASK_BIN", &xtask_bin), ("XTASK_DOWNLOADER_BIN", &xtask_dl)],
        ) {
            warn("omicron-package activate failed");
        }
    }
    note("gimlet bring-up complete");
    Ok(())
}

/// TUF images stage no xtask, and voxel sleds have real storage: each M.2 and
/// U.2 is a propolis NVMe device backed by a host zvol. Bring-up is therefore
/// discovering them, laying down the gimlet M.2 partition layout that omicron
/// refuses to create itself, seeding the boot image, and naming them in the
/// sled-agent config. sled-agent then handles them as real disks - none of its
/// synthetic-disk path runs.
fn setup_virtual_hardware_native() -> Result<()> {
    let disks = discover_disks()?;
    if disks.is_empty() {
        bail!("no NVMe sled disks; was the rack launched by an older voxel?");
    }
    for d in &disks {
        if d.m2 {
            ensure_m2_layout(d)?;
            seed_boot_image(d)?;
        } else {
            ensure_u2_label(d)?;
        }
    }
    write_disk_config(&disks)?;
    note(format!(
        "{} sled disks ({} M.2, {} U.2)",
        disks.len(),
        disks.iter().filter(|d| d.m2).count(),
        disks.iter().filter(|d| !d.m2).count()
    ));
    Ok(())
}

/// One NVMe sled disk as the guest sees it.
struct SledDisk {
    /// The NVMe serial voxel gave the device, e.g. `2FAKE000-M20`. Doubles as
    /// the disk's `DiskIdentity.serial`.
    serial: String,
    m2: bool,
    /// Index within the variant: M.2 0/1 are slots A/B, U.2 0..4 are bays.
    index: usize,
    /// illumos disk name, e.g. `c2t1d0`.
    disk: String,
    /// `/devices` path of the blkdev node with no `:slice` suffix, which is
    /// what sled-agent wants in `paths.devfs_path`.
    devfs_path: String,
}

/// Ask the disks what they are. voxel encodes each disk's role and index in the
/// NVMe serial when it attaches the device, so the guest needs no shared table
/// of PCI slots to interpret what it finds - and `nvmeadm list` in a wedged
/// sled still says plainly which disk is which.
///
/// The controller path is used rather than `readlink /dev/dsk/<disk>`: the
/// whole-disk link only appears once a disk carries a label, and a fresh U.2
/// has none.
fn discover_disks() -> Result<Vec<SledDisk>> {
    let out = capture(
        "nvmeadm",
        &["list", "-p", "-o", "serial,disk,ctrlpath,namespace"],
    )
    .context("nvmeadm list")?;
    let mut disks = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.trim().split(':').collect();
        if f.len() != 4 {
            continue;
        }
        let (serial, disk, ctrlpath, ns) = (f[0], f[1], f[2], f[3]);
        // `<sled serial>-{M2,U2}<index>`. Anything else is not one of ours.
        let Some((_, role)) = serial.rsplit_once('-') else {
            continue;
        };
        if role.len() < 3 {
            continue;
        }
        let (tag, index) = role.split_at(2);
        let m2 = match tag {
            "M2" => true,
            "U2" => false,
            _ => continue,
        };
        let Ok(index) = index.parse::<usize>() else {
            continue;
        };
        disks.push(SledDisk {
            serial: serial.to_string(),
            m2,
            index,
            disk: disk.to_string(),
            devfs_path: format!("/devices{ctrlpath}/blkdev@{ns},0"),
        });
    }
    // M.2s before U.2s, each by index; the serial already sorts that way.
    disks.sort_by(|a, b| a.serial.cmp(&b.serial));
    Ok(disks)
}

impl SledDisk {
    /// The gimlet bay number. Nothing on the injected-disk path consults it
    /// (the variant comes from the config, not from the slot), but matching
    /// real hardware keeps inventory readable: M.2 A/B are 0x11/0x12.
    fn slot(&self) -> i64 {
        if self.m2 { 0x11 + self.index as i64 } else { self.index as i64 }
    }

    /// Which M.2 the sled booted from. A voxel guest still boots off falcon's
    /// own disk, so for now this is an assertion rather than an observation.
    /// Taking it from the SP's active host-boot-flash slot is what an update's
    /// PostUpdateWait needs, and is the next increment.
    fn is_boot_disk(&self) -> bool {
        self.m2 && self.index == 0
    }

    /// This disk as sled-agent's `UnparsedDisk`. `next_active_slot` is left
    /// out: it is an `Option`, and TOML has no null.
    fn config_entry(&self) -> String {
        format!(
            "{{ paths = {{ devfs_path = \"{devfs}\", \
             dev_path = \"/dev/dsk/{disk}\" }}, slot = {slot}, \
             variant = \"{variant}\", identity = {{ vendor = \"Oxide\", \
             model = \"propolis-nvme\", serial = \"{serial}\" }}, \
             is_boot_disk = {boot}, firmware = {{ active_slot = 1, \
             slot1_read_only = true, number_of_slots = 1, \
             slot_firmware_versions = [\"voxel\"] }} }}",
            devfs = self.devfs_path,
            disk = self.disk,
            slot = self.slot(),
            variant = if self.m2 { "M2" } else { "U2" },
            serial = self.serial,
            boot = self.is_boot_disk(),
        )
    }
}

/// Sizes of the M.2 partitions voxel fixes; the ZFS pool takes what is left.
/// The boot image partition has to hold a host phase 2 (~1.2 GiB today).
const M2_BOOT_IMAGE_BYTES: u64 = 4 << 30;
const M2_DUMP_BYTES: u64 = 4 << 30;
const M2_RESERVED_BYTES: u64 = 1 << 20;

/// omicron expects exactly these six on an M.2 - BootImage, three Reserved,
/// DumpDevice, ZfsPool at indices 0..5 - and refuses to create them itself
/// (`CannotFormatM2NotImplemented`), so voxel lays them down.
const M2_PARTITIONS: usize = 6;

/// A `prtvtoc` partition line.
struct VtocPartition {
    index: usize,
    start: u64,
    count: u64,
}

/// Read a disk's label: (bytes per sector, partitions).
fn read_vtoc(disk: &str) -> Result<(u64, Vec<VtocPartition>)> {
    let out = capture("prtvtoc", &[&format!("/dev/rdsk/{disk}s0")])
        .with_context(|| format!("prtvtoc {disk}"))?;
    let mut bytes_per_sector = 0u64;
    let mut parts = Vec::new();
    for line in out.lines() {
        let t = line.trim();
        // Geometry arrives as a comment: "* 4096 bytes/sector".
        if let Some(rest) = t.strip_prefix('*') {
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() >= 2 && f[1] == "bytes/sector" {
                bytes_per_sector = f[0].parse().unwrap_or(0);
            }
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        let (Ok(index), Ok(start), Ok(count)) =
            (f[0].parse(), f[3].parse(), f[4].parse())
        else {
            continue;
        };
        parts.push(VtocPartition { index, start, count });
    }
    if bytes_per_sector == 0 {
        bail!("prtvtoc {disk}: no bytes/sector in the label");
    }
    Ok((bytes_per_sector, parts))
}

/// Give an M.2 the gimlet partition layout, idempotently.
///
/// A fresh NVMe namespace carries only a default VTOC, and `fmthard` can only
/// edit a real EFI label, so borrow the one `zpool create` writes and reshape
/// it. Every partition is tagged 4 (usr): `fmthard` rejects tag 0 outright, and
/// omicron reads intent from the partition index rather than the tag.
fn ensure_m2_layout(d: &SledDisk) -> Result<()> {
    if let Ok((_, parts)) = read_vtoc(&d.disk)
        && parts.iter().filter(|p| p.index < M2_PARTITIONS).count()
            == M2_PARTITIONS
    {
        note(format!("{} ({}): M.2 layout already present", d.disk, d.serial));
        return Ok(());
    }
    let tmp = format!("voxelm2{}", d.index);
    if !run("zpool", &["create", "-f", &tmp, &d.disk]) {
        bail!("{}: could not write an EFI label", d.disk);
    }
    run("zpool", &["destroy", &tmp]);

    let (bytes_per_sector, parts) = read_vtoc(&d.disk)?;
    let usable = parts
        .iter()
        .find(|p| p.index == 0)
        .with_context(|| format!("{}: no partition 0 after labeling", d.disk))?;
    let reserved = parts
        .iter()
        .find(|p| p.index == 8)
        .with_context(|| format!("{}: no reserved partition", d.disk))?;

    let sectors = |bytes: u64| bytes.div_ceil(bytes_per_sector);
    let boot = sectors(M2_BOOT_IMAGE_BYTES);
    let small = sectors(M2_RESERVED_BYTES);
    let dump = sectors(M2_DUMP_BYTES);
    let first = usable.start;
    let Some(pool) =
        reserved.start.checked_sub(first + boot + 3 * small + dump)
    else {
        bail!("{} is too small for the M.2 layout", d.disk);
    };

    let mut start = first;
    let mut map = String::new();
    for (i, count) in
        [boot, small, small, small, dump, pool].into_iter().enumerate()
    {
        map.push_str(&format!("{i} 4 00 {start} {count}\n"));
        start += count;
    }
    map.push_str(&format!("8 11 00 {} {}\n", reserved.start, reserved.count));

    let path = format!("/tmp/m2-{}.map", d.disk);
    fs::write(&path, &map).with_context(|| format!("write {path}"))?;
    if !run("fmthard", &["-s", &path, &format!("/dev/rdsk/{}s0", d.disk)]) {
        bail!("{}: fmthard rejected the M.2 layout", d.disk);
    }
    note(format!("{} ({}): M.2 layout written", d.disk, d.serial));
    Ok(())
}

/// Give a U.2 an EFI label and leave it otherwise empty.
///
/// omicron creates the U.2's pool itself, but reaches the disk through its
/// `/dev/dsk/<disk>` whole-disk path, and illumos only publishes that link once
/// the disk carries a label. A factory U.2 in a real rack arrives labeled; a
/// freshly made zvol does not, so hand it the label `zpool create` writes and
/// give the pool straight back for sled-agent to make on its own terms.
fn ensure_u2_label(d: &SledDisk) -> Result<()> {
    if Utf8Path::new(&format!("/dev/dsk/{}", d.disk)).exists() {
        return Ok(());
    }
    let tmp = format!("voxelu2{}", d.index);
    if !run("zpool", &["create", "-f", &tmp, &d.disk]) {
        bail!("{}: could not write an EFI label", d.disk);
    }
    run("zpool", &["destroy", &tmp]);
    note(format!("{} ({}): labeled", d.disk, d.serial));
    Ok(())
}

/// Write the image's host phase 2 into the M.2's boot image partition, so the
/// slot inventories at the image's own repo version instead of erroring. On a
/// real block device this runs at device speed.
fn seed_boot_image(d: &SledDisk) -> Result<()> {
    const HOST_BOOT_IMAGE: &str = "/opt/voxel/host/boot-image.img";
    if !Utf8Path::new(HOST_BOOT_IMAGE).exists() {
        bail!("{HOST_BOOT_IMAGE} missing from the image");
    }
    let of = format!("of=/dev/rdsk/{}s0", d.disk);
    if !run("dd", &[&format!("if={HOST_BOOT_IMAGE}"), &of, "bs=1048576"]) {
        bail!("{}: seeding the host phase 2 failed", d.disk);
    }
    note(format!("{}: host phase 2 seeded", d.disk));
    Ok(())
}

/// Name the discovered disks in the sled-agent config.
///
/// `ExternalDisks::Hardcoded` carries a list of `UnparsedDisk`s that sled-agent
/// injects during device polling on any platform that is not an Oxide sled -
/// which is the path a voxel guest takes. They arrive as REAL disks, with no
/// omicron change of any kind. `vdevs` goes empty: nothing is file-backed now.
fn write_disk_config(disks: &[SledDisk]) -> Result<()> {
    let items: Vec<String> =
        disks.iter().map(SledDisk::config_entry).collect();
    let rendered = format!(
        "external_disks = {{ kind = \"hardcoded\", vdevs = [], \
         disks = [{}] }}",
        items.join(", ")
    );
    let parsed: toml_edit::DocumentMut =
        rendered.parse().context("render external_disks")?;
    let text = fs::read_to_string(PATCHED_CFG)
        .with_context(|| format!("read {PATCHED_CFG}"))?;
    let mut doc: toml_edit::DocumentMut =
        text.parse().with_context(|| format!("parse {PATCHED_CFG}"))?;
    doc["external_disks"] = parsed["external_disks"].clone();
    fs::write(PATCHED_CFG, doc.to_string())
        .with_context(|| format!("write {PATCHED_CFG}"))?;
    Ok(())
}

/// TUF images: sled-agent is prestaged; activation is importing its SMF
/// manifest now that the runtime configs are injected. The manifest's
/// default instance starts on import.
fn activate_native() {
    if !run("svccfg", &["import", "/opt/oxide/sled-agent/pkg/manifest.xml"]) {
        warn("svccfg import sled-agent manifest failed");
        return;
    }
    run_quiet("svcadm", &["enable", "svc:/oxide/sled-agent:default"]);
    note("sled-agent activated");
}

/// SSH convenience for `voxel host login` (was the sourced `setup_ssh`
/// function). illumos sshd defaults differ from debian's, hence the explicit
/// config edits.
fn setup_ssh() {
    let authorized = format!("{CARGO_BAY}/root_authorized_keys");
    if Utf8Path::new(&authorized).exists() {
        let _ = fs::create_dir_all("/root/.ssh");
        if let Ok(keys) = fs::read(&authorized) {
            use std::io::Write;
            match fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/root/.ssh/authorized_keys")
            {
                Ok(mut f) => {
                    if let Err(e) = f.write_all(&keys) {
                        warn(format!("authorized_keys: {e}"));
                    }
                }
                Err(e) => warn(format!("authorized_keys: {e}")),
            }
        }
    }
    run("ssh-keygen", &["-A"]);
    replace_in_file(
        "/etc/ssh/sshd_config",
        &[
            ("#PasswordAuthentication no", "PasswordAuthentication yes"),
            ("#PermitEmptyPasswords no", "PermitEmptyPasswords yes"),
            ("PermitRootLogin without-password", "PermitRootLogin yes"),
        ],
    );
    run("svcadm", &["restart", "svc:/network/ssh:default"]);
}

fn crash_dump() {
    run("zfs", &["create", "-p", "-V", "8G", "rpool/dump"]);
    run("dumpadm", &["-d", "/dev/zvol/dsk/rpool/dump"]);
}

/// Scrimlets load the baked SoftNPU sidecar P4 program. Gimlets have no softnpu
/// device, so `scadm propolis load-program` would fail there—gate on sled_mode.
fn maybe_load_sidecar() {
    let scrimlet = fs::read_to_string(SLED_CFG)
        .map(|s| s.contains(r#"sled_mode = "scrimlet""#))
        .unwrap_or(false);
    if scrimlet {
        run(
            "/opt/oxide/sidecar/scadm",
            &[
                "propolis",
                "load-program",
                "/opt/oxide/sidecar/libsidecar_lite.so",
            ],
        );
    }
}

/// The Oxide underlay is jumbo (MTU 9000). The guest vioif ordering is
/// topology-dependent (scrimlet vs gimlet, sled count), so we can't hardcode
/// names: probe `vioif0..7`—the ones that accept MTU 9000 are the underlay, the
/// rest are ext / host-LAN candidates.
fn detect_underlay() -> (Vec<String>, Vec<String>) {
    let mut underlay = Vec::new();
    let mut other = Vec::new();
    for n in 0..8 {
        let nic = format!("vioif{n}");
        if !run_quiet("dladm", &["show-link", &nic]) {
            continue;
        }
        if run_quiet("dladm", &["set-linkprop", "-t", "-p", "mtu=9000", &nic]) {
            underlay.push(nic);
        } else {
            other.push(nic);
        }
    }
    note(format!("underlay(jumbo)={underlay:?} ext-candidates={other:?}"));
    (underlay, other)
}

/// Patch this sled's config to the detected underlay links (the generated config
/// ships placeholders), write the patched copy to /tmp, and seed the xtask
/// WORKSPACE config (`smf/sled-agent/non-gimlet/config.toml`) that
/// virtual-hardware reads. Uses `toml_edit`—no `sed`.
fn patch_sled_config(underlay: &[String], tuf: bool) -> Result<()> {
    let text = fs::read_to_string(SLED_CFG)
        .with_context(|| format!("read {SLED_CFG}"))?;
    let mut doc: toml_edit::DocumentMut =
        text.parse().with_context(|| format!("parse {SLED_CFG}"))?;
    if let Some(first) = underlay.first() {
        doc["data_link"] = toml_edit::value(first.as_str());
        // Substitute the detected NICs into whatever data_links SHAPE the staged
        // config has, so this agent works on any image: an inline table (omicron
        // main's `{ kind = "virtual", devices = [...] }`) keeps its `kind` and
        // only its `devices` are replaced; a bare array (pre-main) is rewritten.
        let mut devices = toml_edit::Array::new();
        for u in underlay {
            devices.push(u.as_str());
        }
        let dl = &mut doc["data_links"];
        if let Some(table) = dl.as_inline_table_mut() {
            table.insert("devices", toml_edit::Value::Array(devices));
        } else {
            *dl = toml_edit::value(devices);
        }
    }
    fs::write(PATCHED_CFG, doc.to_string())
        .with_context(|| format!("write {PATCHED_CFG}"))?;
    // xtask virtual-hardware reads the workspace config (vdevs + sled_mode);
    // TUF images have neither xtask nor the workspace tree.
    if !tuf {
        let workspace = "smf/sled-agent/non-gimlet/config.toml";
        fs::copy(PATCHED_CFG, workspace)
            .with_context(|| format!("seed {workspace}"))?;
    }
    Ok(())
}

/// Whether `ifc` already carries an IPv6 link-local. Without one, ipadm rejects
/// a global v6 address on that link.
fn has_link_local(ifc: &str) -> bool {
    let Some(out) =
        capture("ipadm", &["show-addr", "-p", "-o", "addrobj,addr"])
    else {
        return false;
    };
    let prefix = format!("{ifc}/");
    out.lines().any(|l| l.starts_with(&prefix) && l.contains("fe80"))
}

/// Give a scrimlet an address on its rack's SP network, staged by `voxel launch`
/// in `/opt/cargo-bay/sp-net` as `addr/prefixlen`. The emulated SP fleet runs on
/// the falcon host, so that prefix is on-link here and the switch zone reaches it
/// over the bootstrap route it already has - no route to add, nothing to discover.
/// No staged file means a gimlet or an sp-sim rack, so nothing to do.
fn setup_sp_net(other: &[String]) {
    let Ok(staged) = fs::read_to_string(format!("{CARGO_BAY}/sp-net")) else {
        return;
    };
    let addr = staged.trim();
    if addr.is_empty() {
        return;
    }
    let Some(ifc) = other.iter().find(|ifc| ifc.as_str() != "vioif0") else {
        warn("sp-net staged but no external NIC candidate found");
        return;
    };
    // IPv6 refuses a global address on a link with no link-local. Link-local
    // only, so we never adopt a prefix the host LAN advertises.
    if !has_link_local(ifc) {
        run(
            "ipadm",
            &[
                "create-addr",
                "-T",
                "addrconf",
                "-p",
                "stateless=no,stateful=no",
                &format!("{ifc}/voxelll"),
            ],
        );
    }
    // Falcon keeps the sled disk across destroy/relaunch, so a prior launch's
    // address persists and would block create-addr. Silent on absence.
    run_quiet("ipadm", &["delete-addr", &format!("{ifc}/spnet")]);
    // Persistent, not -t: an SP reset restarts its sled, and a scrimlet that
    // came back without this address would leave its MGS unable to reach the
    // fleet.
    if run(
        "ipadm",
        &["create-addr", "-T", "static", "-a", addr, &format!("{ifc}/spnet")],
    ) {
        // The sled must never SOURCE from this address. The trust quorum records
        // whichever peer address it sees, so a sled that dialed the bootstrap
        // network from here would be remembered at an address whose bootstrap
        // agent does not exist, and RSS would retry it forever. Deprecated
        // addresses are skipped by source selection (RFC 6724 rule 3) but still
        // receive, and still anchor the on-link prefix the host routes back to.
        run(
            "ipadm",
            &["set-addrprop", "-p", "deprecated=on", &format!("{ifc}/spnet")],
        );
        note(format!("SP network {addr} on {ifc} (deprecated as a source)"));
    } else {
        warn(format!(
            "could not add SP network {addr} on {ifc}; MGS cannot reach the \
             host SP fleet"
        ));
    }
}

/// Bring up the non-underlay NICs that reach the host LAN—but never vioif0,
/// the SoftNPU packet source the switch zone must claim (plumbing it in the GZ
/// makes oxz_switch fail "interface used in the global zone").
///
/// Isolated mode (voxel-managed segment) stages a static address in
/// `/opt/cargo-bay/ external-net`, applying it to the first non-vioif0 NIC and
/// using the staged DNS list. `lan` mode falls back to DHCP + a hardcoded
/// resolver.
fn setup_external_networking(other: &[String]) {
    if let Some(ext) = read_external_net() {
        let resolv: String =
            ext.dns.iter().map(|s| format!("nameserver {s}\n")).collect();
        if let Err(e) = fs::write("/etc/resolv.conf", resolv) {
            warn(format!("resolv.conf: {e}"));
        }

        match other.iter().find(|ifc| ifc.as_str() != "vioif0") {
            Some(ifc) => {
                // Falcon keeps the sled disk across destroy/relaunch, so a
                // prior launch's static address persists in /etc/ipadm/. `ipadm
                // create-addr` refuses to add over an existing addrobj, so wipe
                // any leftover /v4 addr before staging the current one. Silent
                // on absence (first launch, or a manual pre-wipe).
                run_quiet("ipadm", &["delete-addr", &format!("{ifc}/v4")]);
                run(
                    "ipadm",
                    &[
                        "create-addr",
                        "-T",
                        "static",
                        "-a",
                        &ext.ip_cidr,
                        &format!("{ifc}/v4"),
                    ],
                );
                // Persist the route (-p). voxel-init runs at launch, not at
                // boot, so a plain `route add` is lost if the sled VM reboots
                // mid-run while the static addr above survives via
                // /etc/ipadm/.
                //
                // We clear prior persistent defaults first so that a relaunch
                // (or a gateway change) does not stack or strand entries in
                // /etc/inet/static_routes.
                clear_persistent_defaults();
                run("route", &["-p", "add", "default", &ext.gateway]);
            }
            None => {
                warn("external-net staged but no external NIC candidate found")
            }
        }
        return;
    }
    if let Err(e) = fs::write("/etc/resolv.conf", "nameserver 1.1.1.1\n") {
        warn(format!("resolv.conf: {e}"));
    }
    // A prior isolated run's persistent default would otherwise sit alongside
    // the DHCP default and can win out.
    clear_persistent_defaults();
    for ifc in other {
        if ifc == "vioif0" {
            continue;
        }
        // Wipe any leftover /v4 addrobj (same reason as the isolated branch:
        // a prior isolated run's static address persists across relaunches
        // and blocks the `-T dhcp` create). Silent on absence.
        run_quiet("ipadm", &["delete-addr", &format!("{ifc}/v4")]);
        run("ipadm", &["create-addr", "-T", "dhcp", &format!("{ifc}/v4")]);
    }
}

/// Delete every persistent default route, not just the one via the current
/// gateway. The sled disk survives destroy/relaunch, so a gateway change (or
/// an isolated to lan switch) would otherwise strand a stale default in
/// /etc/inet/static_routes pointing at a dead gateway.
fn clear_persistent_defaults() {
    let Ok(out) = Command::new("route").args(["-p", "show"]).output() else {
        return;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // Lines look like: "persistent: route add default 172.30.199.199".
        let mut toks = line.split_whitespace().skip_while(|t| *t != "default");
        let (Some(_), Some(gw)) = (toks.next(), toks.next()) else {
            continue;
        };
        run_quiet("route", &["-p", "delete", "default", gw]);
    }
}

/// Ephemeral emulated U.2/M.2 (deliberately not baked). Wipe any vdevs from a
/// prior launch first—falcon keeps the sled disk across destroy/relaunch, so
/// stale vdevs carry the OLD rack's trust-quorum ledger + crucible/cockroach
/// data; reusing them makes a fresh launch falsely report "initialized". A clean
/// launch must start from fresh storage.
fn setup_virtual_hardware() {
    let softnpu = [("SOFTNPU_MODE", "propolis")];
    run_env("./xtask", &["virtual-hardware", "destroy"], &softnpu);
    wipe_vdevs();
    if !run_env("./xtask", &["virtual-hardware", "create"], &softnpu) {
        warn("virtual-hardware create failed");
    }
}

/// Control-plane service zones that live in the install dataset (the
/// preset-independent set; global-zone software like switch/propolis is not
/// here). Their hashes must match the target-release TUF repo's zone artifacts.
const INSTALL_ZONES: &[&str] = &[
    "clickhouse.tar.gz",
    "clickhouse_keeper.tar.gz",
    "clickhouse_server.tar.gz",
    "cockroachdb.tar.gz",
    "crucible.tar.gz",
    "crucible_pantry.tar.gz",
    "external_dns.tar.gz",
    "internal_dns.tar.gz",
    "nexus.tar.gz",
    "ntp.tar.gz",
    "oximeter.tar.gz",
    "probe.tar.gz",
];

/// Corpus staged by `image create --from-tuf`: the target repo's own
/// measurement corpus artifacts, byte exact. Preferred over the embedded fake
/// corpus when present.
const STAGED_CORPUS: &str = "/opt/oxide/measurements";

/// Fixed fake measurement corpus, embedded so it is present without a build-time
/// bake. A non-empty measurement manifest is required for a sled to be eligible
/// for noop image-source conversion. Voxel TUF repos must carry this same corpus
/// so the hashes match.
const CORPUS: &[(&str, &[u8])] = &[
    (
        "fake-measurement-id-9830767c45f2a02210a177fabafafe2c84501039289483f72cec299b0c78dbcb.cbor",
        include_bytes!(
            "../corpus/fake-measurement-id-9830767c45f2a02210a177fabafafe2c84501039289483f72cec299b0c78dbcb.cbor"
        ),
    ),
    (
        "fake-measurement-id-ae9279e9135de75e4e137c6da7f939b5a2eae6d931a7f2205df930e37cd58096.cbor",
        include_bytes!(
            "../corpus/fake-measurement-id-ae9279e9135de75e4e137c6da7f939b5a2eae6d931a7f2205df930e37cd58096.cbor"
        ),
    ),
];

/// Pre-create and populate the M.2 install datasets before sled-agent adopts
/// them. sled-agent reads the install-dataset manifest exactly once at startup
/// and never reloads, so seeding after it starts would need a restart (which on
/// a scrimlet recreates oxz_switch and wedges the rack). Instead we run in the
/// window after `virtual-hardware create` made the vdevs but before
/// `omicron-package activate` starts sled-agent: create a pool on each M.2 vdev,
/// drop the zones + corpus into `install/`, and leave it imported. sled-agent's
/// adoption then finds the pool and preserves it, and its first read reports a
/// non-empty manifest, so the reconfigurator can noop-convert to the TUF repo
/// with no restart. The pool must NOT be exported: sled-agent's `zpool import`
/// has no `-d`, so it cannot locate an exported file-vdev pool; an imported one
/// yields "already created/imported", which its import handler accepts.
/// Best-effort: on any failure the sled falls back to the manual path.
fn preseed_install_datasets() {
    // Real sled disks put the internal pool on the M.2's ZfsPool partition
    // (index 5 -> slice 5). Older, file-backed images fall through below.
    let mut vdevs: Vec<String> = discover_disks()
        .unwrap_or_default()
        .iter()
        .filter(|d| d.m2)
        .map(|d| format!("/dev/dsk/{}s5", d.disk))
        .collect();
    if vdevs.is_empty()
        && let Ok(entries) = fs::read_dir("/var/tmp")
    {
        for e in entries.flatten() {
            let p = e.path();
            let is_m2 = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("m2_") && n.ends_with(".vdev"));
            if is_m2 && let Some(s) = p.to_str() {
                vdevs.push(s.to_string());
            }
        }
    }
    if vdevs.is_empty() {
        warn("preseed: no M.2 storage found; skipping install-dataset seed");
        return;
    }
    // One MUPdate marker per sled, mirrored onto both M.2s: installinator
    // stamps the same UUID on both, and sled-agent logs a mismatch otherwise.
    let mupdate_uuid =
        capture("uuidgen", &[]).map(|u| u.trim().to_lowercase());
    for vdev in &vdevs {
        let Some(uuid) =
            capture("uuidgen", &[]).map(|u| u.trim().to_lowercase())
        else {
            warn("preseed: uuidgen failed");
            continue;
        };
        let pool = format!("oxi_{uuid}");
        let mnt = format!("/pool/int/{uuid}/install");
        if !run("zpool", &["create", "-f", &pool, vdev]) {
            warn(format!("preseed: zpool create {pool} on {vdev} failed"));
            continue;
        }
        if !run(
            "zfs",
            &[
                "create",
                "-o",
                &format!("mountpoint={mnt}"),
                &format!("{pool}/install"),
            ],
        ) {
            warn(format!("preseed: zfs create {pool}/install failed"));
            run("zpool", &["destroy", "-f", &pool]);
            continue;
        }
        let meas = format!("{mnt}/measurements");
        let _ = fs::create_dir_all(&meas);
        for z in INSTALL_ZONES {
            let src = format!("/opt/oxide/{z}");
            if Utf8Path::new(&src).exists()
                && let Err(e) = fs::copy(&src, format!("{mnt}/{z}"))
            {
                warn(format!("preseed: copy {z}: {e}"));
            }
        }
        let mut staged = 0;
        if let Ok(entries) = Utf8Path::new(STAGED_CORPUS).read_dir_utf8() {
            for e in entries.flatten() {
                let name = e.file_name();
                match fs::copy(e.path(), format!("{meas}/{name}")) {
                    Ok(_) => staged += 1,
                    Err(e) => warn(format!("preseed: copy corpus {name}: {e}")),
                }
            }
        }
        if staged == 0 {
            for (name, bytes) in CORPUS {
                if let Err(e) = fs::write(format!("{meas}/{name}"), bytes) {
                    warn(format!("preseed: write corpus {name}: {e}"));
                }
            }
        }
        // A rack installed by installinator starts with a MUPdate override on
        // every install dataset, which freezes the reconfigurator until the
        // operator uploads the matching repo and calls recovery-finish (RFD
        // 556). Stage the same marker so a fresh voxel rack starts in that
        // state and exercises the real first step of the update flow, instead
        // of booting straight into normal operation.
        match &mupdate_uuid {
            Some(id) => {
                let path = format!("{mnt}/mupdate-override.json");
                let json = format!("{{\"mupdate_uuid\":\"{id}\"}}");
                if let Err(e) = fs::write(&path, json) {
                    warn(format!("preseed: write mupdate override: {e}"));
                }
            }
            None => warn("preseed: no uuid for the mupdate override"),
        }
        // Leave the pool imported: sled-agent's `zpool import -f` (no `-d`)
        // cannot find an exported file-vdev pool, but on an already-imported
        // one it gets "a pool with that name is already created/imported",
        // which its import handler treats as success, so adoption preserves it.
        note(format!("preseed: staged install dataset on {vdev} ({pool})"));
    }
}

fn wipe_vdevs() {
    let entries = match fs::read_dir("/var/tmp") {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|x| x.to_str()) == Some("vdev") {
            let _ = fs::remove_file(&p);
        }
    }
}

/// Inject the (data-link-patched) sled config + the generated RSS config (the
/// latter present only on the RSS node) as the runtime sled-agent configs.
fn inject_runtime_configs() -> Result<()> {
    fs::copy(PATCHED_CFG, "/opt/oxide/sled-agent/pkg/config.toml")
        .context("inject sled-agent config.toml")?;
    let rss = format!("{CARGO_BAY}/config-rss.toml");
    if Utf8Path::new(&rss).exists() {
        fs::copy(&rss, "/opt/oxide/sled-agent/pkg/config-rss.toml")
            .context("inject config-rss.toml")?;
    }
    Ok(())
}

/// Force vioif0 (the SoftNPU pkt_source) unplumbed in the GZ—the switch zone
/// must claim it, but the softnpu fabric / DHCP keeps grabbing it. Harmless on
/// gimlets (vioif0 unused there).
fn unplumb_softnpu_source() {
    run_quiet("ipadm", &["delete-addr", "vioif0/v4"]);
    run_quiet("ipadm", &["delete-if", "vioif0"]);
}

const SWITCH_ZONE_MGS: &str =
    "/zone/oxz_switch/root/var/svc/manifest/site/mgs/config.toml";
const SWITCH_ZONE_SP: &str =
    "/zone/oxz_switch/root/var/svc/manifest/site/sp-sim/config.toml";

/// Bake-once: the image bakes switch0 + sp-sim for a fixed gimlet count, but this
/// launch may run a different count, and the 2nd scrimlet must present as switchN
/// anyway. `stage_config` generates this scrimlet's slot MGS config + sp-sim
/// config for the live count; if they're staged, spawn a detached watcher that
/// swaps them into the switch zone (+ bounces the services) as soon as it
/// extracts. Detached into its own session with stdio to a log so it doesn't hold
/// `voxel launch`'s exec pipe open. Runs on every scrimlet (slot from the staged
/// filename)—but it's a no-op when the baked configs already match (see
/// `switch_enforcer`), so a matched-count launch behaves exactly as before.
fn maybe_start_switch_enforcer() -> Result<()> {
    let Some(slot) = staged_switch_slot() else {
        return Ok(());
    };
    let exe = std::env::current_exe().context("current_exe")?;
    let log =
        fs::File::create("/tmp/switch-enforcer.log").context("enforcer log")?;
    let mut cmd = Command::new(exe);
    cmd.arg("switch-enforcer")
        .arg(slot.to_string())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // New session so it survives this exec returning (no SIGHUP) and so its own
    // fds—not the launch pipe—are all that hold its stdio.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawn switch-enforcer")?;
    note(format!(
        "switch{slot} enforcer started (pid {}), log /tmp/switch-enforcer.log",
        child.id()
    ));
    Ok(())
}

/// This node's switch slot, from the `mgs-config-switch{N}.toml` `stage_config`
/// dropped into its cargo-bay—or `None` if it isn't a scrimlet.
fn staged_switch_slot() -> Option<u8> {
    for entry in fs::read_dir(CARGO_BAY).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_str()?;
        if let Some(rest) = name.strip_prefix("mgs-config-switch")
            && let Some(slot) =
                rest.strip_suffix(".toml").and_then(|d| d.parse::<u8>().ok())
        {
            return Some(slot);
        }
    }
    None
}

/// Whether the switch zone is fully installed and running. Before that, the
/// zone install can still rewrite the baked configs under us.
fn switch_zone_running() -> bool {
    std::process::Command::new("zoneadm")
        .args(["-z", "oxz_switch", "list", "-p"])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split(':')
                .nth(2)
                .is_some_and(|state| state == "running")
        })
}

/// The detached enforcer (runs as voxel-init switch-enforcer <slot>). Forces
/// this scrimlet's launch-count MGS (switch{slot}) + sp-sim configs into the
/// switch zone, restarting each service, until the live files match what we
/// staged. Judges nothing until the zone RUNS: the install's package
/// extraction rewrites the baked configs, so an early file match is
/// meaningless and an early restart fails. Output -> /tmp/switch-enforcer.log.
pub fn switch_enforcer(slot: u8) {
    let mgs_staged = format!("{CARGO_BAY}/mgs-config-switch{slot}.toml");
    let sp_staged = format!("{CARGO_BAY}/sp-sim-config.toml");
    let mut mgs_restarted = true;
    let mut sp_restarted = true;
    for _ in 0..1500 {
        // up to ~25 min safety net
        if !switch_zone_running() || !Utf8Path::new(SWITCH_ZONE_MGS).exists() {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let mgs_ok = files_equal(SWITCH_ZONE_MGS, &mgs_staged);
        let sp_present = Utf8Path::new(SWITCH_ZONE_SP).exists();
        // No staged sp-sim config (e.g. --emu, where sp-sim is disabled below)
        // -> nothing for the enforcer to reconcile.
        let sp_ok = !Utf8Path::new(&sp_staged).exists()
            || !sp_present
            || files_equal(SWITCH_ZONE_SP, &sp_staged);
        if mgs_ok && sp_ok && mgs_restarted && sp_restarted {
            note(format!("switch{slot} + sp-sim configs in place"));
            break;
        }
        if !mgs_ok || !mgs_restarted {
            if !mgs_ok && let Err(e) = fs::copy(&mgs_staged, SWITCH_ZONE_MGS) {
                warn(format!("copy switch{slot} MGS config: {e}"));
            }
            mgs_restarted = run(
                "zlogin",
                &["oxz_switch", "svcadm", "restart", "svc:/oxide/mgs:default"],
            );
        }
        if sp_present && (!sp_ok || !sp_restarted) {
            if !sp_ok && let Err(e) = fs::copy(&sp_staged, SWITCH_ZONE_SP) {
                warn(format!("copy sp-sim config: {e}"));
            }
            sp_restarted = run(
                "zlogin",
                &[
                    "oxz_switch",
                    "svcadm",
                    "restart",
                    "svc:/oxide/sp-sim:default",
                ],
            );
        }
        note(format!("forced switch{slot} / sp-sim configs"));
        std::thread::sleep(Duration::from_secs(1));
    }
    disable_sp_sim_for_emu();
    open_switch_zone_ssh();
    monitor_switch_zone(slot);
}

/// An --emu rack has no use for sp-sim: MGS dials the fleet on the falcon host,
/// so a baked sp-sim would sit on loopback answering nobody. TUF images bake
/// none, but an image built from a commit or --src carries omicron's, so this is
/// not dead. The staged SP network address is what marks the rack as --emu.
fn disable_sp_sim_for_emu() {
    if !Utf8Path::new(&format!("{CARGO_BAY}/sp-net")).exists() {
        return;
    }
    run(
        "zlogin",
        &["oxz_switch", "svcadm", "disable", "-s", "svc:/oxide/sp-sim:default"],
    );
}

/// Resident watch, in the global zone, so it survives switch-zone recreation.
/// A sled-agent restart recreates oxz_switch from the BAKED config, which names
/// this scrimlet switch0 and points MGS at loopback; the staged config names its
/// real slot and points MGS at the rack's SP fleet on the falcon host. Left
/// alone, a scrimlet bounce would darken that switch and wedge the rack, so
/// re-assert the staged config, recover the fabric, and reopen zone ssh.
fn monitor_switch_zone(slot: u8) {
    let staged = format!("{CARGO_BAY}/mgs-config-switch{slot}.toml");
    loop {
        std::thread::sleep(Duration::from_secs(20));
        // Act only once the zone is back and is actually running baked config.
        if !switch_zone_running()
            || !Utf8Path::new(&staged).exists()
            || files_equal(SWITCH_ZONE_MGS, &staged)
        {
            continue;
        }
        note("switch zone recreated with the baked MGS config; re-asserting");
        if let Err(e) = fs::copy(&staged, SWITCH_ZONE_MGS) {
            warn(format!("re-copy switch{slot} MGS config: {e}"));
            continue;
        }
        run(
            "zlogin",
            &["oxz_switch", "svcadm", "restart", "svc:/oxide/mgs:default"],
        );
        recover_fabric();
        open_switch_zone_ssh();
        note(format!("switch{slot} re-asserted after zone recreation"));
    }
}

/// Recover the softnpu dataplane after a switch-zone recreation: reload the P4
/// program into propolis, bounce dendrite/tfport (they bind the ASIC), kick
/// mg-ddm (underlay routes) and mgd (BGP). Mirrors the manual recipe; idempotent.
fn recover_fabric() {
    run(
        "/opt/oxide/sidecar/scadm",
        &["propolis", "load-program", "/opt/oxide/sidecar/libsidecar_lite.so"],
    );
    run(
        "zlogin",
        &[
            "oxz_switch",
            "svcadm",
            "restart",
            "svc:/oxide/dendrite:default",
            "svc:/oxide/tfport:default",
        ],
    );
    run("svcadm", &["restart", "svc:/oxide/mg-ddm:default"]);
    run(
        "zlogin",
        &["oxz_switch", "svcadm", "restart", "svc:/oxide/mgd:default"],
    );
}

fn files_equal(a: &str, b: &str) -> bool {
    matches!((fs::read(a), fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// Paths to the switch zone's sshd_config and login defaults from the global
/// zone.
const SWITCH_ZONE_SSHD: &str = "/zone/oxz_switch/root/etc/ssh/sshd_config";
const SWITCH_ZONE_LOGIN: &str = "/zone/oxz_switch/root/etc/default/login";

/// Open the switch zone's sshd to the lab posture (root, empty password,
/// forwarding scoped to the commission API), mirroring the global-zone
/// `setup_ssh`. The commission API binds only in-zone loopback, so the host
/// reaches it by forwarding through this sshd. Idempotent.
fn open_switch_zone_ssh() {
    if !Utf8Path::new(SWITCH_ZONE_SSHD).exists() {
        return;
    }
    run("zlogin", &["oxz_switch", "passwd", "-d", "root"]);
    replace_in_file(
        SWITCH_ZONE_SSHD,
        &[
            ("PasswordAuthentication no", "PasswordAuthentication yes"),
            ("PermitEmptyPasswords no", "PermitEmptyPasswords yes"),
            ("PermitRootLogin no", "PermitRootLogin yes"),
            ("AllowTcpForwarding no", "AllowTcpForwarding yes"),
            ("PermitOpen none", "PermitOpen [::1]:12234"),
            ("AllowUsers wicket support\n", "AllowUsers wicket support root\n"),
        ],
    );
    // login rejects the now-empty root password under PASSREQ=YES, which
    // breaks bare `zlogin oxz_switch` (and `voxel tp login`); allow it.
    replace_in_file(SWITCH_ZONE_LOGIN, &[("PASSREQ=YES", "PASSREQ=NO")]);
    run(
        "zlogin",
        &["oxz_switch", "svcadm", "restart", "svc:/network/ssh:default"],
    );
}

/// SMF-service entry point—the baked `svc:/oxide/voxel-switch-enforcer`, run on
/// **every boot**. This is the reboot/restart-safe path: the one-shot detached
/// enforcer (`maybe_start_switch_enforcer`) is lost if the sled is restarted or
/// its process is killed under load mid-bring-up—and then the scrimlet silently
/// reverts to the baked switch0, which wedges that rack's Nexus handoff
/// ("switch-port qsfp0 not found"). As an SMF service, startd re-runs it at every
/// boot and restarts it if it dies, so the slot identity can't be silently lost.
/// It reads the desired slot from the (persistent, host-backed) cargo-bay, so it's
/// a no-op on gimlets and on switch0 (content-equality), and idempotent if the
/// detached enforcer already applied it.
pub fn switch_enforcer_svc() {
    // The cargo-bay 9p mount is present from boot on a real sled; on the image
    // BUILD VM it never appears, so bail fast rather than hang the build/boot.
    let mut waited = 0;
    while !Utf8Path::new(SLED_CFG).exists() {
        if waited >= 30 {
            note("switch-enforcer-svc: no cargo-bay mount; nothing to enforce");
            park();
        }
        std::thread::sleep(Duration::from_secs(2));
        waited += 2;
    }
    match staged_switch_slot() {
        Some(slot) => {
            note(format!("switch-enforcer-svc: enforcing switch{slot}"));
            switch_enforcer(slot);
        }
        None => note(
            "switch-enforcer-svc: no switch slot staged (gimlet); nothing to do",
        ),
    }
    park();
}

/// The service runs under the SMF wait model: the process is the service, so
/// exiting reads as a death and loops the restarter. Paths with nothing left
/// to monitor park instead.
fn park() -> ! {
    note("switch-enforcer-svc: parked");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
