// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Falcon topology construction (driven by [`VoxelConfig`]) and the per-launch
//! cargo-bay staging that feeds it (generated sled/RSS/FRR/switch1 config +
//! sprockets keys).

use anyhow::{Context, anyhow, bail};
use attest_mock::MockData;
use camino::{Utf8Path, Utf8PathBuf};
use indoc::formatdoc;
use libfalcon::{NodeRef, Runner, SmbiosType1Input, unit::gb};
use std::fs;
use voxel_config::{
    SledDataLinksSchema, SledDesc, SledDisksSchema, VoxelConfig,
};

pub(crate) struct Topo {
    pub(crate) runner: Runner,
    pub(crate) sleds: Vec<(SledDesc, NodeRef)>,
    pub(crate) routers: Vec<(String, NodeRef)>,
}

impl Topo {
    pub(crate) fn node_ref(&self, name: &str) -> Option<NodeRef> {
        self.sleds
            .iter()
            .find(|(s, _)| s.name == name)
            .map(|(_, n)| *n)
            .or_else(|| {
                self.routers.iter().find(|(r, _)| r == name).map(|(_, n)| *n)
            })
    }

    /// Each rack's RSS node (its first bootstrap sled), in rack order. One per
    /// rack - each is an independent RSS domain to watch.
    pub(crate) fn rss_sleds(&self) -> Vec<&(SledDesc, NodeRef)> {
        let mut seen = std::collections::BTreeSet::new();
        self.sleds
            .iter()
            .filter(|(s, _)| s.rss && seen.insert(s.rack))
            .collect()
    }
}

/// Wire a node's external NIC. Precedence: `$EXT_INTERFACE` env, then the
/// config-driven link (the voxel-managed stub in isolated mode), then falcon's
/// default (the host's default-route interface).
fn ext_interface(
    d: &mut Runner,
    n: NodeRef,
    cfg_link: Option<&str>,
) -> anyhow::Result<()> {
    if let Ok(ifx) = std::env::var("EXT_INTERFACE") {
        d.ext_link(&ifx, n);
    } else if let Some(ifx) = cfg_link {
        d.ext_link(ifx, n);
    } else {
        d.default_ext_link(n).map_err(|e| {
            anyhow!("failed to find default external interface: {e}")
        })?;
    }
    Ok(())
}

/// Fill in the SMBIOS type-1 for a given sled. The manufacturer must be
/// `a4x2`, the one string omicron's `sled-hardware` reads SMBIOS identity
/// from (anything else falls back to the hostname). We could teach omicron
/// more strings, such as "voxel", in the future.
///
/// Serial and model must match what MGS reports for the sled's SP (sp-sim
/// config or sp-emu VPD): wicketd correlates each sled's bootstrap address
/// by BaseboardId: part number plus serial, so a mismatch leaves the sled's
/// bootstrap address unknown in rack setup. Revision 2 matches the sp-emu
/// VPD barcode rev 002; BaseboardId does not compare it.
fn populate_smbios(d: &mut Runner, x: NodeRef, sled: &SledDesc) {
    d.set_smbios_type1(
        x,
        SmbiosType1Input {
            manufacturer: "a4x2".to_string(),
            product_name: sled.part_number.clone(),
            serial_number: sled.serial_number.clone(),
            version: 2,
        },
    );
}

/// Build the falcon topology from config. The link/softnpu ordering is
/// significant: it determines the `enp0sN` interface names the generated
/// `frr.conf` targets (see `VoxelConfig::to_frr`), so preserve it.
pub(crate) fn build_topo(
    cfg: &VoxelConfig,
    name: &str,
) -> anyhow::Result<Topo> {
    let cp_img = cfg.image.cp_image();
    let frr_img = cfg.image.frr_image();

    let mut d = Runner::new(name);
    d.persistent = true;
    // Falcon skips its on-demand propolis download once a path is set, so an
    // unset knob keeps the released binary.
    if let Some(bin) = &cfg.falcon.propolis_binary {
        d.set_propolis_binary(Some(bin.clone()));
    }

    // Sleds (voxel-cp) and routers (voxel-frr). Guest RAM is configurable so a
    // bigger rack can shrink per-sled memory to fit physical RAM (VMM Memory is
    // the dominant consumer); see the launch memory preflight.
    let sled_mem = gb(cfg.topology.sled_memory_gb);
    let router_mem = gb(cfg.topology.router_memory_gb);
    let mut sleds = Vec::new();
    let dataset = crate::image::falcon_dataset();
    for s in cfg.sleds() {
        let n = d.node(&s.name, &cp_img, 8, sled_mem);
        d.reserve(n, cfg.topology.sled_disk_gb as usize);
        // The sled's complement of real NVMe disks. The zvols behind them are
        // created at launch (see crate::disks); describing the devices here is
        // inert for the non-launch callers of build_topo.
        crate::disks::attach(&mut d, &dataset, name, &s, n)?;
        sleds.push((s, n));
    }
    let mut routers = Vec::new();
    for r in &cfg.topology.routers {
        let n = d.node(r, &frr_img, 4, router_mem);
        d.reserve(n, 20);
        routers.push((r.clone(), n));
    }

    // Isolated mode wires every external NIC onto the voxel-managed etherstub
    // instead of the host LAN ($EXT_INTERFACE still wins inside ext_interface).
    let ext_if =
        cfg.external.isolated().then_some(crate::isolated_external::STUB);

    let all_scrimlets: Vec<NodeRef> =
        sleds.iter().filter(|(s, _)| s.scrimlet).map(|(_, n)| *n).collect();
    let ce = routers.iter().find(|(r, _)| r == "ce").map(|(_, n)| *n);
    let fabric_routers: Vec<(String, NodeRef)> =
        routers.iter().filter(|(r, _)| r != "ce").cloned().collect();

    // Customer edge ↔ routers, then the edge's external uplink.
    if let Some(ce) = ce {
        for (_, r) in &fabric_routers {
            d.link(ce, *r);
        }
        ext_interface(&mut d, ce, ext_if)?;
    }

    // SoftNPU fabric. Each sled links ONLY its own rack's scrimlets - the
    // underlay/bootstrap network is per rack, so the two racks are independent RSS
    // domains. The fabric routers (cr*) link EVERY scrimlet across all racks: that
    // shared transit is what links the racks (eBGP re-advertises each rack's
    // prefix to the other). Sleds are wired first, in index order, so each sled's
    // first softnpu link still carries MAC byte `2*index+1` (its bootstrap addr).
    let mut mac_counter = 0u8;
    let mut new_mac = || {
        mac_counter += 1;
        format!("a8:40:25:00:00:{mac_counter:02}")
    };
    for (s, n) in &sleds {
        for sc in sleds
            .iter()
            .filter(|(o, _)| o.scrimlet && o.rack == s.rack)
            .map(|(_, m)| *m)
        {
            d.softnpu_link(sc, *n, Some(new_mac()), None);
        }
        ext_interface(&mut d, *n, ext_if)?;
    }
    for (_, n) in &fabric_routers {
        for sc in &all_scrimlets {
            d.softnpu_link(*sc, *n, Some(new_mac()), None);
        }
        ext_interface(&mut d, *n, ext_if)?;
    }

    // Cross-rack sidecar interconnects: a direct sidecar<->sidecar link per
    // cross-rack scrimlet pair (auto full mesh when racks > 1; see
    // `Topology::interconnect_pairs`). Wired AFTER the fabric uplinks so dpd
    // assigns each the next front (qsfp) tfport. `softnpu_links`
    // (plural) is the ASIC-to-ASIC form - both ends get a MAC, so neither
    // scrimlet gains a viona NIC (which would shift the external vioif index and
    // break the gimlets' hardcoded DHCP interface).
    for (ai, bi) in cfg.topology.interconnect_pairs() {
        let node = |idx: usize| {
            sleds.iter().find(|(s, _)| s.index == idx).map(|(_, n)| *n)
        };
        if let (Some(a), Some(b)) = (node(ai), node(bi)) {
            d.softnpu_links(a, b, Some(new_mac()), Some(new_mac()));
        }
    }

    // SMBIOS + cargo-bay mounts.
    for (s, n) in &sleds {
        populate_smbios(&mut d, *n, s);
        d.mount(format!("{CARGO_BAY}/{}", s.name), "/opt/cargo-bay", *n)
            .map_err(|e| anyhow!("mount {}: {e}", s.name))?;
    }
    for (r, n) in &routers {
        d.mount_linux(format!("{CARGO_BAY}/{r}"), "/opt/cargo-bay", *n)
            .map_err(|e| anyhow!("mount_linux {r}: {e}"))?;
    }

    Ok(Topo { runner: d, sleds, routers })
}

/// Host-side cargo-bay root (per-node staging dirs live under `<CARGO_BAY>/<node>`,
/// mounted into each guest at `/opt/cargo-bay`).
const CARGO_BAY: &str = "./cargo-bay";

/// Host-side staging root for the emulated SP fleet, one directory per rack. The
/// fleet runs here on the falcon host rather than inside a switch zone, so a
/// rack's SPs outlive the sled reboots they cause and both switch zones share
/// one flash instead of keeping private copies that drift.
const SP_FLEET_DIR: &str = "./sp-fleet";

fn cargo_bay(node: &str) -> Utf8PathBuf {
    Utf8Path::new(CARGO_BAY).join(node)
}

/// The emulated SP fleet for one rack: the sidecar plus one SP per rack sled,
/// addressed at the fleet the falcon host runs. The single construction point,
/// so staging, the host fleet and the operator commands cannot disagree about
/// identities or ports.
pub(crate) fn emu_fleet(
    cfg: &VoxelConfig,
    rack: usize,
) -> voxel_config::sp::SpFleet {
    let indices: Vec<usize> = cfg
        .sleds()
        .iter()
        .filter(|s| s.rack == rack)
        .map(|s| s.index)
        .collect();
    voxel_config::sp::SpFleet::for_gimlets(
        &indices,
        voxel_config::sp::SpBackend::Emu {
            addr: voxel_config::config::sp_host_addr(rack),
        },
    )
}

/// A rack's host-side SP fleet directory.
pub(crate) fn sp_fleet_dir(rack: usize) -> Utf8PathBuf {
    Utf8Path::new(SP_FLEET_DIR).join(format!("r{rack}"))
}

/// Clear each node's cargo-bay before staging so it reflects ONLY the current
/// topology. Otherwise files from a prior launch with a different topology
/// linger: e.g. a 3-sled run (scrimlets g0+g2) stages `mgs-config-switch1.toml`
/// into `cargo-bay/g2`, and a later 4-sled run (scrimlets g0+g3) wouldn't
/// overwrite it - so voxel-init on g2 would find the stale file and start a
/// pointless switch1 enforcer. Wiping first guarantees a clean, correct stage.
pub(crate) fn reset_node_cargo_bay(cfg: &VoxelConfig) -> anyhow::Result<()> {
    let mut nodes: Vec<String> =
        cfg.sleds().into_iter().map(|s| s.name).collect();
    nodes.extend(cfg.topology.routers.iter().cloned());
    for node in nodes {
        let dir = cargo_bay(&node);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("reset {}", dir))?;
        }
        fs::create_dir_all(&dir)?;
    }
    Ok(())
}

/// Render config-rss.toml through omicron's own types (the rack-init-config
/// crate, pinned to an omicron commit).
fn generate_rss_config(
    cfg: &VoxelConfig,
    dir: &Utf8Path,
    rack: usize,
) -> anyhow::Result<()> {
    let rendered = crate::rss_request::config_rss_toml(cfg, rack)?;
    fs::write(dir.join("config-rss.toml"), rendered)?;
    Ok(())
}

/// Auto-detect the sled-agent config shapes (`data_links`, disks) from the
/// image's omicron source, so operators never hand-set per-era knobs. Reads
/// `sled-agent/src/config.rs` from `$VOXEL_OMICRON_SRC`, derived in
/// `resolve_falcon_env` from the build root and `image.cp`'s commit, and keys
/// off the field declarations. Falls back to the oldest shapes if the source
/// can't be read; an explicit `[image]` override wins over detection.
///
/// This is the schema changelog, automated: instead of a hand-maintained
/// commits to requirements table, voxel reads what the commit itself declares.
/// It is the only place voxel consults an omicron checkout, and it is
/// advisory. A rack whose source is absent still launches.
pub(crate) const PROP_DATA_LINKS: &str = "voxel:data-links-schema";
pub(crate) const PROP_DISKS: &str = "voxel:disks-schema";
/// TUF system version stamped on `--from-tuf` images: the repo whose zone and
/// corpus artifacts the image's bytes hash match.
pub(crate) const PROP_TUF_VERSION: &str = "voxel:tuf-version";
/// Directory of SP/RoT firmware `image create --from-tuf` extracted from the
/// same repo. `--emu` launches boot this unless `[sp]` overrides it, so a rack
/// cannot run firmware that disagrees with the release it reports.
pub(crate) const PROP_TUF_FW: &str = "voxel:tuf-fw";

/// The firmware `image create --from-tuf` extracted for this image, if it is a
/// TUF image built by a voxel that stamped the directory.
pub(crate) fn tuf_firmware(image: &str) -> Option<Utf8PathBuf> {
    let ds = format!("{}/img/{image}", crate::image::falcon_dataset());
    let out = std::process::Command::new("zfs")
        .args(["get", "-H", "-o", "value", PROP_TUF_FW])
        .arg(&ds)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dir = Utf8PathBuf::from(dir);
    dir.is_dir().then_some(dir)
}

/// Sled-agent config schema read from an omicron checkout; None if the
/// checkout's sled-agent config source is not readable.
pub(crate) fn schema_from_checkout(
    src_root: &Utf8Path,
) -> Option<(SledDataLinksSchema, SledDisksSchema)> {
    let read = |rel: &str| fs::read_to_string(src_root.join(rel)).ok();
    let src = read("sled-agent/src/config.rs")?;
    // Affirmative-only: each era must match its own marker; an unrecognized
    // config shape is no answer, never an assumed era. The variants of
    // `ExternalDisks` are declared in sled-hardware, so read the enum itself
    // rather than the field that references it; `Virtual { vdevs }` and
    // `HardcodedPhysical { disks }` merged into `Hardcoded { vdevs, disks }`.
    let disks = if src.contains("pub external_disks") {
        if read("sled-hardware/src/lib.rs")
            .unwrap_or_default()
            .contains("Hardcoded {")
        {
            SledDisksSchema::Hardcoded
        } else {
            SledDisksSchema::ExternalDisks
        }
    } else if src.contains("pub vdevs") {
        SledDisksSchema::Vdevs
    } else {
        return None;
    };
    // `data_links: DataLinks` (tagged enum) vs the older flat list.
    let data_links = if src.contains("data_links: DataLinks") {
        SledDataLinksSchema::Tagged
    } else if src.contains("data_links") {
        SledDataLinksSchema::List
    } else {
        return None;
    };
    Some((data_links, disks))
}

/// The schema stamp `image create` wrote on the image dataset; None on a
/// pre-stamp image.
fn schema_from_image_props(
    image: &str,
) -> Option<(SledDataLinksSchema, SledDisksSchema)> {
    let ds = format!("{}/img/{image}", crate::image::falcon_dataset());
    let out = std::process::Command::new("zfs")
        .args(["get", "-H", "-o", "property,value"])
        .arg(format!("{PROP_DATA_LINKS},{PROP_DISKS}"))
        .arg(&ds)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut data_links = None;
    let mut disks = None;
    for line in text.lines() {
        match *line.split('\t').collect::<Vec<_>>().as_slice() {
            [PROP_DATA_LINKS, v] => data_links = SledDataLinksSchema::parse(v),
            [PROP_DISKS, v] => disks = SledDisksSchema::parse(v),
            _ => {}
        }
    }
    Some((data_links?, disks?))
}

/// Copy the schema stamp from one image to another. Ok on a pre-stamp source
/// (nothing to copy); the destination then errors at launch like any
/// unstamped image.
pub(crate) fn copy_image_schema_props(
    src_image: &str,
    out_image: &str,
) -> anyhow::Result<()> {
    let Some((data_links, disks)) = schema_from_image_props(src_image) else {
        return Ok(());
    };
    let ds = format!("{}/img/{out_image}", crate::image::falcon_dataset());
    let status = std::process::Command::new("zfs")
        .arg("set")
        .arg(format!("{PROP_DATA_LINKS}={}", data_links.as_str()))
        .arg(format!("{PROP_DISKS}={}", disks.as_str()))
        .arg(&ds)
        .status()
        .with_context(|| format!("zfs set schema props on {ds}"))?;
    if !status.success() {
        anyhow::bail!("zfs set schema props on {ds} failed");
    }
    Ok(())
}

/// Pick the schema from the available sources: explicit config override wins
/// per field, then the image's zfs stamp, then the omicron checkout. No
/// source at all is an error; a guessed schema writes configs sled-agent
/// rejects at boot.
fn choose_schema(
    cfg: &VoxelConfig,
    image: &str,
    props: Option<(SledDataLinksSchema, SledDisksSchema)>,
    checkout: Option<(SledDataLinksSchema, SledDisksSchema)>,
) -> anyhow::Result<(SledDataLinksSchema, SledDisksSchema, &'static str)> {
    let (detected, source) = match (props, checkout) {
        (Some(v), _) => (Some(v), "image properties"),
        (None, Some(v)) => (Some(v), "omicron checkout"),
        (None, None) => (None, "config override"),
    };
    let data_links = cfg.image.data_links_schema.or(detected.map(|d| d.0));
    let disks = cfg.image.disks_schema.or(detected.map(|d| d.1));
    match (data_links, disks) {
        (Some(data_links), Some(disks)) => Ok((data_links, disks, source)),
        _ => anyhow::bail!(
            "no sled-agent config schema for image {image}: it carries no \
             voxel:*-schema zfs properties (re-create it with this voxel) \
             and no omicron checkout was found (set VOXEL_OMICRON_SRC or \
             [image] data_links_schema/disks_schema)"
        ),
    }
}

/// Resolve the sled-agent config schema for `image` from the live sources.
fn resolve_sled_schema(
    cfg: &VoxelConfig,
    image: &str,
) -> anyhow::Result<(SledDataLinksSchema, SledDisksSchema)> {
    let (data_links, disks, source) = choose_schema(
        cfg,
        image,
        schema_from_image_props(image),
        omicron_src().as_deref().and_then(schema_from_checkout),
    )?;
    eprintln!(
        "[voxel] sled-agent config schema ({source}): \
         data_links={data_links:?} disks={disks:?}"
    );
    Ok((data_links, disks))
}

/// Path of the omicron checkout the image was built from, if it's on disk.
pub(crate) fn omicron_src() -> Option<Utf8PathBuf> {
    std::env::var("VOXEL_OMICRON_SRC")
        .ok()
        .map(Utf8PathBuf::from)
        .filter(|p| p.is_dir())
}

/// Generate + stage per-node config into the cargo-bay before launch.
pub(crate) fn stage_config(
    cfg: &VoxelConfig,
    emu: bool,
    sp_firmware: Option<&Utf8Path>,
) -> anyhow::Result<()> {
    let sleds = cfg.sleds();
    // Per-sled sled-agent config (replaces a4x2's config/gN-config.toml). Each
    // scrimlet's SoftNPU links only its OWN rack's sleds (rear ports) + every
    // fabric router (front ports), so the rear-port budget is the PER-RACK sled
    // count (`topology.sleds`), not the deployment total. (For a single rack the
    // two are equal.) Fabric routers = every router except the customer edge `ce`.
    let num_sleds_per_rack = cfg.topology.sleds;
    let num_fabric_routers =
        cfg.topology.routers.iter().filter(|r| r.as_str() != "ce").count();
    let (data_links, disks) = resolve_sled_schema(cfg, &cfg.image.cp_image())?;
    for s in &sleds {
        let dir = cargo_bay(&s.name);
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("sled-config.toml"),
            s.sled_config(
                num_sleds_per_rack,
                num_fabric_routers,
                data_links,
                disks,
            )
            .with_interconnects(cfg.topology.interconnect_count_for(s.index))
            .render(),
        )?;
    }

    // One typed config-rss per rack, staged on that rack's RSS node (its first
    // bootstrap sled - g0 for rack 0, g{rack*sleds} for the rest). Each rack is
    // an independent RSS domain: the bootstrap set is filtered to that rack and
    // its customer/service network is offset per rack.
    for rack in 0..cfg.topology.racks() {
        let rss_node = sleds
            .iter()
            .find(|s| s.rss && s.rack == rack)
            .ok_or_else(|| anyhow!("rack {rack} has no RSS sled"))?;
        // For `--wicket-setup` we drive RSS through wicketd, so the config-rss
        // must NOT be injected by voxel-init (sled-agent would otherwise auto-init
        // from it). voxel-init only injects `<cargo-bay>/config-rss.toml`, so we
        // simply generate it OUTSIDE the cargo-bay (in `wicket-setup/rackN/`) -
        // `wicket_setup::drive` reads it from there to build the wicketd bodies.
        let rss_dir = if emu {
            let d = Utf8Path::new("wicket-setup").join(format!("rack{rack}"));
            fs::create_dir_all(&d)?;
            d
        } else if rack > 0 {
            // Multirack: rack 0 is the cluster; rack > 0 boots but does NOT RSS -
            // it's an unclaimed rack staged for a future cluster-join (RFD 573).
            // Generate its config-rss OUTSIDE the cargo-bay so voxel-init won't
            // auto-inject + RSS it; kept under multirack-staged/ for the join flow.
            let d =
                Utf8Path::new("multirack-staged").join(format!("rack{rack}"));
            fs::create_dir_all(&d)?;
            d
        } else {
            cargo_bay(&rss_node.name)
        };
        generate_rss_config(cfg, &rss_dir, rack)?;
    }

    // Per-router frr.conf.
    for (name, router) in cfg.to_frr() {
        let dir = cargo_bay(&name);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("frr.conf"), router.render())?;
    }

    // Static customer-edge address (if configured): voxel-init's router bring-up
    // adds it as a SECONDARY IP on ce's uplink, giving the host route a stable
    // nexthop. Staged only into ce's cargo-bay, so only ce picks it up.
    if let Some(ip) = &cfg.topology.ce_external_ip {
        let dir = cargo_bay("ce");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("ce-external-ip"), ip)?;
    }

    // Isolated mode: no DHCP server; instead stage each node's assigned static
    // address into its cargo-bay. voxel-init picks it up on both sled and router
    // roles. The router role also needs the interface name (routers can't jumbo-
    // probe their way to it the way sleds do); sleds self-classify.
    if cfg.external.isolated() {
        let prefix = cfg.external.prefix_length().ok_or_else(|| {
            anyhow!(
                "[external].subnet '{}' must be CIDR (a.b.c.d/len)",
                cfg.external.subnet
            )
        })?;
        let dns = cfg.external.dns.join(" ");
        let router_names: std::collections::HashSet<&str> =
            cfg.topology.routers.iter().map(String::as_str).collect();
        let assignments = cfg.static_external_ips();
        let expected = sleds.len() + cfg.topology.routers.len();
        if assignments.len() != expected {
            return Err(anyhow!(
                "[external].subnet too small: only {} static addresses fit from ip_start '{}' \
                 (need {}). Widen the subnet or lower the node count.",
                assignments.len(),
                cfg.external.ip_start,
                expected
            ));
        }
        for (node, ip) in assignments {
            let dir = cargo_bay(&node);
            fs::create_dir_all(&dir)?;
            let gateway = &cfg.external.host_ip;
            let mut body = formatdoc! {"
                ip {ip}/{prefix}
                gateway {gateway}
                dns {dns}
            "};
            if router_names.contains(node.as_str()) {
                body.push_str(&format!(
                    "iface {}\n",
                    cfg.router_ext_iface(&node)
                ));
            }
            fs::write(dir.join("external-net"), body)?;
        }
    }

    // Bake-once, PER RACK: the image bakes switch0 + sp-sim for a FIXED gimlet
    // count, but a launch can run any count - and each rack has its OWN
    // switch0/switch1 pair, so the switch slot is rack-LOCAL (0 or 1). For each
    // rack, generate that rack's two scrimlets' MGS config (switch0 for the 1st,
    // switch1 for the 2nd) + the sp-sim config, staged into each scrimlet's
    // cargo-bay; voxel-init swaps them into the switch zone at boot (+ restarts
    // mgs/sp-sim). The SP fleet is built for THAT rack's gimlet global indices, so
    // identities (serial/sprockets) stay aligned with the sleds.
    for rack in 0..cfg.topology.racks() {
        let rack_sleds: Vec<&SledDesc> =
            sleds.iter().filter(|s| s.rack == rack).collect();
        let gimlet_indices: Vec<usize> =
            rack_sleds.iter().map(|s| s.index).collect();
        let scrimlet_indices: Vec<usize> =
            rack_sleds.iter().filter(|s| s.scrimlet).map(|s| s.index).collect();
        // The SP fleet (sidecar + one SP per rack sled) is the shared MGS↔SP
        // contract; sim backend today, swappable to a real-firmware host.
        // `--emu`: every SP is real-firmware on sp-emu, run once for the rack on
        // the falcon host. Default: sp-sim in each switch zone, no emu staging.
        let fleet = if emu {
            emu_fleet(cfg, rack)
        } else {
            voxel_config::sp::SpFleet::sim_for_gimlets(&gimlet_indices)
        };
        // Firmware the image carries from its own TUF repo, so an --emu rack
        // runs the release it reports. --sp-firmware overrides it for a launch,
        // which is how the hubris lane tries a build before it ships.
        let fw = emu
            .then(|| {
                sp_firmware
                    .map(Utf8Path::to_path_buf)
                    .or_else(|| tuf_firmware(&cfg.image.cp_image()))
            })
            .flatten();
        for (slot, s) in rack_sleds.iter().filter(|s| s.scrimlet).enumerate() {
            let dir = cargo_bay(&s.name);
            fs::create_dir_all(&dir)?;
            fs::write(
                dir.join(format!("mgs-config-switch{slot}.toml")),
                voxel_config::mgs::switch_config(
                    slot as u8,
                    &fleet,
                    &scrimlet_indices,
                ),
            )?;
            // Stage sp-sim's config only in the default path; under --emu sp-sim is
            // disabled, so don't stage one (and the enforcer leaves sp-sim alone).
            if !emu {
                fs::write(
                    dir.join("sp-sim-config.toml"),
                    fleet.sp_sim_config(),
                )?;
            }
            // The fleet runs on the falcon host, so a scrimlet needs only an
            // address on its rack's SP network to reach it. The switch zone
            // already routes the bootstrap prefix to its own global zone.
            if emu {
                fs::write(
                    dir.join("sp-net"),
                    format!(
                        "{}/{}",
                        voxel_config::config::sp_scrimlet_addr(rack, s.index),
                        voxel_config::config::SP_NET_PREFIX_LEN
                    ),
                )?;
            }
        }
        // One fleet for the rack, staged on the host instead of in each zone.
        stage_sp_emu(cfg, &fleet, &sp_fleet_dir(rack), emu, fw.as_deref())?;
    }
    Ok(())
}

/// Stage the `sp-emu` binary + each emulated SP's flashed hubris image into the
/// rack's host fleet directory (`sp-emu/`), so the falcon host can run the
/// real-firmware SPs once for the whole rack. Each emu SP's image is flashed into `<base_port>.flash`
/// (the filename carries the MGS port; voxel-init derives the board from it).
/// No-op when no SP in the fleet is emulator-backed. Staged pre-launch, so it's
/// present at boot (no 9p-visibility issue).
fn stage_sp_emu(
    cfg: &VoxelConfig,
    fleet: &voxel_config::sp::SpFleet,
    dir: &Utf8Path,
    emu_rot: bool,
    fw: Option<&Utf8Path>,
) -> anyhow::Result<()> {
    let emu = fleet.emu_sps();
    if emu.is_empty() {
        return Ok(());
    }
    let out = dir.join("sp-emu");
    fs::create_dir_all(&out)?;
    // The host fleet needs the binary and archives here: there is no baked
    // in-guest copy to fall back on now that it runs outside the switch zone.
    // [sp].emu_bin overrides; unset fetches the pinned buildomat build.
    let emu_bin = crate::sp_host::ensure_emu_bin(cfg)?;
    fs::copy(&emu_bin, out.join("sp-emu"))
        .with_context(|| format!("stage sp-emu binary from {emu_bin}"))?;
    // Stage `faux-mgs` (the MGS client) alongside it, so `voxel sp
    // ls/state/exec` can talk to the live SPs. The operator commands need it,
    // launch itself does not, so an unavailable faux-mgs only warns.
    match crate::sp_host::ensure_faux_mgs(cfg) {
        Ok(faux) => {
            fs::copy(&faux, out.join("faux-mgs"))
                .with_context(|| format!("stage faux-mgs from {faux}"))?;
        }
        Err(e) => eprintln!(
            "[voxel] faux-mgs unavailable ({e:#}); sp commands will not work"
        ),
    }
    // Stage the RoT image so each SP can run oxide-rot-1 in-process over sprot
    // (sp-emu 1.x runs the RoT inside the SP process, not as a separate service).
    // The image's own firmware wins: an --emu rack should run the release it
    // reports. `[sp]` is the fallback for an image built without --from-tuf,
    // which carries no firmware of its own. Bound once, under its own name:
    // shadowing `dir` here left the archive staging below reading the hubris
    // zips out of its own output directory.
    let Some(fw_dir) = fw else {
        bail!(
            "--emu needs firmware: build the image with --from-tuf, or \
             point --sp-firmware at a directory of SP/RoT images"
        );
    };
    if emu_rot {
        let rot = fw_dir.join("rot-a.zip");
        fs::copy(&rot, out.join("rot.image"))
            .with_context(|| format!("stage RoT image from {rot}"))?;
        // Staged bootleby turns on sp-emu secure boot; rot_image must be
        // self-signed.
        let bootleby = fw_dir.join("bootleby.zip");
        if bootleby.exists() {
            fs::copy(&bootleby, out.join("bootleby.zip"))
                .with_context(|| format!("stage bootleby from {bootleby}"))?;
        }
    }
    // Stage each role's hubris archive; voxel-init flashes a per-instance state
    // directory from it in the zone (sp-emu 1.x flashes from the archive, not a
    // pre-built flash file). Gimlets share one archive; the sidecar has its own.
    let mut staged = std::collections::BTreeSet::new();
    for sp in emu {
        let role =
            if sp.selector() == "sidecar" { "sidecar" } else { "gimlet" };
        if !staged.insert(role) {
            continue;
        }
        // The repo names SP archives by hubris board.
        let image = fw_dir.join(match role {
            "sidecar" => "sp-sidecar-c.zip",
            _ => "sp-gimlet-c.zip",
        });
        fs::copy(&image, out.join(format!("{role}.archive")))
            .with_context(|| format!("stage {role} archive from {image}"))?;
    }
    // The release's host phase 1, staged for the gimlet QSPI seed (sp_host
    // writes it into each gimlet SP's host-boot flash so the slot inventories
    // at the repo's version instead of reading as blank). Cached by cpbuild
    // next to the firmware dir under the same key; an image whose caches
    // predate the rom just leaves host phase 1 unknown, as before.
    let key = fw_dir.file_name().unwrap_or_default();
    let rom = fw_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join(".tuf-host").join(format!("phase1-{key}.rom")));
    match rom {
        Some(rom) if rom.exists() => {
            fs::copy(&rom, out.join("host-phase1.rom"))
                .with_context(|| format!("stage host phase 1 from {rom}"))?;
        }
        _ => eprintln!(
            "[voxel] no cached host phase 1 rom for {key}; \
             gimlet host flash stays blank"
        ),
    }
    Ok(())
}

/// Generate the sprockets / trust-quorum test keys + measurements and stage each
/// sled's identity into `cargo-bay/<sled>/sprockets/`. Mirrors a4x2's
/// `NewSprocketsKeys` subcommand, but driven by the actual sled set rather than a
/// hardcoded `g0..g3` - so `voxel launch` no longer rsyncs them from a4x2. The
/// generated `sled-config.toml` (`voxel-config::sled`) points each sled at its
/// own index's files under `/opt/cargo-bay/sprockets`.
pub(crate) fn stage_sprockets(cfg: &VoxelConfig) -> anyhow::Result<()> {
    use camino::Utf8PathBuf;
    use sprockets_tls_test_utils as sprockets;

    let sleds = cfg.sleds();
    let base = Utf8PathBuf::from(CARGO_BAY);
    let src = base.join("sprockets");
    fs::create_dir_all(&src).with_context(|| format!("{src}"))?;

    let file_behavior = sprockets::OutputFileExistsBehavior::Overwrite;
    let doc = sprockets::generate_config_start_from_0(sleds.len());
    doc.write_key_pairs(src.clone(), file_behavior)
        .map_err(|e| anyhow!("{e}"))?;
    doc.write_certificates(src.clone(), file_behavior)
        .map_err(|e| anyhow!("{e}"))?;
    doc.write_certificate_lists(src.clone(), file_behavior)
        .map_err(|e| anyhow!("{e}"))?;

    // Fake attestation log + measurements. The digests are arbitrary (we don't
    // run a corpus); sled-agent only needs at least one measurement present, in a
    // file named test-sprockets-log.bin. Same constants a4x2 uses. The SP digest
    // is shared by the log and the corim's fake-sp entry; the fwid digest differs.
    const SP_DIGEST: &str =
        "be4df4e085175f3de0c8ac4837e1c2c9a34e8983209dac6b549e94154f7cdd9c";
    const FWID_DIGEST: &str =
        "72fa8f8ea84a42251031366002cbb36281d0131f78cd680436116a720cdd9de5";
    let attest_log =
        attest_mock::MockLog::from_document(attest_mock::log::Document {
            measurements: vec![attest_mock::log::Measurement {
                algorithm: "sha3-256".into(),
                digest: SP_DIGEST.into(),
            }],
        })
        .map_err(|e| anyhow!("attest log: {e}"))?
        .to_bytes()
        .map_err(|e| anyhow!("attest log serialization failed: {e}"))?;

    let corim =
        attest_mock::MockCorim::from_document(attest_mock::corim::Document {
            vendor: "Test Bed".into(),
            tag_id: "test-v0.0.99999".into(),
            id: "corim-test-v0.0.99999".into(),
            measurements: vec![
                attest_mock::corim::Measurement {
                    mkey: "fake-sp".into(),
                    algorithm: 10,
                    digest: SP_DIGEST.into(),
                },
                attest_mock::corim::Measurement {
                    mkey: "fake-fwid".into(),
                    algorithm: 10,
                    digest: FWID_DIGEST.into(),
                },
            ],
        })
        .map_err(|e| anyhow!("corim: {e}"))?
        .to_bytes()
        .map_err(|e| anyhow!("corim serialization failed: {e}"))?;

    // Distribute each sled's own identity index into its cargo-bay.
    for s in &sleds {
        let dst = base.join(&s.name).join("sprockets");
        fs::create_dir_all(&dst).with_context(|| format!("{dst}"))?;
        fs::write(dst.join("test-sprockets-log.bin"), &attest_log)
            .with_context(|| dst.join("test-sprockets-log.bin"))?;
        fs::write(dst.join("test-measurements.corim"), &corim)
            .with_context(|| dst.join("test-measurements.corim"))?;
        for (from, to) in sprockets::all_paths(src.clone(), s.index)
            .into_iter()
            .zip(sprockets::all_paths(dst.clone(), s.index))
        {
            fs::copy(&from, &to).with_context(|| format!("{from} -> {to}"))?;
        }
    }

    fs::remove_dir_all(&src).with_context(|| format!("{src}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in checkout carrying the two files the sled-schema detection
    /// reads: the sled-agent config field and the sled-hardware enum variants.
    fn fake_sled_src(name: &str, field: &str, variants: &str) -> Utf8PathBuf {
        let src =
            crate::util::temp_dir().join(format!("voxel-sledcheck-{name}"));
        let _ = fs::remove_dir_all(&src);
        fs::create_dir_all(src.join("sled-agent/src")).unwrap();
        fs::create_dir_all(src.join("sled-hardware/src")).unwrap();
        fs::write(
            src.join("sled-agent/src/config.rs"),
            format!("pub struct Config {{ {field} }}"),
        )
        .unwrap();
        fs::write(
            src.join("sled-hardware/src/lib.rs"),
            format!("pub enum ExternalDisks {{ {variants} }}"),
        )
        .unwrap();
        src
    }

    /// The disks shape moved twice. `HardcodedPhysical {` must not be mistaken
    /// for the newer `Hardcoded {`, or a cc07512e0-era image would be handed
    /// main's shape and refuse to boot.
    #[test]
    fn detects_the_disks_shape_per_era() {
        let disks = |src: &Utf8PathBuf| schema_from_checkout(src).unwrap().1;

        // Oldest: a flat `vdevs` list, no `external_disks` field at all.
        assert_eq!(
            disks(&fake_sled_src(
                "old",
                "pub vdevs: Vec<String>, data_links: [String; 2],",
                ""
            )),
            SledDisksSchema::Vdevs
        );
        // Middle: separate `Virtual` and `HardcodedPhysical` variants.
        assert_eq!(
            disks(&fake_sled_src(
                "mid",
                "pub external_disks: ExternalDisks, data_links: [String; 2],",
                "Virtual { vdevs: Vec<String> }, HardcodedPhysical { disks: Vec<D> }, DetectPhysical,",
            )),
            SledDisksSchema::ExternalDisks
        );
        // Current: the two merged into `Hardcoded { vdevs, disks }`.
        assert_eq!(
            disks(&fake_sled_src(
                "new",
                "pub external_disks: ExternalDisks, data_links: [String; 2],",
                "Hardcoded { vdevs: Vec<String>, disks: Vec<D> }, DetectPhysical,",
            )),
            SledDisksSchema::Hardcoded
        );
        // No checkout on disk: no answer, not a silent oldest-shape guess.
        assert_eq!(
            schema_from_checkout(Utf8Path::new("/nonexistent-checkout")),
            None
        );
        // Unrecognized shape (a future restructure): also no answer.
        assert_eq!(
            schema_from_checkout(&fake_sled_src(
                "odd",
                "pub storage: StoragePlan,",
                ""
            )),
            None
        );
    }

    const NEW: (SledDataLinksSchema, SledDisksSchema) =
        (SledDataLinksSchema::Tagged, SledDisksSchema::Hardcoded);
    const OLD: (SledDataLinksSchema, SledDisksSchema) =
        (SledDataLinksSchema::List, SledDisksSchema::Vdevs);

    /// The image stamp must beat the checkout, config overrides must beat
    /// both per field, and no source at all must be an error.
    #[test]
    fn schema_choice_precedence() {
        let cfg = VoxelConfig::default();
        let pick = |p, c| choose_schema(&cfg, "img", p, c);

        let (dl, d, src) = pick(Some(NEW), Some(OLD)).unwrap();
        assert_eq!((dl, d, src), (NEW.0, NEW.1, "image properties"));
        let (dl, d, src) = pick(None, Some(NEW)).unwrap();
        assert_eq!((dl, d, src), (NEW.0, NEW.1, "omicron checkout"));
        assert!(pick(None, None).is_err());

        let mut over = VoxelConfig::default();
        over.image.disks_schema = Some(SledDisksSchema::ExternalDisks);
        let (dl, d, _) = choose_schema(&over, "img", Some(NEW), None).unwrap();
        assert_eq!((dl, d), (NEW.0, SledDisksSchema::ExternalDisks));
        assert!(choose_schema(&over, "img", None, None).is_err());
    }

    /// A short image label must resolve through the build root to the
    /// full-sha checkout and its schema (a --commit image with a short label).
    #[test]
    fn short_label_resolves_through_build_root_to_schema() {
        let root = crate::util::temp_dir().join("voxel-schema-chain");
        let _ = fs::remove_dir_all(&root);
        let src = root.join("omicron-21dae8a64f00baa5deadbeef");
        fs::create_dir_all(src.join("sled-agent/src")).unwrap();
        fs::create_dir_all(src.join("sled-hardware/src")).unwrap();
        fs::write(
            src.join("sled-agent/src/config.rs"),
            "pub external_disks: ExternalDisks, data_links: DataLinks,",
        )
        .unwrap();
        fs::write(
            src.join("sled-hardware/src/lib.rs"),
            "pub enum ExternalDisks { Hardcoded { vdevs: V }, DetectPhysical }",
        )
        .unwrap();
        let found = crate::find_omicron_checkout(root.as_str(), "21dae8a64");
        assert_eq!(schema_from_checkout(Utf8Path::new(&found)), Some(NEW));
    }

    /// Live zfs stamp round-trip (write, read, copy). Needs a real dataset;
    /// run on the box: VOXEL_SCHEMA_ZFS_TEST=<dataset> cargo test -- --ignored
    #[test]
    #[ignore]
    fn zfs_stamp_round_trip() {
        let Ok(dataset) = std::env::var("VOXEL_SCHEMA_ZFS_TEST") else {
            panic!("set VOXEL_SCHEMA_ZFS_TEST=<dataset> to run");
        };
        let zfs = |args: &[&str]| {
            assert!(
                std::process::Command::new("zfs")
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "zfs {args:?}"
            );
        };
        for img in ["schema-test-a", "schema-test-b"] {
            let _ = std::process::Command::new("zfs")
                .args(["destroy", &format!("{dataset}/img/{img}")])
                .status();
            zfs(&["create", "-p", &format!("{dataset}/img/{img}")]);
        }
        // SAFETY: test-only process-global override of the dataset root.
        unsafe { std::env::set_var("FALCON_DATASET", &dataset) };
        zfs(&[
            "set",
            &format!("{PROP_DATA_LINKS}=tagged"),
            &format!("{PROP_DISKS}=hardcoded"),
            &format!("{dataset}/img/schema-test-a"),
        ]);
        assert_eq!(schema_from_image_props("schema-test-a"), Some(NEW));
        assert_eq!(schema_from_image_props("schema-test-b"), None);
        copy_image_schema_props("schema-test-a", "schema-test-b").unwrap();
        assert_eq!(schema_from_image_props("schema-test-b"), Some(NEW));
        for img in ["schema-test-a", "schema-test-b"] {
            zfs(&["destroy", &format!("{dataset}/img/{img}")]);
        }
    }
}
