//! `voxel network` - show the network topology, manage switch interconnects, and
//! validate live networking.
//!
//!  - `show`     : render the per-rack network projection + switches + the
//!                 configured switch interconnects (config-derived; works rack-down).
//!  - `add-port` / `rm-port` : manage `[topology].interconnects` - direct
//!                 sidecar<->sidecar links (applied on the next launch). The
//!                 interconnect plumbing is in [`crate::topo`] / `voxel-config`.
//!  - `link-up` / `link-down` : ⚠️ TRANSIENT/DEBUG - create+enable / disable+delete
//!                 a switch port's link directly via `swadm`. Nexus's switch-port
//!                 reconciler reaps manual `swadm`/`mgadm` changes (~30s), so these
//!                 do NOT persist; persistent switch config must go through the
//!                 Oxide API. Useful for a quick poke / proving a link comes up.
//!  - `validate` : live checks against a running rack - per switch zone the link
//!                 states (`swadm link ls`), BGP sessions (`mgadm bgp status`),
//!                 and programmed routes (`swadm route list`), plus the host route.

use anyhow::{anyhow, Context};
use std::path::Path;
use voxel_config::{config as vcfg, SledDesc, VoxelConfig};

use crate::net::{node_external_ip, ssh_capture, ssh_output, zlogin};
use crate::topo::build_topo;

const SWADM: &str = "/opt/oxide/dendrite/bin/swadm";
const MGADM: &str = "/opt/oxide/mgd/bin/mgadm";

/// The global switch index (`switchN`) for each scrimlet, in order.
fn switches(cfg: &VoxelConfig) -> Vec<(usize, SledDesc)> {
    cfg.sleds().into_iter().filter(|s| s.scrimlet).enumerate().collect()
}

// --- show ------------------------------------------------------------------

pub(crate) fn show(cfg: &VoxelConfig) -> anyhow::Result<()> {
    let racks = cfg.topology.racks();
    let nfr = cfg.topology.routers.iter().filter(|r| r.as_str() != "ce").count();
    println!("network topology - {racks} rack{}", if racks == 1 { "" } else { "s" });

    let sw = switches(cfg);
    for rack in 0..racks {
        let net = cfg.network.for_rack(rack);
        println!();
        println!("rack{} [{}]", rack + 1, net.dns_zone);
        println!("  customer prefix : {}", net.infra_prefix);
        println!("  service pool    : {} - {}", net.service_pool_first, net.service_pool_last);
        println!("  external DNS    : {}", net.external_dns_ips.join(", "));
        println!("  rack subnet     : {}", net.rack_subnet);
        println!("  BGP ASN         : {}", net.bgp_asn);
        let mut slot = 0;
        for (gidx, s) in sw.iter().filter(|(_, s)| s.rack == rack) {
            println!("  switch{slot:<9} {} (global switch{gidx})", s.name);
            slot += 1;
        }
    }

    println!();
    if cfg.topology.interconnects.is_empty() {
        println!("switch interconnects: none");
        println!("  add one with: voxel network add-port <a> <b>   (e.g. switch0 switch1)");
    } else {
        println!("switch interconnects (softnpu_links sidecar<->sidecar; land on qsfp{nfr}+, applied at launch):");
        for (a, b) in &cfg.topology.interconnects {
            match (cfg.topology.resolve_switch_index(a), cfg.topology.resolve_switch_index(b)) {
                (Some(ai), Some(bi)) => println!("  {a} <-> {b}   (g{ai} <-> g{bi})"),
                _ => println!("  {a} <-> {b}   (UNRESOLVED - check selectors against `show` above)"),
            }
        }
    }
    Ok(())
}

// --- add-port / rm-port ----------------------------------------------------

/// Serialize the interconnect list to a TOML array and persist it via the
/// toml-edit setter (preserves the rest of the file).
fn write_interconnects(path: &Path, pairs: &[(String, String)]) -> anyhow::Result<()> {
    let arr = format!(
        "[{}]",
        pairs.iter().map(|(a, b)| format!("[\"{a}\", \"{b}\"]")).collect::<Vec<_>>().join(", ")
    );
    let text = crate::config_text(path)?;
    let updated = vcfg::set(&text, "topology.interconnects", &arr).map_err(|e| anyhow!(e))?;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).ok();
        }
    }
    std::fs::write(path, &updated).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub(crate) fn add_port(path: &Path, a: &str, b: &str) -> anyhow::Result<()> {
    let cfg = crate::load_config(path)?;
    let (ra, rb) = (cfg.topology.resolve_switch_index(a), cfg.topology.resolve_switch_index(b));
    let bad = [(a, ra), (b, rb)].into_iter().find(|(_, r)| r.is_none());
    if let Some((sel, _)) = bad {
        return Err(anyhow!(
            "can't resolve switch '{sel}' (use switch0 | switch1 | switchN | rackR/switchS) - see `voxel network show`"
        ));
    }
    if ra == rb {
        return Err(anyhow!("'{a}' and '{b}' resolve to the same switch (g{})", ra.unwrap()));
    }
    let mut pairs = cfg.topology.interconnects.clone();
    if pairs.iter().any(|(x, y)| (x == a && y == b) || (x == b && y == a)) {
        println!("interconnect {a} <-> {b} already present");
        return Ok(());
    }
    pairs.push((a.to_string(), b.to_string()));
    write_interconnects(path, &pairs)?;
    println!("added interconnect {a} <-> {b}  (takes effect on the next `voxel launch`)");
    Ok(())
}

pub(crate) fn rm_port(path: &Path, a: &str, b: &str) -> anyhow::Result<()> {
    let cfg = crate::load_config(path)?;
    let before = cfg.topology.interconnects.len();
    let pairs: Vec<(String, String)> = cfg
        .topology
        .interconnects
        .iter()
        .filter(|(x, y)| !((x == a && y == b) || (x == b && y == a)))
        .cloned()
        .collect();
    if pairs.len() == before {
        return Err(anyhow!("no interconnect {a} <-> {b} (see `voxel network show`)"));
    }
    write_interconnects(path, &pairs)?;
    println!("removed interconnect {a} <-> {b}  (takes effect on the next `voxel launch`)");
    Ok(())
}

// --- link-up / link-down (live) --------------------------------------------

/// Resolve a switch selector to its scrimlet name + host-LAN IP on a running rack.
async fn switch_ip(cfg: &VoxelConfig, name: &str, switch: &str) -> anyhow::Result<(String, String)> {
    let topo = build_topo(cfg, name)?;
    let (sw, n) = {
        let (sd, nr) = crate::access::resolve_switch(&topo, switch)?;
        (sd.name.clone(), *nr)
    };
    let ip = node_external_ip(&topo.runner, n, false)
        .await
        .map_err(|e| anyhow!("{e} - is the rack up? (`voxel status`)"))?;
    Ok((sw, ip))
}

/// `voxel network link-up <switch> <port>` - create (if needed) + enable a link
/// on a switch port (e.g. the interconnect `qsfp2`) in the live switch zone. The
/// link only reaches `Up` once BOTH ends are enabled, so for an interconnect run
/// it on each switch. ⚠️ TRANSIENT: Nexus's switch-port reconciler reaps manual
/// `swadm` links within ~30s - this is a debug/poke tool, not persistence.
/// Persistent switch config must go through the Oxide API.
pub(crate) async fn link_up(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
    port: &str,
    speed: &str,
    fec: &str,
) -> anyhow::Result<()> {
    let (sw, ip) = switch_ip(cfg, name, switch).await?;
    let link = format!("{port}/0");
    eprintln!("[voxel] {sw} ({ip}): bringing up link {link} ({speed}, fec {fec})");
    let present = ssh_capture(&ip, &zlogin(&format!("{SWADM} link get {link} 2>&1")))
        .map(|o| o.contains(port))
        .unwrap_or(false);
    if present {
        eprintln!("[voxel] {link} already exists; (re)enabling");
    } else {
        let create =
            zlogin(&format!("{SWADM} link create -s {speed} --fec {fec} {port} 2>&1 && echo CREATE_OK"));
        let out = ssh_output(&ip, &create).unwrap_or_default();
        if !out.contains("CREATE_OK") {
            return Err(anyhow!("link create {link} on {sw} failed: {}", out.trim()));
        }
    }
    let en = ssh_output(&ip, &zlogin(&format!("{SWADM} link enable {link} 2>&1 && echo ENABLE_OK")))
        .unwrap_or_default();
    if !en.contains("ENABLE_OK") {
        return Err(anyhow!("link enable {link} on {sw} failed: {}", en.trim()));
    }
    // Brief settle, then show the link state (Down until the peer end is up too).
    std::thread::sleep(std::time::Duration::from_secs(2));
    let st = ssh_capture(&ip, &zlogin(&format!("{SWADM} link get {link} 2>&1"))).unwrap_or_default();
    print!("{st}");
    eprintln!("[voxel] {sw}: {link} created + enabled (reaches Up once the peer switch's {port} is also up)");
    eprintln!("[voxel] ⚠️  transient: Nexus's reconciler will reap this manual link in ~30s - debug only; use the Oxide API to persist");
    Ok(())
}

/// `voxel network link-down <switch> <port>` - disable + delete a switch port's
/// link in the live switch zone.
pub(crate) async fn link_down(cfg: &VoxelConfig, name: &str, switch: &str, port: &str) -> anyhow::Result<()> {
    let (sw, ip) = switch_ip(cfg, name, switch).await?;
    let link = format!("{port}/0");
    eprintln!("[voxel] {sw} ({ip}): taking down link {link}");
    let _ = ssh_output(&ip, &zlogin(&format!("{SWADM} link disable {link} 2>&1")));
    let out = ssh_output(&ip, &zlogin(&format!("{SWADM} link delete {link} 2>&1 && echo DELETE_OK")))
        .unwrap_or_default();
    if out.contains("DELETE_OK") {
        println!("{sw}: link {link} disabled + deleted");
        Ok(())
    } else {
        Err(anyhow!("link delete {link} on {sw} failed: {}", out.trim()))
    }
}

// --- validate (live) -------------------------------------------------------

/// Count `swadm`/`mgadm` lines containing `needle` (e.g. `Up`, `Established`).
fn count_lines(out: &str, needle: &str) -> usize {
    out.lines().filter(|l| l.contains(needle)).count()
}

/// Print an indented, titled block of multi-line tool output (`--detail`).
fn section(title: &str, body: &str) {
    println!("    {title}:");
    let body = body.trim();
    if body.is_empty() {
        println!("      (no output / command unavailable)");
    } else {
        for l in body.lines() {
            println!("      {l}");
        }
    }
}

pub(crate) async fn validate(cfg: &VoxelConfig, name: &str, detail: bool) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let racks = cfg.topology.racks();
    println!("validating live network for '{name}'{} ...", if detail { " (--detail)" } else { "" });

    for (s, n) in topo.sleds.iter().filter(|(s, _)| s.scrimlet) {
        let ip = match node_external_ip(&topo.runner, *n, false).await {
            Ok(ip) => ip,
            Err(e) => {
                println!("  switch {} : UNREACHABLE ({e})", s.name);
                continue;
            }
        };
        let asn = cfg.network.for_rack(s.rack).bgp_asn;
        println!("  switch {} ({ip}, rack{}):", s.name, s.rack + 1);
        let zl = |c: String| ssh_capture(&ip, &format!("{} 2>&1", zlogin(&c))).unwrap_or_default();
        let links = zl(format!("{SWADM} link ls"));
        let ports = zl(format!("{SWADM} switch-port ls"));
        let bgp = zl(format!("{MGADM} bgp status neighbors {asn}"));
        let routes = zl(format!("{SWADM} route list"));

        if detail {
            section("links (swadm link ls)", &links);
            section("switch ports (swadm switch-port ls)", &ports);
            section(&format!("bgp (mgadm bgp status neighbors {asn})"), &bgp);
            section("routes (swadm route list)", &routes);
        } else {
            let (up, down) = (count_lines(&links, "Up"), count_lines(&links, "Down"));
            let nports = ports.lines().filter(|l| !l.trim().is_empty()).count();
            println!("    links  : {up} up, {down} down  ({nports} switch ports)");
            println!("    bgp    : {} established (asn {asn})", count_lines(&bgp, "Established"));
            let xrack = routes.lines().filter(|l| l.contains("198.51.10")).count();
            println!(
                "    routes : {} entries{}",
                routes.lines().count(),
                if racks > 1 { format!(", {xrack} customer /24") } else { String::new() }
            );
        }
    }

    // Host route per rack (the box's path to each customer prefix).
    println!("  host routes:");
    for rack in 0..racks {
        let net = cfg.network.for_rack(rack);
        let dest = net.infra_prefix.split('/').next().unwrap_or(&net.infra_prefix).to_string();
        let gw = std::process::Command::new("route")
            .args(["-n", "get", &dest])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .and_then(|s| {
                s.lines().find_map(|l| l.trim().strip_prefix("gateway:").map(|g| g.trim().to_string()))
            })
            .unwrap_or_else(|| "(none)".into());
        println!("    rack{} {} -> {gw}", rack + 1, net.infra_prefix);
    }
    Ok(())
}
