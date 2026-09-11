// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rack lifecycle commands: launch, route, destroy, info, status.

use anyhow::{Context, bail};
use camino::Utf8Path;
use libfalcon::{NodeRef, Runner};
use slog::{info, warn};
use std::collections::HashSet;
use std::process::Command;
use voxel_config::VoxelConfig;

use crate::isolated_external::{DryRun, link_mtu, up as external_up};
use crate::net::{
    ce_static_ip, resolve_external_ip, set_external_route, ssh_capture,
    ssh_output, wait_external_reachable, zlogin,
};
use crate::rss::watch_rss;
use crate::topo::{
    Topo, build_topo, reset_node_cargo_bay, stage_config, stage_sprockets,
};

/// A per-rack label: rackN, 1-based, for a multi-rack deployment, else the
/// single-rack fallback the caller passes.
fn rack_label(racks: usize, rack: usize, single: &str) -> String {
    if racks > 1 { format!("rack{}", rack + 1) } else { single.to_string() }
}

pub(crate) async fn cmd_route(
    cfg: &VoxelConfig,
    name: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let ce = topo.node_ref("ce").context("no ce router in topology")?;
    // One host route per rack's external prefix. All racks egress via ce.
    let racks = cfg.topology.racks();
    for rack in 0..racks {
        let prefix = cfg.network.for_rack(rack).infra_prefix;
        set_external_route(
            &topo.runner,
            ce,
            &prefix,
            !dry_run,
            ce_static_ip(cfg).as_deref(),
        )
        .await?;
    }
    Ok(())
}

/// Physical RAM in GiB via prtconf -m, which prints MB.
fn physical_ram_gb() -> Option<u64> {
    let out = Command::new("prtconf").arg("-m").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .map(|mb| mb / 1024)
}

/// Refuse a launch whose guest RAM cannot fit beside the kernel and a minimal
/// ARC. Skipped when RAM cannot be read; VOXEL_SKIP_MEM_PREFLIGHT=1 overrides.
fn memory_preflight(cfg: &VoxelConfig) -> anyhow::Result<()> {
    if std::env::var("VOXEL_SKIP_MEM_PREFLIGHT").is_ok() {
        return Ok(());
    }
    let Some(phys) = physical_ram_gb() else {
        return Ok(());
    };
    let guest = cfg.topology.guest_memory_gb();
    let vmm = (guest as f64 * 1.2).ceil() as u64;
    const RESERVE_GB: u64 = 22; // kernel (~14G observed) + minimal ARC (~8G)
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

/// The host's default route interface, or None without a default route.
fn default_route_iface() -> Option<String> {
    let out = Command::new(crate::net::ROUTE)
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("interface:"))
        .map(|s| s.trim().to_string())
}

/// Refuse a lan mode launch on a jumbo external link: voxel-init classifies
/// any NIC accepting MTU 9000 as underlay. Skipped when the MTU cannot be read.
fn lan_mtu_preflight() -> anyhow::Result<()> {
    let link = match std::env::var("EXT_INTERFACE") {
        Ok(l) => l,
        Err(_) => match default_route_iface() {
            Some(l) => l,
            None => return Ok(()),
        },
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

/// Run voxel-init with a role on each node concurrently, surfacing the
/// milestone lines. The command echoes stay in the guest's launch log.
async fn run_voxel_init(
    d: &Runner,
    items: Vec<(NodeRef, &'static str, String)>,
) {
    let handles = items.into_iter().map(|(n, command, node)| async move {
        info!(d.log, "{node}: launch start");
        match d.exec(n, command).await {
            Ok(out) => {
                for line in out.lines().filter(|l| l.contains("[voxel-init]")) {
                    info!(d.log, "{node}: {}", line.trim());
                }
                info!(d.log, "{node}: launch ok");
            }
            Err(e) => warn!(d.log, "{node}: launch failed: {e}"),
        }
    });
    futures::future::join_all(handles).await;
}

/// Bring up the cross-rack interconnect ports on a held rack, which never
/// runs RSS. Matches the cluster port config-rss carries for rack 0.
async fn bring_up_interconnect(
    d: &Runner,
    topo: &Topo,
    cfg: &VoxelConfig,
    rack: usize,
) {
    let ports = cfg.interconnect_ports(rack);
    if ports.is_empty() {
        return;
    }
    // This rack's scrimlets in slot order, matching interconnect_ports.
    let scrimlets: Vec<(NodeRef, String)> = topo
        .sleds
        .iter()
        .filter(|(s, _)| s.rack == rack && s.scrimlet)
        .map(|(s, n)| (*n, s.name.clone()))
        .collect();
    for (sw, port) in ports {
        let Some(slot) =
            sw.strip_prefix("switch").and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        let Some((n, sled)) = scrimlets.get(slot) else {
            continue;
        };
        let ip = match resolve_external_ip(cfg, d, sled, *n, false).await {
            Ok(ip) => ip,
            Err(e) => {
                warn!(
                    d.log,
                    "rack{}: interconnect {port}: no switch IP ({e})",
                    rack + 1
                );
                continue;
            }
        };
        // The switch zone may still be installing. Poll until dendrite answers,
        // create and enable the link, then wait for its link-local under one deadline.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(600);
        let mut up = false;
        let mut last = String::new();
        while std::time::Instant::now() < deadline {
            let step = if !crate::network::switch_ready(&ip) {
                "waiting for the switch zone (dendrite not answering)"
                    .to_string()
            } else if let Err(e) =
                crate::network::enable_link(&ip, sled, &port, "100G", "none")
            {
                format!("link create/enable: {e}")
            } else {
                let _ = ssh_output(
                    &ip,
                    &zlogin(&format!(
                        "ipadm create-addr -T addrconf tfport{port}_0/ll 2>/dev/null || true"
                    )),
                );
                let state = ssh_capture(
                    &ip,
                    &zlogin(&format!(
                        "ipadm show-addr -po addrobj,state | grep tfport{port}_0"
                    )),
                )
                .unwrap_or_default();
                if state.contains(":ok") {
                    up = true;
                    break;
                }
                "waiting for the link-local to reach ok".to_string()
            };
            if step != last {
                info!(
                    d.log,
                    "rack{}: interconnect {sled}:{port}: {step}",
                    rack + 1
                );
                last = step;
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        if up {
            info!(
                d.log,
                "rack{}: interconnect {sled}:{port} up (link-local)",
                rack + 1
            );
        } else {
            warn!(
                d.log,
                "rack{}: interconnect {sled}:{port}: not up within 600s",
                rack + 1
            );
        }
    }
}

pub(crate) async fn cmd_launch(
    cfg: &VoxelConfig,
    name: &str,
    config_path: &Utf8Path,
    no_progress: bool,
    no_route: bool,
    emu: bool,
    sp_firmware: Option<&Utf8Path>,
) -> anyhow::Result<()> {
    // Per rack: the control plane needs 3 sleds for Crucible and CockroachDB,
    // and the RSS to Nexus handoff needs both switches.
    let sleds = cfg.sleds();
    let racks = cfg.topology.racks();
    if cfg.topology.sleds < 3 {
        bail!(
            "each rack needs ≥3 sleds (Crucible 3x replication + Cockroach/trust-quorum quorum); got {} per rack",
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
    // SoftNPU front ports are the uplinks plus the cross-rack interconnects.
    // Guard the sidecar's port budget.
    const MAX_FRONT_PORTS: usize = 128;
    let n_cr =
        cfg.topology.routers.iter().filter(|r| r.as_str() != "ce").count();
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
    // Fail fast when the configured images are not built.
    crate::image::ensure_image(&cfg.image.cp_image())?;
    crate::image::ensure_image(&cfg.image.frr_image())?;
    memory_preflight(cfg)?;
    // The isolated segment must exist before any node boots; the staged
    // static addresses stay in use after bring-up.
    if cfg.external.isolated() {
        external_up(&cfg.external, DryRun::No)
            .context("bringing up the isolated external segment")?;
    } else {
        lan_mtu_preflight()?;
    }
    reset_node_cargo_bay(cfg)?;
    stage_config(cfg, emu, sp_firmware)?;
    stage_sprockets(cfg)?;
    // The SP fleet runs on this host and must exist before any node boots.
    if emu {
        crate::sp_host::up_all(cfg, emu)?;
    }
    // Real NVMe media for every sled, before any node boots.
    crate::disks::create_zvols(&crate::image::falcon_dataset(), name, &sleds)
        .context("creating sled disks")?;
    let mut topo = build_topo(cfg, name)?;
    // The boot spike can time out falcon's cargo-bay mount over the console.
    // A clean retry recovers it: tear down and rebuild the topology.
    const BOOT_ATTEMPTS: u32 = 3;
    let mut attempt = 1;
    loop {
        match topo.runner.launch().await {
            Ok(()) => break,
            Err(e) if attempt < BOOT_ATTEMPTS => {
                warn!(
                    topo.runner.log,
                    "boot attempt {attempt}/{BOOT_ATTEMPTS} failed ({e}); retrying"
                );
                let _ = teardown(&topo.runner, name);
                std::thread::sleep(std::time::Duration::from_secs(3));
                topo = build_topo(cfg, name)?;
                attempt += 1;
            }
            Err(e) => bail!("launch failed after {attempt} attempts: {e}"),
        }
    }

    // Capture each node's instance so an SP can bring it back as launched,
    // then start the loop that lets the SPs own the sleds' power.
    let nodes: Vec<String> = topo
        .sleds
        .iter()
        .map(|(s, _)| s.name.clone())
        .chain(topo.routers.iter().map(|(r, _)| r.clone()))
        .collect();
    crate::node::capture_missing(&nodes).await;
    if emu {
        crate::power::up_all(cfg, name, config_path)?;
    }

    // Run the in-guest agent, baked into the images at /opt/oxide/voxel-init.
    const GIMLET_LAUNCH: &str =
        "/opt/oxide/voxel-init gimlet 2>&1 | tee /tmp/launch.log";
    const ROUTER_LAUNCH: &str =
        "/opt/oxide/voxel-init router 2>&1 | tee /tmp/launch.log";
    let d = &topo.runner;

    // Customer routers first: quick, and the uplink BGP needs them.
    let routers: Vec<(NodeRef, &'static str, String)> = topo
        .routers
        .iter()
        .map(|(r, n)| (*n, ROUTER_LAUNCH, r.clone()))
        .collect();
    run_voxel_init(d, routers).await;

    if no_progress {
        // No RSS watcher as a barrier. Bring every sled up at once.
        let sleds: Vec<(NodeRef, &'static str, String)> = topo
            .sleds
            .iter()
            .map(|(s, n)| (*n, GIMLET_LAUNCH, s.name.clone()))
            .collect();
        run_voxel_init(d, sleds).await;
        info!(d.log, "launch complete (progress watch skipped)");
    } else {
        // Stagger by rack: bring up a rack's sleds and watch its RSS to
        // completion before the next. Concurrent zone init thrashes the box.
        for rack in 0..racks {
            let rack_sleds: Vec<(NodeRef, &'static str, String)> = topo
                .sleds
                .iter()
                .filter(|(s, _)| s.rack == rack)
                .map(|(s, n)| (*n, GIMLET_LAUNCH, s.name.clone()))
                .collect();
            if racks > 1 {
                info!(
                    d.log,
                    "rack{}: bringing up {} sleds",
                    rack + 1,
                    rack_sleds.len()
                );
            }
            run_voxel_init(d, rack_sleds).await;
            // Only rack 0 runs RSS. Later racks boot and stay pre-RSS, the
            // unclaimed state a cluster join would start from (RFD 573).
            if rack > 0 {
                // Without RSS the switch front ports stay unconfigured. The
                // interconnect ports come up at the end of launch.
                info!(
                    d.log,
                    "rack{}: booted, left uninitialized (no multirack join yet)",
                    rack + 1
                );
                continue;
            }
            if let Some((s, n)) =
                topo.rss_sleds().into_iter().find(|(s, _)| s.rack == rack)
            {
                let tag = rack_label(racks, rack, "rack-init");
                // No staged config-rss: drive rack setup through the commission
                // API, then watch the bring-up as usual.
                if emu
                    && let Err(e) = crate::commission::drive(
                        cfg, d, *n, &s.name, rack, &tag,
                    )
                    .await
                {
                    warn!(
                        d.log,
                        "{tag}: commission setup failed: {e:#}; rack will not initialize"
                    );
                }
                let watch_cap = rss_watch_cap(emu, racks);
                let known_ip = if cfg.external.isolated() {
                    cfg.static_external_ips()
                        .into_iter()
                        .find(|(name, _)| name == &s.name)
                        .map(|(_, ip)| ip)
                } else {
                    None
                };
                watch_rss(
                    d,
                    *n,
                    &s.bootstrap_addr(),
                    &tag,
                    watch_cap,
                    known_ip,
                )
                .await;
            }
        }
        info!(d.log, "launch complete");
    }

    // Route each rack's external prefix at this launch's ce, whose lease
    // changes every bring-up, then confirm the rack is reachable.
    if let Some(ce) = topo.node_ref("ce") {
        for rack in 0..racks {
            // Held racks have no external services or DNS. Skip them.
            if rack > 0 {
                continue;
            }
            let net = cfg.network.for_rack(rack);
            let label = rack_label(racks, rack, "rack");
            if let Err(e) = set_external_route(
                d,
                ce,
                &net.infra_prefix,
                !no_route,
                ce_static_ip(cfg).as_deref(),
            )
            .await
            {
                warn!(d.log, "{label} external route: {e}");
                continue;
            }
            if !no_route && let Some(dns_ip) = net.external_dns_ips.first() {
                wait_external_reachable(&d.log, dns_ip, &net.dns_zone, &label);
            }
        }
    }

    // Bring up the held racks' interconnect ports last, after their switch
    // zones are past the startup dendrite restart that would wipe the links.
    for rack in 1..racks {
        bring_up_interconnect(d, &topo, cfg, rack).await;
    }

    // The RoT needs nothing here: voxel-init points every SP at the shared
    // voxel-rot-emu service from boot.
    Ok(())
}

/// Kill this deployment's propolis processes that falcon lost track of, found
/// by the deployment's VNIC paths in their open files. Returns the count.
fn reap_orphan_propolis(name: &str, log: &slog::Logger) -> usize {
    // Pids falcon tracks via the workspace pid files are left to falcon.
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

    let out =
        match Command::new("pgrep").args(["-f", "propolis-server"]).output() {
            Ok(o) if o.status.success() => o.stdout,
            // pgrep exits non-zero with no matches: nothing to reap.
            _ => return 0,
        };
    let needle = format!("/dev/net/{name}_");
    let mut reaped = 0;
    for line in String::from_utf8_lossy(&out).lines() {
        let pid: i32 = match line.trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if tracked.contains(&pid) {
            continue;
        }
        // Whether this propolis holds one of this deployment's VNICs.
        let pf = match Command::new("pfiles").arg(pid.to_string()).output() {
            Ok(o) => o.stdout,
            Err(_) => continue,
        };
        if String::from_utf8_lossy(&pf).contains(&needle) {
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

/// Tear down a deployment: reap orphan propolis, run falcon's destroy, then
/// wipe the node disks unconditionally so the next launch boots clean.
fn teardown(runner: &Runner, name: &str) -> anyhow::Result<()> {
    if reap_orphan_propolis(name, &runner.log) > 0 {
        // Give the kernel a moment to release the VNIC and zvol handles.
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    let result = runner.destroy();
    let topo_ds = format!("{}/topo/{name}", crate::image::falcon_dataset());
    let wipe = std::process::Command::new("zfs")
        .args(["destroy", "-r", &topo_ds])
        .output();
    match (&result, wipe) {
        // destroy errored but the wipe succeeded: the rack is gone and clean.
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
    // The power loop first so it does not act on the sleds vanishing, then
    // the fleet, both before teardown. Scoped by rack.
    crate::power::down_all(cfg);
    crate::sp_host::down_all(cfg);
    let topo = build_topo(cfg, name)?;
    teardown(&topo.runner, name)?;
    // The isolated segment stays up for the next launch. Node addresses are
    // staged fresh each launch.
    Ok(())
}

pub(crate) fn cmd_info(cfg: &VoxelConfig, name: &str) -> anyhow::Result<()> {
    println!("topology: {name}");
    println!("  cp image:  {}", cfg.image.cp_image());
    println!("  frr image: {}", cfg.image.frr_image());
    let racks = cfg.topology.racks();
    if racks > 1 {
        println!("  racks: {racks} × {} sleds", cfg.topology.sleds);
    }
    println!("  sleds:");
    for s in cfg.sleds() {
        let role = if s.scrimlet { "scrimlet" } else { "gimlet  " };
        let rss = if s.rss { "rss" } else { "   " };
        let rack = if racks > 1 {
            format!("rack{} ", s.rack + 1)
        } else {
            String::new()
        };
        println!(
            "    {} {rack}[{role}] {rss}  bootstrap {}",
            s.name,
            s.bootstrap_addr()
        );
    }
    println!("  routers: {}", cfg.topology.routers.join(", "));
    Ok(())
}

/// RSS watch budget: 60m with emulated SPs or multiple racks, else 30m.
fn rss_watch_cap(emu_sp: bool, racks: usize) -> std::time::Duration {
    std::time::Duration::from_secs(if emu_sp || racks > 1 {
        3600
    } else {
        1800
    })
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
    // Multiple racks converge under each other's load. Watch longer.
    let watch_cap = rss_watch_cap(false, racks);
    let ips = if cfg.external.isolated() {
        cfg.static_external_ips()
    } else {
        Vec::new()
    };
    let watchers = rss_nodes.into_iter().map(|(s, n)| {
        let tag = rack_label(racks, s.rack, "rack-init");
        let addr = s.bootstrap_addr();
        let known_ip = ips
            .iter()
            .find(|(name, _)| name == &s.name)
            .map(|(_, ip)| ip.clone());
        async move { watch_rss(d, *n, &addr, &tag, watch_cap, known_ip).await }
    });
    futures::future::join_all(watchers).await;
    Ok(())
}
