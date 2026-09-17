// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Falcon topology construction and the per-launch cargo-bay staging.

use anyhow::{Context, anyhow, bail};
use attest_mock::MockData;
use camino::{Utf8Path, Utf8PathBuf};
use indoc::formatdoc;
use libfalcon::{NodeRef, Runner, SmbiosType1Input, unit::gb};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::process::Command;
use voxel_config::config::{SP_NET_PREFIX_LEN, sp_host_addr, sp_scrimlet_addr};
use voxel_config::sp::{SpBackend, SpFleet};
use voxel_config::{
    SledDataLinksSchema, SledDesc, SledDisksSchema, VoxelConfig, mgs,
};

use crate::image::falcon_dataset;
use crate::isolated_external::STUB;
use crate::rss_request::config_rss_toml;

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

    /// Each rack's RSS node (its first bootstrap sled), in rack order.
    pub(crate) fn rss_sleds(&self) -> Vec<&(SledDesc, NodeRef)> {
        let mut seen = BTreeSet::new();
        self.sleds
            .iter()
            .filter(|(s, _)| s.rss && seen.insert(s.rack))
            .collect()
    }
}

/// Wire a node's external NIC: EXT_INTERFACE, then cfg_link, then falcon's
/// default-route interface.
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

/// SMBIOS type 1. Manufacturer a4x2 is what sled-hardware reads identity from;
/// serial and part must match the SP's so wicketd correlates the BaseboardId.
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

/// Build the falcon topology. Link and softnpu order determines the enp0sN
/// names the generated frr.conf targets.
pub(crate) fn build_topo(
    cfg: &VoxelConfig,
    name: &str,
) -> anyhow::Result<Topo> {
    let cp_img = cfg.image.cp_image();
    let frr_img = cfg.image.frr_image();

    let mut d = Runner::new(name);
    d.persistent = true;
    // Falcon skips its on-demand propolis download once a path is set.
    if let Some(bin) = &cfg.falcon.propolis_binary {
        d.set_propolis_binary(Some(bin.clone()));
    }

    let sled_mem = gb(cfg.topology.sled_memory_gb);
    let router_mem = gb(cfg.topology.router_memory_gb);
    let mut sleds = Vec::new();
    let dataset = falcon_dataset();
    for s in cfg.sleds() {
        let n = d.node(&s.name, &cp_img, 8, sled_mem);
        d.reserve(n, cfg.topology.sled_disk_gb as usize);
        // The zvols behind the NVMe devices are created at launch.
        crate::disks::attach(&mut d, &dataset, name, &s, n)?;
        sleds.push((s, n));
    }
    let mut routers = Vec::new();
    for r in &cfg.topology.routers {
        let n = d.node(r, &frr_img, 4, router_mem);
        d.reserve(n, 20);
        routers.push((r.clone(), n));
    }

    // Isolated mode puts every external NIC on the voxel-managed etherstub.
    let ext_if = cfg.external.isolated().then_some(STUB);

    let all_scrimlets: Vec<NodeRef> =
        sleds.iter().filter(|(s, _)| s.scrimlet).map(|(_, n)| *n).collect();
    let ce = routers.iter().find(|(r, _)| r == "ce").map(|(_, n)| *n);
    let fabric_routers: Vec<(String, NodeRef)> =
        routers.iter().filter(|(r, _)| r != "ce").cloned().collect();

    // Customer edge to each fabric router, then the edge's external uplink.
    if let Some(ce) = ce {
        for (_, r) in &fabric_routers {
            d.link(ce, *r);
        }
        ext_interface(&mut d, ce, ext_if)?;
    }

    // SoftNPU fabric: each sled links only its own rack's scrimlets, each fabric
    // router links every scrimlet. Sleds go first so the MAC byte is 2*index+1.
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

    // Cross-rack sidecar interconnects, wired after the uplinks so dpd assigns
    // the next front tfport. Both ends get a MAC so neither gains a viona NIC.
    for (ai, bi) in cfg.topology.interconnect_pairs() {
        let node = |idx: usize| {
            sleds.iter().find(|(s, _)| s.index == idx).map(|(_, n)| *n)
        };
        if let (Some(a), Some(b)) = (node(ai), node(bi)) {
            d.softnpu_links(a, b, Some(new_mac()), Some(new_mac()));
        }
    }

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

/// Host-side staging root, one directory per node, mounted at /opt/cargo-bay.
const CARGO_BAY: &str = "./cargo-bay";

/// Host-side staging root for the emulated SP fleet, one directory per rack.
const SP_FLEET_DIR: &str = "./sp-fleet";

fn cargo_bay(node: &str) -> Utf8PathBuf {
    Utf8Path::new(CARGO_BAY).join(node)
}

/// The emulated SP fleet for one rack: the sidecar plus one SP per rack sled.
pub(crate) fn emu_fleet(cfg: &VoxelConfig, rack: usize) -> SpFleet {
    let indices: Vec<usize> = cfg
        .sleds()
        .iter()
        .filter(|s| s.rack == rack)
        .map(|s| s.index)
        .collect();
    SpFleet::for_gimlets(&indices, SpBackend::Emu { addr: sp_host_addr(rack) })
}

pub(crate) fn sp_fleet_dir(rack: usize) -> Utf8PathBuf {
    Utf8Path::new(SP_FLEET_DIR).join(format!("r{rack}"))
}

/// Wipe and recreate each node's cargo-bay so a prior topology's files (a
/// stale mgs-config-switch1.toml, say) cannot linger.
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

/// Sled-agent config schema stamps on an image dataset.
pub(crate) const PROP_DATA_LINKS: &str = "voxel:data-links-schema";
pub(crate) const PROP_DISKS: &str = "voxel:disks-schema";
/// TUF system version stamped on a --from-tuf image.
pub(crate) const PROP_TUF_VERSION: &str = "voxel:tuf-version";
/// Firmware directory image create --from-tuf extracted from the same repo.
pub(crate) const PROP_TUF_FW: &str = "voxel:tuf-fw";

fn image_dataset(image: &str) -> String {
    format!("{}/img/{image}", falcon_dataset())
}

/// zfs user properties on an image dataset as (property, value) pairs, None
/// if the dataset is absent.
fn image_props(image: &str, props: &[&str]) -> Option<Vec<(String, String)>> {
    let out = Command::new("zfs")
        .args(["get", "-H", "-o", "property,value"])
        .arg(props.join(","))
        .arg(image_dataset(image))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let pairs = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(p, v)| (p.to_string(), v.trim().to_string()))
        .collect();
    Some(pairs)
}

/// The firmware directory stamped on a --from-tuf image, if it still exists.
pub(crate) fn tuf_firmware(image: &str) -> Option<Utf8PathBuf> {
    let dir = Utf8PathBuf::from(tuf_fw_prop(image)?);
    dir.is_dir().then_some(dir)
}

/// Whether image was built by image create --from-tuf and so bakes no sp-sim.
pub(crate) fn is_tuf_image(image: &str) -> bool {
    tuf_fw_prop(image).is_some()
}

/// The stamped firmware directory, an unset property reported as None.
fn tuf_fw_prop(image: &str) -> Option<String> {
    image_props(image, &[PROP_TUF_FW])?
        .into_iter()
        .map(|(_, v)| v)
        .find(|v| !v.is_empty() && v != "-")
}

/// Sled-agent config schema read from an omicron checkout, None if the
/// source is unreadable or its shape unrecognized.
pub(crate) fn schema_from_checkout(
    src_root: &Utf8Path,
) -> Option<(SledDataLinksSchema, SledDisksSchema)> {
    let read = |rel: &str| fs::read_to_string(src_root.join(rel)).ok();
    let src = read("sled-agent/src/config.rs")?;
    // ExternalDisks is declared in sled-hardware; Hardcoded { vdevs, disks }
    // replaced the separate Virtual and HardcodedPhysical variants.
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
    let data_links = if src.contains("data_links: DataLinks") {
        SledDataLinksSchema::Tagged
    } else if src.contains("data_links") {
        SledDataLinksSchema::List
    } else {
        return None;
    };
    Some((data_links, disks))
}

/// The schema stamp image create wrote on the image dataset, None on an
/// unstamped image.
fn schema_from_image_props(
    image: &str,
) -> Option<(SledDataLinksSchema, SledDisksSchema)> {
    let mut data_links = None;
    let mut disks = None;
    for (prop, v) in image_props(image, &[PROP_DATA_LINKS, PROP_DISKS])? {
        match prop.as_str() {
            PROP_DATA_LINKS => data_links = SledDataLinksSchema::parse(&v),
            PROP_DISKS => disks = SledDisksSchema::parse(&v),
            _ => {}
        }
    }
    Some((data_links?, disks?))
}

/// Copy the schema stamp from one image to another. Ok on an unstamped
/// source; the destination then errors at launch like any unstamped image.
pub(crate) fn copy_image_schema_props(
    src_image: &str,
    out_image: &str,
) -> anyhow::Result<()> {
    let Some((data_links, disks)) = schema_from_image_props(src_image) else {
        return Ok(());
    };
    let ds = image_dataset(out_image);
    let status = Command::new("zfs")
        .arg("set")
        .arg(format!("{PROP_DATA_LINKS}={}", data_links.as_str()))
        .arg(format!("{PROP_DISKS}={}", disks.as_str()))
        .arg(&ds)
        .status()
        .with_context(|| format!("zfs set schema props on {ds}"))?;
    if !status.success() {
        bail!("zfs set schema props on {ds} failed");
    }
    Ok(())
}

/// Pick the schema: the config override wins per field, then the image stamp,
/// then the omicron checkout. No source at all is an error.
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
        _ => bail!(
            "no sled-agent config schema for image {image}: it carries no \
             voxel:*-schema zfs properties (re-create it with this voxel) \
             and no omicron checkout was found (set VOXEL_OMICRON_SRC or \
             [image] data_links_schema/disks_schema)"
        ),
    }
}

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

/// The omicron checkout the image was built from, if VOXEL_OMICRON_SRC names
/// one on disk.
pub(crate) fn omicron_src() -> Option<Utf8PathBuf> {
    std::env::var("VOXEL_OMICRON_SRC")
        .ok()
        .map(Utf8PathBuf::from)
        .filter(|p| p.is_dir())
}

/// Generate and stage per-node config into the cargo-bay before launch.
pub(crate) fn stage_config(
    cfg: &VoxelConfig,
    emu: bool,
    init_rss: bool,
    sp_firmware: Option<&Utf8Path>,
) -> anyhow::Result<()> {
    let sleds = cfg.sleds();
    let racks = cfg.topology.racks();
    let cp_image = cfg.image.cp_image();

    // Per-sled sled-agent config. A scrimlet's SoftNPU rear ports carry only
    // its own rack's sleds, so the budget is the per-rack count.
    let num_fabric_routers = cfg.fabric_router_count();
    let (data_links, disks) = resolve_sled_schema(cfg, &cp_image)?;
    for s in &sleds {
        let dir = cargo_bay(&s.name);
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("sled-config.toml"),
            s.sled_config(
                cfg.topology.sleds,
                num_fabric_routers,
                data_links,
                disks,
            )
            .with_interconnects(cfg.topology.interconnect_count_for(s.index))
            .render(),
        )?;
    }

    // One config-rss per rack. Under --init-rss rack 0's goes into its RSS
    // node's cargo-bay for sled-agent; otherwise it is rendered for inspection.
    for rack in 0..racks {
        let rss_node = sleds
            .iter()
            .find(|s| s.rss && s.rack == rack)
            .ok_or_else(|| anyhow!("rack {rack} has no RSS sled"))?;
        let inspect_dir =
            Utf8Path::new("wicket-setup").join(format!("rack{rack}"));
        let rss_dir = match (init_rss, rack) {
            (false, _) => inspect_dir,
            (true, 0) => {
                if inspect_dir.exists() {
                    fs::remove_dir_all(&inspect_dir)?;
                }
                cargo_bay(&rss_node.name)
            }
            // A held rack boots but never runs RSS.
            (true, _) => {
                if inspect_dir.exists() {
                    fs::remove_dir_all(&inspect_dir)?;
                }
                Utf8Path::new("multirack-staged").join(format!("rack{rack}"))
            }
        };
        fs::create_dir_all(&rss_dir)?;
        fs::write(
            rss_dir.join("config-rss.toml"),
            config_rss_toml(cfg, rack)?,
        )?;
    }

    for (name, router) in cfg.to_frr() {
        let dir = cargo_bay(&name);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("frr.conf"), router.render())?;
    }

    // voxel-init adds the static customer-edge address as a secondary IP on
    // ce's uplink, giving the host route a stable nexthop.
    if let Some(ip) = &cfg.topology.ce_external_ip {
        let dir = cargo_bay("ce");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("ce-external-ip"), ip)?;
    }

    // Isolated mode has no DHCP: stage each node's static address. Routers
    // also need the interface name; sleds self-classify.
    if cfg.external.isolated() {
        let prefix = cfg.external.prefix_length().ok_or_else(|| {
            anyhow!(
                "[external].subnet '{}' must be CIDR (a.b.c.d/len)",
                cfg.external.subnet
            )
        })?;
        let dns = cfg.external.dns.join(" ");
        let router_names: HashSet<&str> =
            cfg.topology.routers.iter().map(String::as_str).collect();
        let assignments = cfg.static_external_ips();
        let expected = sleds.len() + cfg.topology.routers.len();
        if assignments.len() != expected {
            bail!(
                "[external].subnet too small: only {} static addresses fit from ip_start '{}' \
                 (need {}). Widen the subnet or lower the node count.",
                assignments.len(),
                cfg.external.ip_start,
                expected
            );
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

    // Per rack: each scrimlet's MGS config for its rack-local switch slot plus
    // the SP fleet, sp-sim in the switch zones or sp-emu on the host.
    for rack in 0..racks {
        let rack_sleds: Vec<&SledDesc> =
            sleds.iter().filter(|s| s.rack == rack).collect();
        let gimlet_indices: Vec<usize> =
            rack_sleds.iter().map(|s| s.index).collect();
        let scrimlet_indices: Vec<usize> =
            rack_sleds.iter().filter(|s| s.scrimlet).map(|s| s.index).collect();
        let fleet = if emu {
            emu_fleet(cfg, rack)
        } else {
            SpFleet::sim_for_gimlets(&gimlet_indices)
        };
        // --sp-firmware overrides the firmware the image carries from its TUF repo.
        let fw = emu
            .then(|| {
                sp_firmware
                    .map(Utf8Path::to_path_buf)
                    .or_else(|| tuf_firmware(&cp_image))
            })
            .flatten();
        for (slot, s) in rack_sleds.iter().filter(|s| s.scrimlet).enumerate() {
            let dir = cargo_bay(&s.name);
            fs::create_dir_all(&dir)?;
            fs::write(
                dir.join(format!("mgs-config-switch{slot}.toml")),
                mgs::switch_config(slot as u8, &fleet, &scrimlet_indices),
            )?;
            if emu {
                // The scrimlet only needs an address on its rack's SP network
                // to reach the host fleet.
                fs::write(
                    dir.join("sp-net"),
                    format!(
                        "{}/{SP_NET_PREFIX_LEN}",
                        sp_scrimlet_addr(rack, s.index)
                    ),
                )?;
            } else {
                fs::write(
                    dir.join("sp-sim-config.toml"),
                    fleet.sp_sim_config(),
                )?;
            }
        }
        stage_sp_emu(cfg, &fleet, &sp_fleet_dir(rack), fw.as_deref())?;
    }
    Ok(())
}

/// Stage the sp-emu binary, faux-mgs, RoT image, hubris archives and host
/// phase 1 rom into the rack's host fleet directory. No-op without emu SPs.
fn stage_sp_emu(
    cfg: &VoxelConfig,
    fleet: &SpFleet,
    dir: &Utf8Path,
    fw: Option<&Utf8Path>,
) -> anyhow::Result<()> {
    let emu = fleet.emu_sps();
    if emu.is_empty() {
        return Ok(());
    }
    let out = dir.join("sp-emu");
    fs::create_dir_all(&out)?;
    // sp_host reports the missing binary rather than starting nothing.
    let Some(emu_bin) = cfg.sp.emu_bin.as_deref() else {
        return Ok(());
    };
    fs::copy(emu_bin, out.join("sp-emu"))
        .with_context(|| format!("stage sp-emu binary from {emu_bin}"))?;
    // Optional: the operator sp commands need it, launch does not.
    if let Some(faux) = cfg.sp.faux_mgs.as_deref() {
        fs::copy(faux, out.join("faux-mgs"))
            .with_context(|| format!("stage faux-mgs from {faux}"))?;
    }
    let Some(fw_dir) = fw else {
        bail!(
            "--emu needs firmware: build the image with --from-tuf, or \
             point --sp-firmware at a directory of SP/RoT images"
        );
    };
    // sp-emu runs the RoT in-process over sprot.
    let rot = fw_dir.join("rot-a.zip");
    fs::copy(&rot, out.join("rot.image"))
        .with_context(|| format!("stage RoT image from {rot}"))?;
    // A staged bootleby turns on sp-emu secure boot.
    let bootleby = fw_dir.join("bootleby.zip");
    if bootleby.exists() {
        fs::copy(&bootleby, out.join("bootleby.zip"))
            .with_context(|| format!("stage bootleby from {bootleby}"))?;
    }
    // One hubris archive per role; voxel-init flashes each instance from it.
    let mut staged = BTreeSet::new();
    for sp in emu {
        let (role, archive) = if sp.selector() == "sidecar" {
            ("sidecar", "sp-sidecar-c.zip")
        } else {
            ("gimlet", "sp-gimlet-c.zip")
        };
        if !staged.insert(role) {
            continue;
        }
        let image = fw_dir.join(archive);
        fs::copy(&image, out.join(format!("{role}.archive")))
            .with_context(|| format!("stage {role} archive from {image}"))?;
    }
    // The release's host phase 1, cached by cpbuild next to the firmware dir,
    // seeds each gimlet SP's host-boot flash.
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

/// Generate the sprockets trust-quorum test keys and measurements and stage
/// each sled's identity into cargo-bay/<sled>/sprockets.
pub(crate) fn stage_sprockets(cfg: &VoxelConfig) -> anyhow::Result<()> {
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

    // Fake attestation log and corim, the same constants a4x2 uses; sled-agent
    // only needs one measurement present.
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

    /// A stand-in checkout with the sled-agent config field and the
    /// sled-hardware enum variants the detection reads.
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

    /// HardcodedPhysical { must not be mistaken for the newer Hardcoded {.
    #[test]
    fn detects_the_disks_shape_per_era() {
        let disks = |src: &Utf8PathBuf| schema_from_checkout(src).unwrap().1;

        assert_eq!(
            disks(&fake_sled_src(
                "old",
                "pub vdevs: Vec<String>, data_links: [String; 2],",
                ""
            )),
            SledDisksSchema::Vdevs
        );
        assert_eq!(
            disks(&fake_sled_src(
                "mid",
                "pub external_disks: ExternalDisks, data_links: [String; 2],",
                "Virtual { vdevs: Vec<String> }, HardcodedPhysical { disks: Vec<D> }, DetectPhysical,",
            )),
            SledDisksSchema::ExternalDisks
        );
        assert_eq!(
            disks(&fake_sled_src(
                "new",
                "pub external_disks: ExternalDisks, data_links: [String; 2],",
                "Hardcoded { vdevs: Vec<String>, disks: Vec<D> }, DetectPhysical,",
            )),
            SledDisksSchema::Hardcoded
        );
        // No checkout, or an unrecognized shape, is no answer.
        assert_eq!(
            schema_from_checkout(Utf8Path::new("/nonexistent-checkout")),
            None
        );
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

    /// Image stamp beats checkout, config overrides beat both per field, and
    /// no source at all is an error.
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

    /// A short image label resolves through the build root to the full-sha
    /// checkout and its schema.
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

    /// Live zfs stamp round-trip. Needs a real dataset, run on the box:
    /// VOXEL_SCHEMA_ZFS_TEST=<dataset> cargo test -- --ignored
    #[test]
    #[ignore]
    fn zfs_stamp_round_trip() {
        let Ok(dataset) = std::env::var("VOXEL_SCHEMA_ZFS_TEST") else {
            panic!("set VOXEL_SCHEMA_ZFS_TEST=<dataset> to run");
        };
        let zfs = |args: &[&str]| {
            assert!(
                Command::new("zfs").args(args).status().unwrap().success(),
                "zfs {args:?}"
            );
        };
        for img in ["schema-test-a", "schema-test-b"] {
            let _ = Command::new("zfs")
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
