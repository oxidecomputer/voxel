// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rack lifecycle commands: launch, route, destroy, info, status.

use anyhow::{Context, bail};
use camino::Utf8Path;
use libfalcon::{NodeRef, Runner};
use slog::{Logger, info, warn};
use std::collections::HashSet;
use std::fmt;
use std::process::Command;
use std::time::{Duration, Instant};
use voxel_config::VoxelConfig;

use crate::isolated_external::{
    DryRun, link_mtu, probe_out, up as external_up,
};
use crate::net::{
    ce_static_ip, resolve_external_ip, set_external_route, ssh_capture,
    ssh_output, static_external_ip, wait_external_reachable, zlogin,
};
use crate::network::{enable_link, switch_ready};
use crate::rss::watch_rss;
use crate::topo::{
    Topo, build_topo, is_tuf_image, reset_node_cargo_bay, stage_config,
    stage_sprockets,
};
use crate::{commission, disks, image, sp_host};

#[derive(Clone, Copy)]
pub(crate) struct LaunchOpts<'a> {
    pub(crate) no_progress: bool,
    pub(crate) no_route: bool,
    pub(crate) emu: bool,
    pub(crate) init_rss: bool,
    /// Firmware directory for the emulated fleet, overriding the image's own.
    pub(crate) sp_firmware: Option<&'a Utf8Path>,
}

/// Progress tag for a rack: rackN (1-based) with several racks, else single.
fn rack_label(racks: usize, rack: usize, single: &str) -> String {
    if racks > 1 { format!("rack{}", rack + 1) } else { single.to_string() }
}

/// The sled's static external address in isolated mode, None in lan mode.
fn known_external_ip(cfg: &VoxelConfig, sled: &str) -> Option<String> {
    if !cfg.external.isolated() {
        return None;
    }
    static_external_ip(cfg, sled)
}

/// Point the host route for one rack's external prefix at ce.
async fn set_rack_route(
    cfg: &VoxelConfig,
    d: &Runner,
    ce: NodeRef,
    rack: usize,
    apply: bool,
) -> anyhow::Result<()> {
    let prefix = cfg.network.for_rack(rack).infra_prefix;
    set_external_route(d, ce, &prefix, apply, ce_static_ip(cfg).as_deref())
        .await
}

pub(crate) async fn cmd_route(
    cfg: &VoxelConfig,
    name: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let ce = topo.node_ref("ce").context("no ce router in topology")?;
    for rack in 0..cfg.topology.racks() {
        set_rack_route(cfg, &topo.runner, ce, rack, !dry_run).await?;
    }
    Ok(())
}

/// Physical RAM in GiB from prtconf -m, which reports MB.
fn physical_ram_gb() -> Option<u64> {
    probe_out("prtconf", &["-m"])?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|mb| mb / 1024)
}

/// Refuse a launch whose guest RAM plus kernel and ARC headroom exceeds physical
/// RAM. Skipped when prtconf fails or VOXEL_SKIP_MEM_PREFLIGHT is set.
fn memory_preflight(cfg: &VoxelConfig) -> anyhow::Result<()> {
    // bhyve overhead over the requested guest RAM, as seen in VMM Memory.
    const VMM_OVERHEAD: f64 = 1.2;
    // Kernel (14G observed) plus a minimal ARC (8G).
    const RESERVE_GB: u64 = 22;
    if std::env::var("VOXEL_SKIP_MEM_PREFLIGHT").is_ok() {
        return Ok(());
    }
    let Some(phys) = physical_ram_gb() else {
        return Ok(());
    };
    let vmm =
        (cfg.topology.guest_memory_gb() as f64 * VMM_OVERHEAD).ceil() as u64;
    if vmm + RESERVE_GB > phys {
        bail!(
            "topology needs ~{vmm} GB guest RAM (VMM) + ~{RESERVE_GB} GB kernel/ARC headroom, \
             but this box has {phys} GB. Lower topology.sled_memory_gb (now {}) or the sled count \
             (or set VOXEL_SKIP_MEM_PREFLIGHT=1 to override).",
            cfg.topology.sled_memory_gb
        );
    }
    Ok(())
}

/// The host's default-route interface, None without a default route.
fn default_route_iface() -> Option<String> {
    probe_out(crate::net::ROUTE, &["-n", "get", "default"])?
        .lines()
        .find_map(|l| l.trim().strip_prefix("interface:"))
        .map(|s| s.trim().to_string())
}

/// Refuse a lan-mode launch on a jumbo external link: voxel-init classifies a
/// NIC as underlay iff it accepts mtu 9000, so external NICs never come up.
fn lan_mtu_preflight() -> anyhow::Result<()> {
    let Some(link) =
        std::env::var("EXT_INTERFACE").ok().or_else(default_route_iface)
    else {
        return Ok(());
    };
    if let Some(mtu) = link_mtu(&link)
        && mtu.parse::<u32>().is_ok_and(|m| m >= 9000)
    {
        bail!(
            "external link {link} has mtu {mtu}: sled NICs are classified as underlay \
             iff they accept mtu=9000, so external NICs on a jumbo link are \
             misclassified and never come up. Point EXT_INTERFACE at a sub-9000-mtu \
             link or use isolated mode (voxel config set external.mode isolated)."
        );
    }
    Ok(())
}

/// The topology's sleds as (name, node), optionally only one rack's.
fn sled_nodes(topo: &Topo, rack: Option<usize>) -> Vec<(String, NodeRef)> {
    topo.sleds
        .iter()
        .filter(|(s, _)| rack.is_none_or(|r| s.rack == r))
        .map(|(s, n)| (s.name.clone(), *n))
        .collect()
}

/// Run voxel-init on each node concurrently, logging its [voxel-init] milestone
/// lines. The full output stays in the guest's /tmp/launch.log.
async fn run_voxel_init(d: &Runner, role: &str, nodes: Vec<(String, NodeRef)>) {
    let command =
        format!("/opt/oxide/voxel-init {role} 2>&1 | tee /tmp/launch.log");
    let handles = nodes.into_iter().map(|(node, n)| {
        let command = &command;
        async move {
            info!(d.log, "{node}: launch start");
            match d.exec(n, command).await {
                Ok(out) => {
                    for line in
                        out.lines().filter(|l| l.contains("[voxel-init]"))
                    {
                        info!(d.log, "{node}: {}", line.trim());
                    }
                    info!(d.log, "{node}: launch ok");
                }
                Err(e) => warn!(d.log, "{node}: launch failed: {e}"),
            }
        }
    });
    futures::future::join_all(handles).await;
}

/// One interconnect port's state on a held rack's switch.
enum PortState {
    NoSwitch,
    LinkError(anyhow::Error),
    NoLinkLocal,
    Up,
}

impl fmt::Display for PortState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSwitch => {
                write!(
                    f,
                    "waiting for the switch zone (dendrite not answering)"
                )
            }
            Self::LinkError(e) => write!(f, "link create/enable: {e}"),
            Self::NoLinkLocal => {
                write!(f, "waiting for the link-local to reach ok")
            }
            Self::Up => write!(f, "up (link-local)"),
        }
    }
}

/// Bring up the cross-rack interconnect front ports on a held (pre-RSS) rack,
/// which early networking never configures. No-op for a single rack.
async fn bring_up_interconnect(
    d: &Runner,
    topo: &Topo,
    cfg: &VoxelConfig,
    rack: usize,
) {
    const TIMEOUT: Duration = Duration::from_secs(600);
    const POLL: Duration = Duration::from_secs(5);
    let ports = cfg.interconnect_ports(rack);
    if ports.is_empty() {
        return;
    }
    let label = rack_label(cfg.topology.racks(), rack, "rack");
    // Scrimlets in slot order, matching the switch{slot} names in interconnect_ports.
    let scrimlets: Vec<(NodeRef, String)> = topo
        .sleds
        .iter()
        .filter(|(s, _)| s.rack == rack && s.scrimlet)
        .map(|(s, n)| (*n, s.name.clone()))
        .collect();
    for (sw, port) in ports {
        let Some((n, sled)) = sw
            .strip_prefix("switch")
            .and_then(|s| s.parse::<usize>().ok())
            .and_then(|slot| scrimlets.get(slot))
        else {
            continue;
        };
        let ip = match resolve_external_ip(cfg, d, sled, *n, false).await {
            Ok(ip) => ip,
            Err(e) => {
                warn!(
                    d.log,
                    "{label}: interconnect {port}: no switch IP ({e})"
                );
                continue;
            }
        };
        // The held rack's switch zone may still be installing. Poll a create,
        // enable, and addrconf pass matching rack 0's 100G no-FEC cluster ports.
        let deadline = Instant::now() + TIMEOUT;
        let mut last = String::new();
        let mut up = false;
        while Instant::now() < deadline {
            let state = if !switch_ready(&ip) {
                PortState::NoSwitch
            } else if let Err(e) = enable_link(&ip, sled, &port, "100G", "none")
            {
                PortState::LinkError(e)
            } else {
                let _ = ssh_output(
                    &ip,
                    &zlogin(&format!(
                        "ipadm create-addr -T addrconf tfport{port}_0/ll 2>/dev/null || true"
                    )),
                );
                let addr = ssh_capture(
                    &ip,
                    &zlogin(&format!(
                        "ipadm show-addr -po addrobj,state | grep tfport{port}_0"
                    )),
                )
                .unwrap_or_default();
                if addr.contains(":ok") {
                    PortState::Up
                } else {
                    PortState::NoLinkLocal
                }
            };
            if matches!(state, PortState::Up) {
                up = true;
                break;
            }
            let step = state.to_string();
            if step != last {
                info!(d.log, "{label}: interconnect {sled}:{port}: {step}");
                last = step;
            }
            tokio::time::sleep(POLL).await;
        }
        if up {
            info!(d.log, "{label}: interconnect {sled}:{port} up (link-local)");
        } else {
            warn!(
                d.log,
                "{label}: interconnect {sled}:{port}: not up within {}s",
                TIMEOUT.as_secs()
            );
        }
    }
}

pub(crate) async fn cmd_launch(
    cfg: &VoxelConfig,
    name: &str,
    opts: LaunchOpts<'_>,
) -> anyhow::Result<()> {
    const MAX_FRONT_PORTS: usize = 128;
    const BOOT_ATTEMPTS: u32 = 3;
    let racks = cfg.topology.racks();
    let sleds = cfg.sleds();

    // Omicron cannot form below 3 sleds, and the RSS to Nexus handoff needs
    // both switches, so exactly 2 scrimlets per rack.
    if cfg.topology.sleds < 3 {
        bail!(
            "each rack needs >=3 sleds (Crucible 3x replication + Cockroach/trust-quorum quorum); got {} per rack",
            cfg.topology.sleds
        );
    }
    for rack in 0..racks {
        let scrimlets =
            sleds.iter().filter(|s| s.rack == rack && s.scrimlet).count();
        if scrimlets != 2 {
            bail!(
                "rack {rack} needs exactly 2 scrimlets for the dual-switch RSS->Nexus handoff; got {scrimlets}"
            );
        }
    }
    // Each scrimlet's SoftNPU front ports: fabric uplinks plus cross-rack interconnects.
    let n_cr = cfg.fabric_router_count();
    for s in sleds.iter().filter(|s| s.scrimlet) {
        let front = n_cr + cfg.topology.interconnect_count_for(s.index);
        if front > MAX_FRONT_PORTS {
            bail!(
                "scrimlet {} needs {front} SoftNPU front ports (> {MAX_FRONT_PORTS}); \
                 reduce racks or switches-per-rack",
                s.name
            );
        }
    }
    let cp = cfg.image.cp_image();
    image::ensure_image(&cp)?;
    image::ensure_image(&cfg.image.frr_image())?;
    // A --from-tuf image bakes no sp-sim.
    if !opts.emu && is_tuf_image(&cp) {
        if !opts.init_rss {
            bail!(
                "image {cp} was built with --from-tuf and carries no sp-sim; \
                 launch with --emu, or use an image built from a commit or --src"
            );
        }
        eprintln!(
            "[voxel] warning: image {cp} was built with --from-tuf and carries \
             no sp-sim; the rack will initialize with no SPs in its inventory"
        );
    }
    memory_preflight(cfg)?;
    // Nodes are staged with static addresses on the isolated segment, so it
    // must exist before any node boots.
    if cfg.external.isolated() {
        external_up(&cfg.external, DryRun::No)
            .context("bringing up the isolated external segment")?;
    } else {
        lan_mtu_preflight()?;
    }
    reset_node_cargo_bay(cfg)?;
    stage_config(cfg, opts.emu, opts.init_rss, opts.sp_firmware)?;
    stage_sprockets(cfg)?;
    // The switch zones' MGS dials the host fleet, so it exists before any node boots.
    if opts.emu {
        sp_host::up_all(cfg, opts.emu)?;
    }
    disks::create_zvols(&image::falcon_dataset(), name, &sleds)
        .context("creating sled disks")?;

    // A failed all-VMs-at-once boot (falcon's cargo-bay mount times out under
    // the RAM spike) is torn down and retried from a fresh topology.
    let mut attempt = 1;
    let topo = loop {
        let mut topo = build_topo(cfg, name)?;
        match topo.runner.launch().await {
            Ok(()) => break topo,
            Err(e) if attempt < BOOT_ATTEMPTS => {
                warn!(
                    topo.runner.log,
                    "boot attempt {attempt}/{BOOT_ATTEMPTS} failed ({e}); tearing down + retrying"
                );
                let _ = teardown(&topo.runner, name);
                tokio::time::sleep(Duration::from_secs(3)).await;
                attempt += 1;
            }
            Err(e) => bail!("launch failed after {attempt} attempts: {e}"),
        }
    };
    let d = &topo.runner;

    // Routers first: the racks' uplink BGP needs the transit up.
    run_voxel_init(d, "router", topo.routers.clone()).await;
    if opts.no_progress {
        run_voxel_init(d, "gimlet", sled_nodes(&topo, None)).await;
        info!(d.log, "launch complete (progress watch skipped)");
    } else {
        // One rack at a time: concurrent zone-init thrashes the box hard
        // enough to knock a scrimlet over and wedge that rack's handoff.
        for rack in 0..racks {
            let rack_sleds = sled_nodes(&topo, Some(rack));
            if racks > 1 {
                info!(
                    d.log,
                    "rack{}: bringing up {} sleds",
                    rack + 1,
                    rack_sleds.len()
                );
            }
            run_voxel_init(d, "gimlet", rack_sleds).await;
            // Only rack 0 is initialized; omicron has no multirack join yet.
            if rack > 0 {
                info!(
                    d.log,
                    "rack{}: booted, left pre-RSS (unclaimed - multirack join not yet supported)",
                    rack + 1
                );
                continue;
            }
            let Some((s, n)) =
                topo.rss_sleds().into_iter().find(|(s, _)| s.rack == rack)
            else {
                continue;
            };
            let tag = rack_label(racks, rack, "rack-init");
            if !opts.init_rss
                && let Err(e) =
                    commission::drive(cfg, d, *n, &s.name, rack, &tag).await
            {
                warn!(
                    d.log,
                    "{tag}: commission setup failed: {e:#}; rack will not initialize"
                );
            }
            watch_rss(
                d,
                *n,
                &s.bootstrap_addr(),
                &tag,
                rss_watch_cap(opts.emu, racks),
                known_external_ip(cfg, &s.name),
            )
            .await;
        }
        info!(d.log, "launch complete");
    }

    // Only rack 0 has external services to route to and probe; ce's DHCP
    // address changes every bring-up.
    if let Some(ce) = topo.node_ref("ce") {
        let label = rack_label(racks, 0, "rack");
        match set_rack_route(cfg, d, ce, 0, !opts.no_route).await {
            Err(e) => warn!(d.log, "{label} external route: {e}"),
            Ok(()) => {
                let net = cfg.network.for_rack(0);
                if !opts.no_route
                    && let Some(dns_ip) = net.external_dns_ips.first()
                {
                    wait_external_reachable(
                        &d.log,
                        dns_ip,
                        &net.dns_zone,
                        &label,
                    );
                }
            }
        }
    }

    // Last, once the held racks' switch zones are past their startup dendrite
    // restart, which would wipe these runtime links.
    for rack in 1..racks {
        bring_up_interconnect(d, &topo, cfg, rack).await;
    }
    Ok(())
}

/// Kill this deployment's propolis processes that falcon no longer tracks (no
/// .falcon/<node>.pid), which would hold VNICs and zvols busy through destroy.
fn reap_orphan_propolis(name: &str, log: &Logger) -> usize {
    let mut tracked: HashSet<i32> = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(".falcon") {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("pid")
                && let Ok(s) = std::fs::read_to_string(&p)
                && let Ok(pid) = s.trim().parse::<i32>()
            {
                tracked.insert(pid);
            }
        }
    }
    // pgrep exits non-zero with no matches.
    let Some(pids) = probe_out("pgrep", &["-f", "propolis-server"]) else {
        return 0;
    };
    // Orphans are identified by this deployment's VNIC paths in their open files.
    let needle = format!("/dev/net/{name}_");
    let mut reaped = 0;
    for pid in pids.lines().filter_map(|l| l.trim().parse::<i32>().ok()) {
        if tracked.contains(&pid) {
            continue;
        }
        let holds_ours = probe_out("pfiles", &[&pid.to_string()])
            .is_some_and(|pf| pf.contains(&needle));
        if holds_ours {
            warn!(
                log,
                "reaping orphaned propolis {pid} holding {name} resources (no falcon pid file)"
            );
            let _ =
                Command::new("kill").args(["-9", &pid.to_string()]).status();
            reaped += 1;
        }
    }
    reaped
}

/// Tear down a deployment: reap orphan propolis, run falcon's destroy, then wipe
/// the node disks so the next launch boots clean even when destroy erred.
fn teardown(runner: &Runner, name: &str) -> anyhow::Result<()> {
    if reap_orphan_propolis(name, &runner.log) > 0 {
        // Let the kernel release the freed VNIC and zvol handles.
        std::thread::sleep(Duration::from_secs(1));
    }
    let result = runner.destroy();
    let topo_ds = format!("{}/topo/{name}", image::falcon_dataset());
    let wipe = Command::new("zfs").args(["destroy", "-r", &topo_ds]).output();
    match (&result, wipe) {
        (Err(e), Ok(o)) if o.status.success() => {
            warn!(
                runner.log,
                "falcon destroy reported '{e}', but node disks wiped clean ({topo_ds})"
            );
            Ok(())
        }
        _ => result.context("destroy"),
    }
}

pub(crate) fn cmd_destroy(cfg: &VoxelConfig, name: &str) -> anyhow::Result<()> {
    // Before teardown, so the fleet still goes away if falcon's destroy errors.
    sp_host::down_all(cfg);
    let topo = build_topo(cfg, name)?;
    teardown(&topo.runner, name)
}

pub(crate) fn cmd_info(cfg: &VoxelConfig, name: &str) -> anyhow::Result<()> {
    println!("topology: {name}");
    println!("  cp image:  {}", cfg.image.cp_image());
    println!("  frr image: {}", cfg.image.frr_image());
    let racks = cfg.topology.racks();
    if racks > 1 {
        println!("  racks: {racks} x {} sleds", cfg.topology.sleds);
    }
    println!("  sleds:");
    for s in cfg.sleds() {
        let role = if s.scrimlet { "scrimlet" } else { "gimlet" };
        let rss = if s.rss { "rss" } else { "" };
        let rack = if racks > 1 {
            format!("rack{} ", s.rack + 1)
        } else {
            String::new()
        };
        println!(
            "    {} {rack}[{role:<8}] {rss:<3}  bootstrap {}",
            s.name,
            s.bootstrap_addr()
        );
    }
    println!("  routers: {}", cfg.topology.routers.join(", "));
    Ok(())
}

/// RSS watch budget: 60m with emulated SPs or several racks, else 30m.
fn rss_watch_cap(emu: bool, racks: usize) -> Duration {
    Duration::from_secs(if emu || racks > 1 { 3600 } else { 1800 })
}

pub(crate) async fn cmd_status(
    cfg: &VoxelConfig,
    name: &str,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let racks = cfg.topology.racks();
    let rss_nodes = topo.rss_sleds();
    if rss_nodes.is_empty() {
        bail!("no RSS sled in topology");
    }
    let d = &topo.runner;
    // A running rack does not say whether it is --emu, so assume sp-sim.
    let watch_cap = rss_watch_cap(false, racks);
    let watchers = rss_nodes.into_iter().map(|(s, n)| {
        let tag = rack_label(racks, s.rack, "rack-init");
        let addr = s.bootstrap_addr();
        let known_ip = known_external_ip(cfg, &s.name);
        async move { watch_rss(d, *n, &addr, &tag, watch_cap, known_ip).await }
    });
    futures::future::join_all(watchers).await;
    Ok(())
}
