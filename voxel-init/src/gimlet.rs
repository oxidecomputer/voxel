// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The control plane is already installed (`/opt/oxide`); this
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
const SLED_CFG: &str = "/opt/cargo-bay/sled-config.toml";
const PATCHED_CFG: &str = "/tmp/sled-config.toml";
/// Written at bake by image create --from-tuf.
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
        // Activate the unpacked control plane. omicron-package reads XTASK_BIN
        // and XTASK_DOWNLOADER_BIN from the environment.
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

/// Discover the sled disks, lay down the gimlet M.2 layout, seed the boot
/// image, and name the disks in the sled-agent config.
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
    /// The NVMe serial voxel gave the device, also the DiskIdentity serial.
    serial: String,
    m2: bool,
    /// Index within the variant: M.2 0/1 are slots A/B, U.2 0..4 are bays.
    index: usize,
    /// illumos disk name.
    disk: String,
    /// devices path of the blkdev node without a slice suffix, the form
    /// sled-agent wants in paths.devfs_path.
    devfs_path: String,
}

/// Read each disk's role and index from the NVMe serial voxel attached it
/// with. The controller path is used because the whole-disk link needs a label.
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
        // Serial form: <sled serial>-<M2|U2><index>. Others are not voxel's.
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
    /// The gimlet bay number, matching real hardware so inventory reads the
    /// same: M.2 A/B are 0x11/0x12.
    fn slot(&self) -> i64 {
        if self.m2 { 0x11 + self.index as i64 } else { self.index as i64 }
    }

    /// Which M.2 the sled booted from. A voxel guest still boots off falcon's
    /// own disk, so for now this is an assertion rather than an observation.
    fn is_boot_disk(&self) -> bool {
        self.m2 && self.index == 0
    }

    /// This disk as sled-agent's UnparsedDisk. next_active_slot is omitted:
    /// it is optional and TOML has no null.
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

const M2_PARTITIONS: usize = 6;

/// A prtvtoc partition line.
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

/// Give an M.2 the gimlet partition layout, idempotently, by reshaping the
/// default VTOC that zpool create writes.
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
    let usable = parts.iter().find(|p| p.index == 0).with_context(|| {
        format!("{}: no partition 0 after labeling", d.disk)
    })?;
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

/// Write the image's host phase 2 into the M.2 boot image partition so the
/// slot inventories at the image's repo version.
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
fn write_disk_config(disks: &[SledDisk]) -> Result<()> {
    let items: Vec<String> = disks.iter().map(SledDisk::config_entry).collect();
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

/// TUF images prestage sled-agent. Activation imports its SMF manifest,
/// whose default instance starts on import.
fn activate_native() {
    if !run("svccfg", &["import", "/opt/oxide/sled-agent/pkg/manifest.xml"]) {
        warn("svccfg import sled-agent manifest failed");
        return;
    }
    run_quiet("svcadm", &["enable", "svc:/oxide/sled-agent:default"]);
    note("sled-agent activated");
}

/// SSH setup for voxel host login. illumos sshd defaults differ from
/// debian's.
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

/// Scrimlets load the baked SoftNPU P4 program. Gimlets have no softnpu
/// device.
fn maybe_load_sidecar() {
    let scrimlet = fs::read_to_string(SLED_CFG)
        .map(|s| s.contains("# voxel role: scrimlet"))
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

/// The Oxide underlay is jumbo (MTU 9000). The vioif order depends on the
/// topology: the NICs that accept MTU 9000 are the underlay.
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

/// Patch this sled's config to the detected underlay links and seed the
/// xtask workspace config that virtual-hardware reads.
fn patch_sled_config(underlay: &[String], tuf: bool) -> Result<()> {
    let text = fs::read_to_string(SLED_CFG)
        .with_context(|| format!("read {SLED_CFG}"))?;
    let mut doc: toml_edit::DocumentMut =
        text.parse().with_context(|| format!("parse {SLED_CFG}"))?;
    if let Some(first) = underlay.first() {
        doc["data_link"] = toml_edit::value(first.as_str());
        // Replace only the devices in the staged data_links shape: an inline
        // table keeps its kind, a bare array is rewritten.
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

/// Whether ifc already carries an IPv6 link-local. ipadm rejects a global
/// v6 address on a link without one.
fn has_link_local(ifc: &str) -> bool {
    let Some(out) =
        capture("ipadm", &["show-addr", "-p", "-o", "addrobj,addr"])
    else {
        return false;
    };
    let prefix = format!("{ifc}/");
    out.lines().any(|l| l.starts_with(&prefix) && l.contains("fe80"))
}

/// Give a scrimlet its address on the rack's SP network, staged in
/// /opt/cargo-bay/sp-net as addr/prefixlen.
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
    // A global v6 address needs a link-local. Link-local only: no LAN prefix
    // is adopted.
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
    // Persistent: an SP reset restarts its sled, and MGS must still reach the
    // fleet afterwards.
    if run(
        "ipadm",
        &["create-addr", "-T", "static", "-a", addr, &format!("{ifc}/spnet")],
    ) {
        // Deprecated: the trust quorum records the peer address it sees, and no
        // bootstrap agent answers at this one. It still receives.
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

/// Bring up the non-underlay NICs toward the host LAN, never vioif0, which
/// the switch zone claims. Isolated mode is static, lan mode is DHCP.
fn setup_external_networking(other: &[String]) {
    if let Some(ext) = read_external_net() {
        let resolv: String =
            ext.dns.iter().map(|s| format!("nameserver {s}\n")).collect();
        if let Err(e) = fs::write("/etc/resolv.conf", resolv) {
            warn(format!("resolv.conf: {e}"));
        }

        match other.iter().find(|ifc| ifc.as_str() != "vioif0") {
            Some(ifc) => {
                // A prior launch's static address persists in /etc/ipadm and
                // ipadm create-addr refuses to add over it.
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
                // Persist the route. Clear prior persistent defaults first so a
                // relaunch does not stack entries in /etc/inet/static_routes.
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
        // Wipe any leftover /v4 addrobj
        run_quiet("ipadm", &["delete-addr", &format!("{ifc}/v4")]);
        run("ipadm", &["create-addr", "-T", "dhcp", &format!("{ifc}/v4")]);
    }
}

/// Delete every persistent default route, not just the one via the current
/// gateway.
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

/// Ephemeral emulated U.2 and M.2, never baked. Vdevs from a prior launch are
/// wiped first: falcon keeps the sled disk across destroy and relaunch.
fn setup_virtual_hardware() {
    let softnpu = [("SOFTNPU_MODE", "propolis")];
    run_env("./xtask", &["virtual-hardware", "destroy"], &softnpu);
    wipe_vdevs();
    if !run_env("./xtask", &["virtual-hardware", "create"], &softnpu) {
        warn("virtual-hardware create failed");
    }
}

/// Control plane zones that live in the install dataset. Their hashes must
/// match the target release TUF repo's zone artifacts.
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

/// Measurement corpus staged by image create --from-tuf, byte exact from the
/// target repo. Preferred over the embedded fake corpus when present.
const STAGED_CORPUS: &str = "/opt/oxide/measurements";

/// Embedded fake measurement corpus. A sled needs a non-empty manifest to be
/// eligible for noop image source conversion. Voxel TUF repos carry it.
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

/// Populate the M.2 install datasets before sled-agent starts. sled-agent reads
/// the install manifest once at startup and never reloads it. Best effort.
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
    let mupdate_uuid = capture("uuidgen", &[]).map(|u| u.trim().to_lowercase());
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
        // Stage the MUPdate override installinator leaves on every install
        // dataset, so a fresh rack starts in the same state (RFD 556).
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
        // Leave the pool imported. sled-agent cannot find an exported file
        // vdev pool, and it accepts an already imported one.
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

/// Keep vioif0, the SoftNPU packet source, unplumbed in the global zone so the
/// switch zone can claim it. Harmless on gimlets.
fn unplumb_softnpu_source() {
    run_quiet("ipadm", &["delete-addr", "vioif0/v4"]);
    run_quiet("ipadm", &["delete-if", "vioif0"]);
}

const SWITCH_ZONE_MGS: &str =
    "/zone/oxz_switch/root/var/svc/manifest/site/mgs/config.toml";
const SWITCH_ZONE_SP: &str =
    "/zone/oxz_switch/root/var/svc/manifest/site/sp-sim/config.toml";

/// Spawn the detached enforcer that swaps the staged slot MGS and sp-sim
/// configs into the switch zone once it is installed. No-op when they match.
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
    // New session: it survives this exec returning and holds no launch pipe
    // fds.
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

/// This node's switch slot, from the staged mgs-config-switch<N>.toml in its
/// cargo-bay. None on a gimlet.
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

/// Whether the switch zone is installed and running. Until then the zone
/// install still rewrites the baked configs.
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

/// The detached enforcer: force the staged MGS and sp-sim configs into the
/// running switch zone, restarting each service, until the live files match.
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
        // No staged sp-sim config, as on an --emu rack: nothing to reconcile.
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

/// Disable a baked sp-sim on an --emu rack, where MGS dials the fleet on the
/// falcon host. The staged SP network address marks the rack as --emu.
fn disable_sp_sim_for_emu() {
    if !Utf8Path::new(&format!("{CARGO_BAY}/sp-net")).exists() {
        return;
    }
    run(
        "zlogin",
        &["oxz_switch", "svcadm", "disable", "-s", "svc:/oxide/sp-sim:default"],
    );
}

/// Resident watch in the global zone. A recreated switch zone carries the baked
/// config: re-assert the staged one, recover the fabric, reopen zone ssh.
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

/// Whether the SoftNPU has a programmed pipeline: its local table holds the
/// rear link-locals. scadm blocks forever on an unprogrammed one.
fn sidecar_programmed() -> bool {
    capture(
        "timeout",
        &["5", "/opt/oxide/sidecar/scadm", "propolis", "dump-state"],
    )
    .is_some_and(|s| s.contains("fe80:"))
}

/// A restarted propolis has an empty SoftNPU. Load the P4 program when it is
/// missing, and recover the dataplane if the switch zone is already up.
fn ensure_sidecar_program() {
    let scrimlet = fs::read_to_string(SLED_CFG)
        .map(|s| s.contains(r#"sled_mode = "scrimlet""#))
        .unwrap_or(false);
    if !scrimlet || sidecar_programmed() {
        return;
    }
    note("switch-enforcer-svc: SoftNPU has no program; loading sidecar_lite");
    if switch_zone_running() {
        recover_fabric();
    } else {
        maybe_load_sidecar();
    }
}

/// Recover the SoftNPU dataplane: reload the P4 program, restart dendrite and
/// tfport, then mg-ddm and mgd. mgd must start after the tfports exist.
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
    wait_for_tfport();
    run("svcadm", &["restart", "svc:/oxide/mg-ddm:default"]);
    run(
        "zlogin",
        &[
            "oxz_switch",
            "svcadm",
            "restart",
            "svc:/oxide/mg-ddm:default",
            "svc:/oxide/mgd:default",
        ],
    );
}

/// Wait for the switch zone's tfport service to come back online. Its stop can
/// outlive the SMF timeout and land in maintenance, which is cleared here.
fn wait_for_tfport() {
    let tfport = "svc:/oxide/tfport:default";
    for _ in 0..60 {
        std::thread::sleep(Duration::from_secs(2));
        let state = capture(
            "zlogin",
            &["oxz_switch", "svcs", "-H", "-o", "state", tfport],
        );
        match state.as_deref() {
            Some("online") => return,
            Some("maintenance") => {
                run("zlogin", &["oxz_switch", "svcadm", "clear", tfport]);
            }
            _ => {}
        }
    }
    warn("tfport did not come back online; continuing");
}

fn files_equal(a: &str, b: &str) -> bool {
    matches!((fs::read(a), fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// Paths to the switch zone's sshd_config and login defaults from the global
/// zone.
const SWITCH_ZONE_SSHD: &str = "/zone/oxz_switch/root/etc/ssh/sshd_config";
const SWITCH_ZONE_LOGIN: &str = "/zone/oxz_switch/root/etc/default/login";

/// Open the switch zone's sshd to the lab posture: root, empty password,
/// forwarding to the commission API on zone loopback. Idempotent.
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
    // PASSREQ=YES rejects the empty root password at zlogin.
    replace_in_file(SWITCH_ZONE_LOGIN, &[("PASSREQ=YES", "PASSREQ=NO")]);
    run(
        "zlogin",
        &["oxz_switch", "svcadm", "restart", "svc:/network/ssh:default"],
    );
}

/// Entry point of the baked svc:/oxide/voxel-switch-enforcer service, run at
/// every boot. Reads the slot from the cargo-bay; no-op on gimlets and switch0.
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
    ensure_sidecar_program();
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

/// Under the SMF wait model the process is the service and an exit is a death.
/// Paths with nothing to monitor park.
fn park() -> ! {
    note("switch-enforcer-svc: parked");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
