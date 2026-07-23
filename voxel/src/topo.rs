//! Falcon topology construction (driven by [`VoxelConfig`]) and the per-launch
//! cargo-bay staging that feeds it (generated sled/RSS/FRR/switch1 config +
//! sprockets keys).

use anyhow::{anyhow, Context};
use libfalcon::{unit::gb, NodeRef, Runner, SmbiosType1Input};
use std::fs;
use std::path::{Path, PathBuf};
use voxel_config::{SledDataLinksSchema, SledDesc, SledDisksSchema, VoxelConfig};

/// Gimlet board-serial prefix. The SMBIOS serial ([`populate_smbios`]) and the
/// faux-mgs lookup serial (`sp_cmd::sp_serial`) both build `{prefix}{index+1}`
/// from this, so they can't drift; it BYTE-MATCHES the emulated SP's VPD. Swapping
/// to a mfg-allocated serial is a coordinated edit here + sp-emu `build_vpd_eeprom`
/// + the vendored sprockets `platform_id` (see the de-a4x2 handoff notes).
pub(crate) const GIMLET_SERIAL_PREFIX: &str = "BRM4422000";

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

fn ext_interface(d: &mut Runner, n: NodeRef) -> anyhow::Result<()> {
    if let Ok(ifx) = std::env::var("EXT_INTERFACE") {
        d.ext_link(&ifx, n);
    } else {
        d.default_ext_link(n)
            .map_err(|e| anyhow!("failed to find default external interface: {e}"))?;
    }
    Ok(())
}

/// SMBIOS type-1 for sled `index`. Manufacturer is `a4x2` — the ONLY string
/// omicron's `sled-hardware` recognises to read identity from SMBIOS instead of
/// falling back to the hostname. Serial `BRM4422000{index+1}` and revision `2`
/// BYTE-MATCH the emulated SP's VPD (sp-emu builds `BRM4422000{(port-33300)/10}`,
/// i.e. `index+1`, barcode rev `002`) and model `913-0000019`. Paired with the
/// omicron `parse_smbios_output` Pc->Gimlet patch (applied in build-cp.sh),
/// sled-agent then reports the SAME `Gimlet` baseboard the SP reports via MGS, so
/// wicketd's RACK SETUP correlates each sled's bootstrap address instead of
/// showing UNKNOWN. (Without the patch sled-agent returns a `Pc` baseboard, which
/// can never equal the SP's `Gimlet` in wicketd's lookup.)
fn populate_smbios(d: &mut Runner, x: NodeRef, index: usize) {
    d.set_smbios_type1(
        x,
        SmbiosType1Input {
            manufacturer: "a4x2".to_string(),
            product_name: "913-0000019".to_string(),
            serial_number: format!("{GIMLET_SERIAL_PREFIX}{}", index + 1),
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
        ext_interface(&mut d, ce)?;
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
        ext_interface(&mut d, *n)?;
    }
    for (_, n) in &fabric_routers {
        for sc in &all_scrimlets {
            d.softnpu_link(*sc, *n, Some(new_mac()), None);
        }
        ext_interface(&mut d, *n)?;
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
        populate_smbios(&mut d, *n, s.index);
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

/// Render `config-rss.toml` with the typed, release-pinned generator
/// (`voxel-rss-gen`, built against the image's omicron - see voxel/rss-gen).
/// This is release-accurate by construction; we deliberately do NOT fall back
/// to a hand-rolled renderer. Point `VOXEL_RSS_GEN` at the binary if it isn't
/// at the default path.
fn generate_rss_config(cfg: &VoxelConfig, dir: &Path, rack: usize) -> anyhow::Result<()> {
    let gen = std::env::var("VOXEL_RSS_GEN")
        .unwrap_or_else(|_| "/opt/omicron/target/debug/voxel-rss-gen".to_string());
    let effective = dir.join("voxel-effective.toml");
    // Write the *resolved* config (derived scrimlets/rss_sleds made explicit) -
    // the separately-built rss-gen doesn't re-run the derivation, so empty
    // scrimlets / rss_sleds = 0 would yield an empty bootstrap set ("Must
    // request at least one peer"). rss-gen projects it to a single rack via
    // `--rack` (filters the bootstrap set + offsets the customer network).
    fs::write(&effective, cfg.to_resolved_toml())?;
    let status = std::process::Command::new(&gen)
        .arg("generate")
        .arg(&effective)
        .arg(dir.join("config-rss.toml"))
        .arg("--rack")
        .arg(rack.to_string())
        .status()
        .map_err(|e| anyhow!("run {gen}: {e} - build voxel/rss-gen or set VOXEL_RSS_GEN"))?;
    if !status.success() {
        return Err(anyhow!(
            "{gen} generate failed. If the error above is a TOML 'unknown field', \
             voxel-rss-gen is STALE for the current voxel-config (it's built separately and \
             pins voxel-config at build time) - rebuild it: \
             `voxel-image/build-rss-gen.sh <omicron-src>`, or run `voxel image create <commit>` \
             without BUILD_RSS_GEN=0."
        ));
    }
    Ok(())
}

/// Auto-detect the sled-agent config shapes (`data_links`, disks) from the
/// image's omicron source, so operators never hand-set per-era knobs. The source
/// sits beside the commit-pinned rss-gen (`$VOXEL_RSS_GEN` =
/// `<build_root>/omicron-<commit>/target/debug/voxel-rss-gen`), so we read its
/// `sled-agent/src/config.rs` and key off the field declarations - which are the
/// ground truth for that commit. Falls back to the oldest shapes if the source
/// can't be read; an explicit `[image]` override wins over detection.
///
/// This is the "schema changelog", automated: instead of a hand-maintained
/// commits->requirements table, voxel reads what the commit itself declares.
fn detect_sled_schema(cfg: &VoxelConfig) -> (SledDataLinksSchema, SledDisksSchema) {
    let src = std::env::var("VOXEL_RSS_GEN")
        .ok()
        .and_then(|g| {
            Path::new(&g)
                .ancestors()
                .nth(3)
                .map(|p| p.join("sled-agent/src/config.rs"))
        })
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
    // `pub external_disks: ExternalDisks` (main) vs `pub vdevs: ...` (older).
    let disks = if src.contains("pub external_disks") {
        SledDisksSchema::ExternalDisks
    } else {
        SledDisksSchema::Vdevs
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
    let (data_links, disks) = detect_sled_schema(cfg);
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
    // independent RSS domain: rss-gen (`--rack`) filters the bootstrap set to that
    // rack and offsets its customer/service network.
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
    let attest_log = attest_mock::log::mock(attest_mock::log::Document {
        measurements: vec![attest_mock::log::Measurement {
            algorithm: "sha3-256".into(),
            digest: SP_DIGEST.into(),
        }],
    })
    .map_err(|e| anyhow!("attest log: {e}"))?;
    let corim = attest_mock::corim::mock(attest_mock::corim::Document {
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
    .map_err(|e| anyhow!("corim: {e}"))?;

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
