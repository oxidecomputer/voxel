//! Falcon topology construction (driven by [`VoxelConfig`]) and the per-launch
//! cargo-bay staging that feeds it (generated sled/RSS/FRR/switch1 config +
//! sprockets keys).

use anyhow::{Context, anyhow};
use attest_mock::MockData;
use libfalcon::{NodeRef, Runner, SmbiosType1Input, unit::gb};
use std::fs;
use std::path::{Path, PathBuf};
use voxel_config::{
    ServicePoolSchema, SledDataLinksSchema, SledDesc, SledDisksSchema, VoxelConfig,
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
                self.routers
                    .iter()
                    .find(|(r, _)| r == name)
                    .map(|(_, n)| *n)
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
fn ext_interface(d: &mut Runner, n: NodeRef, cfg_link: Option<&str>) -> anyhow::Result<()> {
    if let Ok(ifx) = std::env::var("EXT_INTERFACE") {
        d.ext_link(&ifx, n);
    } else if let Some(ifx) = cfg_link {
        d.ext_link(ifx, n);
    } else {
        d.default_ext_link(n)
            .map_err(|e| anyhow!("failed to find default external interface: {e}"))?;
    }
    Ok(())
}

/// Fill in the SMBIOS type-1 for a given sled. The manufacturer must currently
/// always be `a4x2` — the ONLY string omicron's `sled-hardware` recognises to
/// read identity from SMBIOS instead of falling back to the hostname. We can change
/// this in Omicron to allow more strings, such as "voxel" in the future.
///
/// We must ensure the reported SMBIOS info used to populate the `BaseboardId`
/// in an emulated hardware environments matches what is reported by MGS for
/// simulated and emulated SPs.
///
/// TODO: eliminate any need to patch as described below. This should come soon
/// with the changes in https://github.com/oxidecomputer/omicron/pull/10518
/// that remove a lot of the reliance on `Baseboard` and use `BaseboardId` more
/// broadly instead.
///
///  Serial `2FAKE00{index+1}` and revision `2` BYTE-MATCH the emulated
/// SP's VPD (sp-emu builds `2FAKE00{(port-33300)/10}`, i.e. `index+1`,
/// barcode rev `002`) and model `913-0000019`. Paired with the omicron
/// `parse_smbios_output` Pc->Gimlet patch (applied in build-cp.sh), sled-agent
/// then reports the SAME `Gimlet` baseboard the SP reports via MGS, so
/// wicketd's RACK SETUP correlates each sled's bootstrap address instead of
/// showing UNKNOWN. (Without the patch sled-agent returns a `Pc` baseboard,
/// which can never equal the SP's `Gimlet` in wicketd's lookup.)
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
pub(crate) fn build_topo(cfg: &VoxelConfig, name: &str) -> anyhow::Result<Topo> {
    let cp_img = cfg.image.cp_image();
    let frr_img = cfg.image.frr_image();

    let mut d = Runner::new(name);
    d.persistent = true;

    // Sleds (voxel-cp) and routers (voxel-frr). Guest RAM is configurable so a
    // bigger rack can shrink per-sled memory to fit physical RAM (VMM Memory is
    // the dominant consumer); see the launch memory preflight.
    let sled_mem = gb(cfg.topology.sled_memory_gb);
    let router_mem = gb(cfg.topology.router_memory_gb);
    let mut sleds = Vec::new();
    for s in cfg.sleds() {
        let n = d.node(&s.name, &cp_img, 8, sled_mem);
        d.reserve(n, 100);
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
    let ext_if = cfg
        .external
        .isolated()
        .then_some(crate::isolated_external::STUB);

    let all_scrimlets: Vec<NodeRef> = sleds
        .iter()
        .filter(|(s, _)| s.scrimlet)
        .map(|(_, n)| *n)
        .collect();
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
        let node = |idx: usize| sleds.iter().find(|(s, _)| s.index == idx).map(|(_, n)| *n);
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

    Ok(Topo {
        runner: d,
        sleds,
        routers,
    })
}

/// Host-side cargo-bay root (per-node staging dirs live under `<CARGO_BAY>/<node>`,
/// mounted into each guest at `/opt/cargo-bay`).
const CARGO_BAY: &str = "./cargo-bay";

fn cargo_bay(node: &str) -> PathBuf {
    Path::new(CARGO_BAY).join(node)
}

/// Clear each node's cargo-bay before staging so it reflects ONLY the current
/// topology. Otherwise files from a prior launch with a different topology
/// linger: e.g. a 3-sled run (scrimlets g0+g2) stages `mgs-config-switch1.toml`
/// into `cargo-bay/g2`, and a later 4-sled run (scrimlets g0+g3) wouldn't
/// overwrite it - so voxel-init on g2 would find the stale file and start a
/// pointless switch1 enforcer. Wiping first guarantees a clean, correct stage.
pub(crate) fn reset_node_cargo_bay(cfg: &VoxelConfig) -> anyhow::Result<()> {
    let mut nodes: Vec<String> = cfg.sleds().into_iter().map(|s| s.name).collect();
    nodes.extend(cfg.topology.routers.iter().cloned());
    for node in nodes {
        let dir = cargo_bay(&node);
        if dir.exists() {
            fs::remove_dir_all(&dir).with_context(|| format!("reset {}", dir.display()))?;
        }
        fs::create_dir_all(&dir)?;
    }
    Ok(())
}

/// Render this rack's `config-rss.toml` into `dir`. Generation lives in
/// `voxel_config::rss`.
fn generate_rss_config(
    cfg: &VoxelConfig,
    dir: &Path,
    rack: usize,
    pools: ServicePoolSchema,
) -> anyhow::Result<()> {
    let text = cfg
        .to_config_rss(rack, pools)
        .map_err(|e| anyhow!("render config-rss.toml for rack {rack}: {e}"))?;
    let out = dir.join("config-rss.toml");
    fs::write(&out, text).with_context(|| format!("write {}", out.display()))?;
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
pub(crate) fn detect_sled_schema(
    cfg: &VoxelConfig,
    src_root: Option<&Path>,
) -> (SledDataLinksSchema, SledDisksSchema) {
    let read = |rel: &str| {
        src_root
            .map(|p| p.join(rel))
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default()
    };
    let src = read("sled-agent/src/config.rs");
    // The variants of `ExternalDisks` are declared in sled-hardware, so read the
    // enum itself rather than the field that references it. `Virtual { vdevs }`
    // and `HardcodedPhysical { disks }` merged into `Hardcoded { vdevs, disks }`.
    let disks = if !src.contains("pub external_disks") {
        SledDisksSchema::Vdevs
    } else if read("sled-hardware/src/lib.rs").contains("Hardcoded {") {
        SledDisksSchema::Hardcoded
    } else {
        SledDisksSchema::ExternalDisks
    };
    // `data_links: DataLinks` (tagged enum) vs the older flat list.
    let data_links = if src.contains("data_links: DataLinks") {
        SledDataLinksSchema::Tagged
    } else {
        SledDataLinksSchema::List
    };
    (
        cfg.image.data_links_schema.unwrap_or(data_links),
        cfg.image.disks_schema.unwrap_or(disks),
    )
}

/// Path of the omicron checkout the image was built from, if it's on disk.
pub(crate) fn omicron_src() -> Option<PathBuf> {
    std::env::var("VOXEL_OMICRON_SRC")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

/// The config-rss file omicron ships as its own worked example. It tracks the
/// schema in-tree, which makes it the cheapest ground truth for what a given
/// commit expects.
pub(crate) const OMICRON_EXAMPLE_RSS: &str = "smf/sled-agent/non-gimlet/config-rss.toml";

/// Detect the service IP pool shape from an omicron checkout. omicron #10956
/// replaced `internal_services_ip_pool_ranges` with `service_ip_pools`;
/// the field is declared in the bootstrap-agent lockstep types.
pub(crate) fn detect_service_pool_schema(src: Option<&Path>) -> ServicePoolSchema {
    let declared = src
        .map(|s| s.join("sled-agent/bootstrap-agent-lockstep-types/src/lib.rs"))
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
    if declared.contains("service_ip_pools") {
        ServicePoolSchema::Pools
    } else {
        ServicePoolSchema::Ranges
    }
}

/// Compare the top-level keys voxel emits against the ones `src`'s own example
/// config-rss carries. `RackInitializeRequest` does not `deny_unknown_fields`,
/// so a stale key voxel emits is ignored, but a key omicron expects and voxel
/// omits is a hard "missing field" at sled-agent startup. Only the second
/// direction fails here; the first is reported as drift worth cleaning up.
///
/// Returns Ok(()) when the example can't be read, so an unusual checkout layout
/// doesn't block a build.
pub(crate) fn check_rss_schema(cfg: &VoxelConfig, src: &Path) -> anyhow::Result<()> {
    let example = src.join(OMICRON_EXAMPLE_RSS);
    let Ok(text) = fs::read_to_string(&example) else {
        return Ok(());
    };
    let expected: toml::Table =
        toml::from_str(&text).with_context(|| format!("parse {}", example.display()))?;
    let pools = cfg
        .image
        .service_pool_schema
        .unwrap_or_else(|| detect_service_pool_schema(Some(src)));
    let ours = cfg
        .config_rss_keys(pools)
        .map_err(|e| anyhow!("render config-rss to check its schema: {e}"))?;

    let missing: Vec<&String> = expected.keys().filter(|k| !ours.contains(k)).collect();
    let stale: Vec<&String> = ours.iter().filter(|k| !expected.contains_key(*k)).collect();
    if !stale.is_empty() {
        eprintln!(
            "[voxel] config-rss keys absent from this omicron's example: {}",
            stale
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !missing.is_empty() {
        return Err(anyhow!(
            "config-rss schema drift: {} requires top-level {}, which voxel does not emit. \
             Update voxel-config's rss module, or set [image].service_pool_schema.",
            example.display(),
            missing
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    eprintln!("[voxel] config-rss schema ok, service pool {pools:?}");
    Ok(())
}

/// Generate + stage per-node config into the cargo-bay before launch.
pub(crate) fn stage_config(
    cfg: &VoxelConfig,
    emu_sp: bool,
    emu_rot: bool,
    wicket_setup: bool,
) -> anyhow::Result<()> {
    let sleds = cfg.sleds();
    // Per-sled sled-agent config (replaces a4x2's config/gN-config.toml). Each
    // scrimlet's SoftNPU links only its OWN rack's sleds (rear ports) + every
    // fabric router (front ports), so the rear-port budget is the PER-RACK sled
    // count (`topology.sleds`), not the deployment total. (For a single rack the
    // two are equal.) Fabric routers = every router except the customer edge `ce`.
    let num_sleds_per_rack = cfg.topology.sleds;
    let num_fabric_routers = cfg
        .topology
        .routers
        .iter()
        .filter(|r| r.as_str() != "ce")
        .count();
    // Auto-detect the sled-agent config shapes from the image's omicron (no
    // per-era operator knobs); an [image] override wins if set.
    let (data_links, disks) = detect_sled_schema(cfg, omicron_src().as_deref());
    eprintln!("[voxel] sled-agent config schema: data_links={data_links:?} disks={disks:?}");
    for s in &sleds {
        let dir = cargo_bay(&s.name);
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("sled-config.toml"),
            s.sled_config(num_sleds_per_rack, num_fabric_routers, data_links, disks)
                .with_interconnects(cfg.topology.interconnect_count_for(s.index))
                .render(),
        )?;
    }

    // One typed config-rss per rack, staged on that rack's RSS node (its first
    // bootstrap sled - g0 for rack 0, g{rack*sleds} for the rest). Each rack is an
    // independent RSS domain: generation filters the bootstrap set to that rack
    // and offsets its customer/service network.
    let pools = cfg
        .image
        .service_pool_schema
        .unwrap_or_else(|| detect_service_pool_schema(omicron_src().as_deref()));
    eprintln!("[voxel] config-rss service pool schema: {pools:?}");
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
        let rss_dir = if wicket_setup {
            let d = Path::new("wicket-setup").join(format!("rack{rack}"));
            fs::create_dir_all(&d)?;
            d
        } else if rack > 0 {
            // Multirack: rack 0 is the cluster; rack > 0 boots but does NOT RSS -
            // it's an unclaimed rack staged for a future cluster-join (RFD 573).
            // Generate its config-rss OUTSIDE the cargo-bay so voxel-init won't
            // auto-inject + RSS it; kept under multirack-staged/ for the join flow.
            let d = Path::new("multirack-staged").join(format!("rack{rack}"));
            fs::create_dir_all(&d)?;
            d
        } else {
            cargo_bay(&rss_node.name)
        };
        generate_rss_config(cfg, &rss_dir, rack, pools)?;
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
            let mut body = format!(
                "ip {ip}/{prefix}\ngateway {}\ndns {dns}\n",
                cfg.external.host_ip
            );
            if router_names.contains(node.as_str()) {
                body.push_str(&format!("iface {}\n", cfg.router_ext_iface(&node)));
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
        let rack_sleds: Vec<&SledDesc> = sleds.iter().filter(|s| s.rack == rack).collect();
        let gimlet_indices: Vec<usize> = rack_sleds.iter().map(|s| s.index).collect();
        let scrimlet_indices: Vec<usize> = rack_sleds
            .iter()
            .filter(|s| s.scrimlet)
            .map(|s| s.index)
            .collect();
        // The SP fleet (sidecar + one SP per rack sled) is the shared MGS↔SP
        // contract; sim backend today, swappable to a real-firmware host.
        // `--emu`: every SP is real-firmware on sp-emu (voxel-init disables sp-sim
        // in-zone). Default: sp-sim for the whole fleet, no emu staging.
        let fleet = if emu_sp {
            voxel_config::sp::SpFleet::for_gimlets(
                &gimlet_indices,
                voxel_config::sp::SpBackend::Emu,
            )
        } else {
            voxel_config::sp::SpFleet::sim_for_gimlets(&gimlet_indices)
        };
        for (slot, s) in rack_sleds.iter().filter(|s| s.scrimlet).enumerate() {
            let dir = cargo_bay(&s.name);
            fs::create_dir_all(&dir)?;
            fs::write(
                dir.join(format!("mgs-config-switch{slot}.toml")),
                voxel_config::mgs::switch_config(slot as u8, &fleet, &scrimlet_indices),
            )?;
            // Stage sp-sim's config only in the default path; under --emu sp-sim is
            // disabled, so don't stage one (and the enforcer leaves sp-sim alone).
            if !emu_sp {
                fs::write(dir.join("sp-sim-config.toml"), fleet.sp_sim_config())?;
            }
            stage_sp_emu(cfg, &fleet, &dir, emu_rot)?;
        }
    }
    Ok(())
}

/// Stage the `sp-emu` binary + each emulated SP's flashed hubris image into a
/// scrimlet's cargo-bay (`sp-emu/`), so `voxel-init` can run the real-firmware SPs
/// in that switch zone. Each emu SP's image is flashed into `<base_port>.flash`
/// (the filename carries the MGS port; voxel-init derives the board from it).
/// No-op when no SP in the fleet is emulator-backed. Staged pre-launch, so it's
/// present at boot (no 9p-visibility issue).
fn stage_sp_emu(
    cfg: &VoxelConfig,
    fleet: &voxel_config::sp::SpFleet,
    dir: &Path,
    emu_rot: bool,
) -> anyhow::Result<()> {
    let emu = fleet.emu_sps();
    if emu.is_empty() {
        return Ok(());
    }
    let out = dir.join("sp-emu");
    fs::create_dir_all(&out)?;
    // Always write the fleet manifest (`rot <0|1>` + `<port> <role>` lines) so
    // voxel-init knows the SP set, each SP's role, and whether --emu-rot is on —
    // even when it boots from the image's BAKED /opt/oxide/sp-emu artifacts
    // (self-contained) rather than these staged copies. (The staged rot.flash was
    // previously the only signal of --emu-rot; the baked path needs it explicit.)
    let mut manifest = format!("rot {}\n", if emu_rot { 1 } else { 0 });
    for sp in &emu {
        let role = if sp.selector() == "sidecar" {
            "sidecar"
        } else {
            "gimlet"
        };
        manifest.push_str(&format!("{} {}\n", sp.base_port, role));
    }
    let ports_manifest = out.join("ports");
    fs::write(&ports_manifest, manifest)
        .with_context(|| format!("write {}", ports_manifest.display()))?;
    // Dev override: with [sp].emu_bin set, stage the binary + per-SP flashes from
    // the local build for fast iteration (no rebake). Unset -> voxel-init uses the
    // baked image artifacts (the per-SP flash from the baked per-role flash).
    let Some(emu_bin) = cfg.sp.emu_bin.as_deref() else {
        return Ok(());
    };
    fs::copy(emu_bin, out.join("sp-emu"))
        .with_context(|| format!("stage sp-emu binary from {emu_bin}"))?;
    // Stage `faux-mgs` (the MGS client) alongside it when configured, so
    // `voxel sp ls/state/exec` can talk to the live SPs from inside the switch
    // zone. Optional: the operator `sp` commands need it; launch itself doesn't.
    if let Some(faux) = cfg.sp.faux_mgs.as_deref() {
        fs::copy(faux, out.join("faux-mgs"))
            .with_context(|| format!("stage faux-mgs from {faux}"))?;
    }
    // The sidecar SP runs oxide-rot-1 as a second emulated core (the sprot
    // bridge) when --emu-rot is set, so MGS/Nexus see a real RoT. OFF by
    // default: the two-core sidecar cannot answer MGS switch-id in time during
    // RSS, which wedges the nexus handoff - attach the bridge after bring-up.
    if emu_rot {
        let rot =
            cfg.sp.rot_image.as_deref().ok_or_else(|| {
                anyhow!("--emu-rot requires [sp].rot_image (the oxide-rot-1 image)")
            })?;
        fs::copy(rot, out.join("rot.flash"))
            .with_context(|| format!("stage RoT image from {rot}"))?;
    }
    for sp in emu {
        let sel = sp.selector();
        let image = cfg.sp.image_for(&sel).ok_or_else(|| {
            let key = if sel == "sidecar" {
                "sidecar_image"
            } else {
                "gimlet_image"
            };
            anyhow!("[sp].emu includes {sel} but [sp].{key} is unset")
        })?;
        let flash = out.join(format!("{}.flash", sp.base_port));
        let status = std::process::Command::new(emu_bin)
            .env("SP_EMU_FLASH", &flash)
            .args(["flash", "a", image])
            .status()
            .with_context(|| format!("run {emu_bin} flash for {sel}"))?;
        if !status.success() {
            return Err(anyhow!("sp-emu flash failed for {sel} (image {image})"));
        }
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
    const SP_DIGEST: &str = "be4df4e085175f3de0c8ac4837e1c2c9a34e8983209dac6b549e94154f7cdd9c";
    const FWID_DIGEST: &str = "72fa8f8ea84a42251031366002cbb36281d0131f78cd680436116a720cdd9de5";
    let attest_log = attest_mock::MockLog::from_document(attest_mock::log::Document {
        measurements: vec![attest_mock::log::Measurement {
            algorithm: "sha3-256".into(),
            digest: SP_DIGEST.into(),
        }],
    })
    .map_err(|e| anyhow!("attest log: {e}"))?
    .to_bytes()
    .map_err(|e| anyhow!("attest log serialization failed: {e}"))?;

    let corim = attest_mock::MockCorim::from_document(attest_mock::corim::Document {
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

    /// A stand-in omicron checkout carrying only the two files the schema
    /// check reads: the example config-rss and the lockstep type declaration.
    fn fake_omicron(name: &str, example_pool_key: &str, lockstep_field: &str) -> PathBuf {
        let src = std::env::temp_dir().join(format!("voxel-rsscheck-{name}"));
        let _ = fs::remove_dir_all(&src);
        fs::create_dir_all(src.join("smf/sled-agent/non-gimlet")).unwrap();
        fs::create_dir_all(src.join("sled-agent/bootstrap-agent-lockstep-types/src")).unwrap();
        // Every era's example carries this same key set apart from the pool.
        let example = format!(
            "ntp_servers = []\ndns_servers = []\nexternal_dns_ips = []\n\
             external_dns_zone_name = \"x\"\nexternal_certificates = []\n\
             {example_pool_key}\n[bootstrap_discovery]\ntype = \"only_these\"\naddrs = []\n\
             [recovery_silo]\nsilo_name = \"r\"\nuser_name = \"r\"\nuser_password_hash = \"h\"\n\
             [rack_network_config]\nrack_subnet = \"fd00::/56\"\n\
             [allowed_source_ips]\nallow = \"any\"\n"
        );
        fs::write(src.join(OMICRON_EXAMPLE_RSS), example).unwrap();
        fs::write(
            src.join("sled-agent/bootstrap-agent-lockstep-types/src/lib.rs"),
            format!("pub struct RackInitializeRequest {{ pub {lockstep_field}: T }}"),
        )
        .unwrap();
        src
    }

    fn pools_era(name: &str) -> PathBuf {
        fake_omicron(
            name,
            "[[service_ip_pools]]\nname = \"p\"\ndescription = \"d\"\nranges = []",
            "service_ip_pools",
        )
    }

    fn ranges_era(name: &str) -> PathBuf {
        fake_omicron(
            name,
            "[[internal_services_ip_pool_ranges]]\nfirst = \"1.1.1.1\"\nlast = \"1.1.1.2\"",
            "internal_services_ip_pool_ranges",
        )
    }

    /// A stand-in checkout carrying the two files the sled-schema detection
    /// reads: the sled-agent config field and the sled-hardware enum variants.
    fn fake_sled_src(name: &str, field: &str, variants: &str) -> PathBuf {
        let src = std::env::temp_dir().join(format!("voxel-sledcheck-{name}"));
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
        let cfg = VoxelConfig::default();
        let disks = |src: &PathBuf| detect_sled_schema(&cfg, Some(src)).1;

        // Oldest: a flat `vdevs` list, no `external_disks` field at all.
        assert_eq!(
            disks(&fake_sled_src("old", "pub vdevs: Vec<String>,", "")),
            SledDisksSchema::Vdevs
        );
        // Middle: separate `Virtual` and `HardcodedPhysical` variants.
        assert_eq!(
            disks(&fake_sled_src(
                "mid",
                "pub external_disks: ExternalDisks,",
                "Virtual { vdevs: Vec<String> }, HardcodedPhysical { disks: Vec<D> }, DetectPhysical,",
            )),
            SledDisksSchema::ExternalDisks
        );
        // Current: the two merged into `Hardcoded { vdevs, disks }`.
        assert_eq!(
            disks(&fake_sled_src(
                "new",
                "pub external_disks: ExternalDisks,",
                "Hardcoded { vdevs: Vec<String>, disks: Vec<D> }, DetectPhysical,",
            )),
            SledDisksSchema::Hardcoded
        );
        // No checkout on disk: assume the oldest shape.
        assert_eq!(detect_sled_schema(&cfg, None).1, SledDisksSchema::Vdevs);
    }

    #[test]
    fn detects_the_service_pool_shape_per_era() {
        assert_eq!(
            detect_service_pool_schema(Some(&pools_era("det-new"))),
            ServicePoolSchema::Pools
        );
        assert_eq!(
            detect_service_pool_schema(Some(&ranges_era("det-old"))),
            ServicePoolSchema::Ranges
        );
        // No checkout on disk: assume the older shape.
        assert_eq!(detect_service_pool_schema(None), ServicePoolSchema::Ranges);
    }

    #[test]
    fn schema_check_passes_on_both_eras() {
        let cfg = VoxelConfig::default();
        check_rss_schema(&cfg, &pools_era("ok-new")).expect("new era");
        check_rss_schema(&cfg, &ranges_era("ok-old")).expect("old era");
    }

    /// The regression this exists for: omicron #10956 renamed the pool field.
    /// Pinning voxel to the old shape against a new omicron must fail the
    /// BUILD, naming the field, not surface as a bring-up failure.
    #[test]
    fn schema_check_fails_when_voxel_lags_omicron() {
        let mut cfg = VoxelConfig::default();
        cfg.image.service_pool_schema = Some(ServicePoolSchema::Ranges);
        let err = check_rss_schema(&cfg, &pools_era("drift"))
            .expect_err("stale pool shape must fail the build")
            .to_string();
        assert!(err.contains("service_ip_pools"), "unhelpful error: {err}");
        assert!(err.contains("schema drift"), "unhelpful error: {err}");
    }

    /// An unreadable/unusual checkout must not block a build.
    #[test]
    fn schema_check_skips_without_an_example() {
        let empty = std::env::temp_dir().join("voxel-rsscheck-empty");
        let _ = fs::remove_dir_all(&empty);
        fs::create_dir_all(&empty).unwrap();
        check_rss_schema(&VoxelConfig::default(), &empty).expect("no example is not fatal");
    }
}
