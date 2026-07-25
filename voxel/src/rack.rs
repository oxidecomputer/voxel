//! Rack lifecycle commands: launch, route, destroy, info, status.

use anyhow::anyhow;
use libfalcon::{NodeRef, Runner};
use slog::{info, warn};
use std::collections::HashSet;
use std::process::Command;
use voxel_config::VoxelConfig;

use crate::net::{
    node_external_ip, set_external_route, ssh_output, wait_external_reachable,
    zlogin,
};
use crate::rss::watch_rss;
use crate::topo::{
    Topo, build_topo, reset_node_cargo_bay, stage_config, stage_sprockets,
};

/// A per-rack progress/label tag: `rackN` (1-based) when the deployment has more
/// than one rack, else the single-rack fallback the caller passes ("rack",
/// "rack-init", ...).
fn rack_label(racks: usize, rack: usize, single: &str) -> String {
    if racks > 1 { format!("rack{}", rack + 1) } else { single.to_string() }
}

pub(crate) async fn cmd_route(
    cfg: &VoxelConfig,
    name: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let ce = topo
        .node_ref("ce")
        .ok_or_else(|| anyhow!("no ce router in topology"))?;
    // One host route per rack's external prefix - all racks egress via the shared ce.
    let racks = cfg.topology.racks();
    for rack in 0..racks {
        let prefix = cfg.network.for_rack(rack).infra_prefix;
        set_external_route(
            &topo.runner,
            ce,
            &prefix,
            !dry_run,
            cfg.topology.ce_external_ip.as_deref(),
        )
        .await?;
    }
    Ok(())
}

/// Physical RAM in GiB via `prtconf -m` (illumos prints total memory in MB).
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

/// Refuse a launch that can't physically fit. Guest RAM shows up as `VMM Memory`
/// (~1.2× the requested guest RAM, from bhyve overhead) and must leave room for
/// the kernel + a minimal ZFS ARC, or the all-VMs-at-once boot thrashes - which
/// is what makes falcon's cargo-bay mount time out on the serial console. Better
/// a clear "won't fit" up front than a cryptic boot-spike timeout. Best-effort:
/// if physical RAM can't be read we skip; `VOXEL_SKIP_MEM_PREFLIGHT=1` overrides.
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
        return Err(anyhow!(
            "topology needs ~{vmm} GB guest RAM (VMM) + ~{RESERVE_GB} GB kernel/ARC headroom, \
             but this box has {phys} GB. Lower topology.sled_memory_gb (now {}) or the sled count \
             (or set VOXEL_SKIP_MEM_PREFLIGHT=1 to override).",
            cfg.topology.sled_memory_gb
        ));
    }
    Ok(())
}

/// Run `/opt/oxide/voxel-init <role>` on each given node concurrently, surfacing
/// each node's `[voxel-init]` milestone lines (the raw `+ cmd` echoes stay in the
/// guest's `/tmp/launch.log`).
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

/// Produce a rack's `config-rss.toml` by running the image-baked, commit-pinned
/// rss-gen inside its (already booted) RSS node and pulling the text back. Used
/// where the host needs config-rss out-of-band: the wicketd bodies
/// (`--wicket-setup`) and multirack staging for a future join. The normal path
/// has voxel-init generate + inject it in-guest. A sentinel fence keeps the pull
/// robust to the serial console's command echo and prompt noise.
async fn generate_rss_in_node(
    d: &Runner,
    node: NodeRef,
    rack: usize,
    out: &std::path::Path,
) -> anyhow::Result<()> {
    let cmd = format!(
        "/opt/oxide/voxel-rss-gen generate /opt/cargo-bay/voxel-effective.toml \
         /tmp/config-rss.toml --rack {rack} 1>&2; \
         echo VOXRSS_BEGIN; cat /tmp/config-rss.toml; echo VOXRSS_END"
    );
    let out_text = d
        .exec(node, &cmd)
        .await
        .map_err(|e| anyhow!("in-node rss-gen: {e}"))?;
    // Whole-line match on the markers so the console's echo of the command (which
    // contains the marker words) is ignored; take the lines strictly between them.
    let body: Vec<&str> = out_text
        .lines()
        .skip_while(|l| l.trim() != "VOXRSS_BEGIN")
        .skip(1)
        .take_while(|l| l.trim() != "VOXRSS_END")
        .map(|l| l.trim_end_matches('\r'))
        .collect();
    if body.is_empty() {
        return Err(anyhow!(
            "in-node rss-gen produced no config-rss (rack {rack})"
        ));
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, body.join("\n") + "\n")?;
    Ok(())
}

/// Bring up the cross-rack interconnect front ports on a HELD (pre-RSS) rack.
/// rack 0 gets these from early networking during RSS (rss-gen emits them as
/// AddrConf cluster ports); a rack > 0 never runs RSS, so its switch's front
/// ports are never configured. Create each interconnect port + its link-local by
/// hand in the switch zone, matching the 100G/no-FEC/AddrConf cluster port rss-gen
/// emits for rack 0, so the cross-rack DDM underlay has a live link on both ends.
/// No-op for a single rack (`interconnect_ports` is empty).
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
    // This rack's scrimlets in slot order, matching `interconnect_ports`' `switch{slot}`.
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
        let ip = match node_external_ip(d, *n, false).await {
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
        // Create the front-port link and plumb its link-local. A re-run errors
        // harmlessly ("already exists"); the addr create is guarded so it stays
        // idempotent.
        let cmd = format!(
            "/opt/oxide/dendrite/bin/swadm link create -s 100G --fec none {port}; \
             ipadm create-addr -T addrconf tfport{port}_0/ll 2>/dev/null || true"
        );
        match ssh_output(&ip, &zlogin(&cmd)) {
            Some(_) => info!(
                d.log,
                "rack{}: interconnect {sled}:{port} up (link-local)",
                rack + 1
            ),
            None => warn!(
                d.log,
                "rack{}: interconnect {sled}:{port}: switch-zone exec failed",
                rack + 1
            ),
        }
    }
}

pub(crate) async fn cmd_launch(
    cfg: &VoxelConfig,
    name: &str,
    no_progress: bool,
    no_route: bool,
    emu_sp: bool,
    emu_rot: bool,
    wicket_setup: bool,
) -> anyhow::Result<()> {
    // Floor (per rack - each is an independent RSS domain): omicron's control
    // plane can't form below 3 sleds (Crucible 3-way replication,
    // CockroachDB/trust-quorum majority), and the RSS->Nexus handoff needs both
    // switches, i.e. exactly 2 scrimlets.
    let sleds = cfg.sleds();
    let racks = cfg.topology.racks();
    if cfg.topology.sleds < 3 {
        return Err(anyhow!(
            "each rack needs ≥3 sleds (Crucible 3x replication + Cockroach/trust-quorum quorum); got {} per rack",
            cfg.topology.sleds
        ));
    }
    for rack in 0..racks {
        let scrimlets =
            sleds.iter().filter(|s| s.rack == rack && s.scrimlet).count();
        if scrimlets != 2 {
            return Err(anyhow!(
                "rack {rack} needs exactly 2 scrimlets for the dual-switch RSS->Nexus handoff; got {scrimlets}"
            ));
        }
    }
    // Each scrimlet's SoftNPU front ports = fabric uplinks + cross-rack
    // interconnects. Guard against exceeding the sidecar's port budget (the full
    // cross-rack mesh grows with racks*switches).
    const MAX_FRONT_PORTS: usize = 128;
    let n_cr =
        cfg.topology.routers.iter().filter(|r| r.as_str() != "ce").count();
    for s in sleds.iter().filter(|s| s.scrimlet) {
        let front = n_cr + cfg.topology.interconnect_count_for(s.index);
        if front > MAX_FRONT_PORTS {
            return Err(anyhow!(
                "scrimlet {} needs {front} SoftNPU front ports (> {MAX_FRONT_PORTS}); \
                 reduce racks or switches-per-rack",
                s.name
            ));
        }
    }
    // Fail fast if the configured images aren't built yet - a clear message
    // beats the cryptic clone error falcon would throw partway through launch.
    crate::image::ensure_image(&cfg.image.cp_image())?;
    crate::image::ensure_image(&cfg.image.frr_image())?;
    memory_preflight(cfg)?;
    reset_node_cargo_bay(cfg)?;
    stage_config(cfg, emu_sp, emu_rot, wicket_setup)?;
    stage_sprockets(cfg)?;
    let mut topo = build_topo(cfg, name)?;
    // The all-VMs-at-once boot grabs ~all the guest RAM in one spike; under that
    // pressure falcon's cargo-bay mount over the serial console can transiently
    // time out ("[sc] <node>: timeout waiting for data") and abort the whole
    // boot. It's recoverable on a clean retry, so do that automatically: tear
    // down the partial boot (releasing VNICs/zvols) and rebuild a fresh topology.
    const BOOT_ATTEMPTS: u32 = 3;
    let mut attempt = 1;
    loop {
        match topo.runner.launch().await {
            Ok(()) => break,
            Err(e) if attempt < BOOT_ATTEMPTS => {
                warn!(
                    topo.runner.log,
                    "boot attempt {attempt}/{BOOT_ATTEMPTS} failed ({e}); tearing down + retrying"
                );
                let _ = teardown(&topo.runner, name);
                std::thread::sleep(std::time::Duration::from_secs(3));
                topo = build_topo(cfg, name)?;
                attempt += 1;
            }
            Err(e) => {
                return Err(anyhow!(
                    "launch failed after {attempt} attempts: {e}"
                ));
            }
        }
    }

    // Run the in-guest agent, baked into the images at /opt/oxide/voxel-init.
    const GIMLET_LAUNCH: &str =
        "/opt/oxide/voxel-init gimlet 2>&1 | tee /tmp/launch.log";
    const ROUTER_LAUNCH: &str =
        "/opt/oxide/voxel-init router 2>&1 | tee /tmp/launch.log";
    let d = &topo.runner;

    // Customer routers (the shared transit) first - quick, and must be up for
    // the racks' uplink BGP.
    let routers: Vec<(NodeRef, &'static str, String)> = topo
        .routers
        .iter()
        .map(|(r, n)| (*n, ROUTER_LAUNCH, r.clone()))
        .collect();
    run_voxel_init(d, routers).await;

    if no_progress {
        // No RSS watcher to use as a barrier, so bring every sled up at once.
        let sleds: Vec<(NodeRef, &'static str, String)> = topo
            .sleds
            .iter()
            .map(|(s, n)| (*n, GIMLET_LAUNCH, s.name.clone()))
            .collect();
        run_voxel_init(d, sleds).await;
        info!(d.log, "launch complete (progress watch skipped)");
    } else {
        // **Stagger by rack.** Bring up each rack's sleds and watch its RSS to
        // completion before starting the next rack. Running two racks' heavy
        // zone-init concurrently thrashes the box hard enough to knock a scrimlet
        // over mid-bring-up - which loses its runtime switch-slot identity and
        // wedges that rack's Nexus handoff (the switch1-reverts-to-switch0 bug).
        // One rack at a time keeps the box within its I/O budget. A single rack
        // behaves exactly as before.
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
            // Multirack: only rack 0 (the cluster) runs RSS. rack > 0 boots (sleds +
            // the cross-rack interconnect wired) but is left PRE-RSS - the unclaimed
            // state a future cluster-join (RFD 573) would start from. omicron can't
            // join racks into one AZ yet, so we stage it and stop here.
            if rack > 0 {
                // RSS won't run here, so early networking never configures this
                // rack's switch front ports. Bring up the interconnect ports by
                // hand so the cross-rack DDM underlay has a live link on both ends.
                bring_up_interconnect(d, &topo, cfg, rack).await;
                // Stage this held rack's config-rss (for a future cluster-join)
                // from its baked in-guest rss-gen.
                if let Some((_, rn)) =
                    topo.rss_sleds().into_iter().find(|(s, _)| s.rack == rack)
                {
                    let staged = std::path::Path::new("multirack-staged")
                        .join(format!("rack{rack}"))
                        .join("config-rss.toml");
                    if let Err(e) =
                        generate_rss_in_node(d, *rn, rack, &staged).await
                    {
                        warn!(
                            d.log,
                            "rack{}: staging config-rss failed: {e}",
                            rack + 1
                        );
                    }
                }
                info!(
                    d.log,
                    "rack{}: booted, left pre-RSS (unclaimed - multirack join not yet supported)",
                    rack + 1
                );
                continue;
            }
            if let Some((s, n)) =
                topo.rss_sleds().into_iter().find(|(s, _)| s.rack == rack)
            {
                let tag = rack_label(racks, rack, "rack-init");
                // --wicket-setup: nothing auto-inited (no staged config-rss), so
                // drive RSS through wicketd (upload config + cert + recovery
                // password, then POST to start). watch_rss then reports the
                // wicketd-triggered bring-up exactly as for the file path.
                if wicket_setup {
                    let net = cfg.network.for_rack(rack);
                    let config_rss = std::path::Path::new("wicket-setup")
                        .join(format!("rack{rack}"))
                        .join("config-rss.toml");
                    // wicketd suppresses auto-RSS, so config-rss isn't staged in
                    // the cargo-bay; produce it now from the baked in-guest rss-gen.
                    if let Err(e) =
                        generate_rss_in_node(d, *n, rack, &config_rss).await
                    {
                        warn!(
                            d.log,
                            "{tag}: in-node rss-gen failed: {e}; rack will not initialize"
                        );
                    }
                    // wicketd's bootstrap_sleds must be THIS rack's cubby slots =
                    // its sleds' GLOBAL indices (rack 1 -> 3,4,5), matching what the
                    // MGS sim reports (`location = ["sled", global_index]`); a flat
                    // 0..n only correlates for rack 0.
                    let slots: Vec<u16> = topo
                        .sleds
                        .iter()
                        .filter(|(s, _)| s.rack == rack)
                        .map(|(s, _)| s.index as u16)
                        .collect();
                    if let Err(e) = crate::wicket_setup::drive(
                        d,
                        *n,
                        &slots,
                        &config_rss,
                        &net.dns_zone,
                        &tag,
                    )
                    .await
                    {
                        warn!(
                            d.log,
                            "{tag}: wicket-setup failed: {e}; rack will not initialize"
                        );
                    }
                }
                let watch_cap = rss_watch_cap(emu_sp, racks);
                watch_rss(d, *n, &s.bootstrap_addr(), &tag, watch_cap).await;
            }
        }
        info!(d.log, "launch complete");
    }

    // Point the host route at this launch's ce for each rack's external prefix
    // (all racks egress via the shared ce; ce's DHCP IP changes every bring-up),
    // then confirm the rack is actually reachable before declaring it usable - a
    // route isn't reachability (the shared transit can briefly flap the first
    // rack's path as the second rack joins).
    if let Some(ce) = topo.node_ref("ce") {
        for rack in 0..racks {
            let net = cfg.network.for_rack(rack);
            let label = rack_label(racks, rack, "rack");
            if let Err(e) = set_external_route(
                d,
                ce,
                &net.infra_prefix,
                !no_route,
                cfg.topology.ce_external_ip.as_deref(),
            )
            .await
            {
                warn!(d.log, "{label} external route: {e}");
                continue;
            }
            if !no_route {
                if let Some(dns_ip) = net.external_dns_ips.first() {
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

    // --emu-rot: nothing to attach here anymore. voxel-init stands up a shared
    // `voxel-rot-emu` service per switch zone and points every SP at it via
    // SP_EMU_ROT_SERVICE from boot, so each SP stays single-core and the RoT
    // bridge is live through RSS -- MGS/Nexus pin the real RoT at rack-init.
    Ok(())
}

/// Kill propolis processes that belong to this deployment but that falcon won't
/// reap itself. A node whose `.falcon/<node>.pid` went missing (a partially
/// failed prior teardown) leaves an orphaned propolis holding that node's VNICs
/// and zvol busy - which then wedges *this* destroy: link teardown aborts with
/// "Device busy", and the follow-up zvol wipe can't proceed either. We identify
/// orphans by the deployment-prefixed VNIC paths in their open files
/// (`/dev/net/<name>_*`, e.g. `/dev/net/voxel_g3_sn_vnic0`), so this is scoped
/// to this rack and never touches another deployment's propolis. Pids falcon
/// already tracks via the workspace pid files are left for falcon to kill.
/// Returns how many it reaped.
fn reap_orphan_propolis(name: &str, log: &slog::Logger) -> usize {
    // Pids falcon tracks via the workspace pid files - leave those to falcon.
    let mut tracked: HashSet<i32> = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(".falcon") {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("pid") {
                if let Ok(s) = std::fs::read_to_string(&p) {
                    if let Ok(pid) = s.trim().parse::<i32>() {
                        tracked.insert(pid);
                    }
                }
            }
        }
    }

    let out =
        match Command::new("pgrep").args(["-f", "propolis-server"]).output() {
            Ok(o) if o.status.success() => o.stdout,
            // pgrep exits non-zero when there are no matches - nothing to reap.
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
        // Does this propolis hold one of THIS deployment's VNICs?
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

/// Tear down a deployment's falcon resources and guarantee a clean slate. Reap
/// orphan propolis the workspace can't (a node whose `.falcon/<node>.pid` went
/// missing leaves one holding VNICs/zvol busy, which would wedge the teardown),
/// run falcon's own destroy, then unconditionally wipe the node disks - falcon's
/// destroy tears down nodes -> links -> zvols -> workspace and bails on the first
/// busy resource, which can leave the persistent `topo/<name>` datasets (and
/// their stale crucible/trust-quorum ledger) behind, so the next launch boots
/// dirty (RSS falsely reports an already-initialized rack). Ok if the rack is
/// gone + disks clean, even when falcon's destroy erred but the wipe succeeded.
/// Shared by `cmd_destroy` and the boot-retry path.
fn teardown(runner: &Runner, name: &str) -> anyhow::Result<()> {
    if reap_orphan_propolis(name, &runner.log) > 0 {
        // Give the kernel a moment to release the freed VNIC/zvol handles.
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    let result = runner.destroy();
    let topo_ds = format!("{}/topo/{name}", crate::image::falcon_dataset());
    let wipe = std::process::Command::new("zfs")
        .args(["destroy", "-r", &topo_ds])
        .output();
    match (&result, wipe) {
        // destroy errored but the disk wipe succeeded - the rack is gone + clean.
        (Err(e), Ok(o)) if o.status.success() => {
            warn!(
                runner.log,
                "falcon destroy reported '{e}', but node disks wiped clean ({topo_ds})"
            );
            Ok(())
        }
        _ => result.map_err(|e| anyhow!("destroy: {e}")),
    }
}

pub(crate) fn cmd_destroy(cfg: &VoxelConfig, name: &str) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    teardown(&topo.runner, name)
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

/// RSS watch budget: emulated SPs slow every MGS RPC and multi-rack racks
/// converge under each other's load, so both get 60m vs the 30m a single sp-sim
/// rack needs. (`cmd_status` watches a running rack with no emu_sp context, so it
/// passes `false`.)
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
        return Err(anyhow!("no RSS sled in topology"));
    }
    let d = &topo.runner;
    // Multi-rack racks converge under each other's load - watch longer (matches
    // cmd_launch). Duration is Copy, so each watcher closure gets its own.
    let watch_cap = rss_watch_cap(false, racks);
    let watchers = rss_nodes.into_iter().map(|(s, n)| {
        let tag = rack_label(racks, s.rack, "rack-init");
        let addr = s.bootstrap_addr();
        async move { watch_rss(d, *n, &addr, &tag, watch_cap).await }
    });
    futures::future::join_all(watchers).await;
    Ok(())
}
