// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `voxel network` - show the network topology and validate live networking.
//!
//! - `show`: render the per-rack network projection + switches + the auto
//!   cross-rack sidecar interconnect mesh (config-derived, rack-down).
//! - `link-up` / `link-down`: transient/debug - create+enable / disable+delete
//!   a switch port's link directly via `swadm`. Nexus's switch-port reconciler
//!   reaps manual `swadm`/`mgadm` changes (~30s), so these do not persist;
//!   persistent switch config must go through the Oxide API. Useful for a quick
//!   poke / proving a link comes up.
//! - `validate`: live checks against a running rack - per switch zone the link
//!   states (`swadm link ls`), BGP sessions (`mgadm bgp status`), and programmed
//!   routes (`swadm route list`), plus the host route.

use anyhow::{Context, bail};
use voxel_config::{SledDesc, VoxelConfig};

use crate::net::{resolve_external_ip, ssh_capture, ssh_output, zlogin};
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
    let nfr =
        cfg.topology.routers.iter().filter(|r| r.as_str() != "ce").count();
    println!(
        "network topology - {racks} rack{}",
        if racks == 1 { "" } else { "s" }
    );

    let sw = switches(cfg);
    for rack in 0..racks {
        let net = cfg.network.for_rack(rack);
        println!();
        println!("rack{} [{}]", rack + 1, net.dns_zone);
        println!("  customer prefix : {}", net.infra_prefix);
        println!(
            "  service pool    : {} - {}",
            net.service_pool_first, net.service_pool_last
        );
        println!("  external DNS    : {}", net.external_dns_ips.join(", "));
        println!("  rack subnet     : {}", net.rack_subnet);
        println!("  BGP ASN         : {}", net.bgp_asn);
        for (slot, (gidx, s)) in
            sw.iter().filter(|(_, s)| s.rack == rack).enumerate()
        {
            println!("  switch{slot:<9} {} (global switch{gidx})", s.name);
        }
    }

    println!();
    let pairs = cfg.topology.interconnect_pairs();
    if pairs.is_empty() {
        println!("cross-rack interconnects: none (single rack)");
    } else {
        println!(
            "cross-rack sidecar interconnects (auto full mesh; softnpu_links, land on qsfp{nfr}+):"
        );
        for (a, b) in &pairs {
            println!("  g{a} <-> g{b}");
        }
    }
    Ok(())
}

// --- link-up / link-down (live) --------------------------------------------

/// Resolve a switch selector to its scrimlet name + host-LAN IP on a running rack.
async fn switch_ip(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
) -> anyhow::Result<(String, String)> {
    let topo = build_topo(cfg, name)?;
    let (sw, n) = {
        let (sd, nr) = crate::access::resolve_switch(&topo, switch)?;
        (sd.name.clone(), *nr)
    };
    let ip = resolve_external_ip(cfg, &topo.runner, &sw, n, false)
        .await
        .context("is the rack up? (`voxel status`)")?;
    Ok((sw, ip))
}

/// Create (if needed) + enable a link on a switch port in the switch zone at
/// `ip`, checking the `CREATE_OK`/`ENABLE_OK` markers so callers can retry on the
/// transient switch-zone exec flakiness. Does NOT plumb an address. Shared by
/// `link_up` (the operator command) and rack.rs's held-rack interconnect bring-up.
pub(crate) fn enable_link(
    ip: &str,
    sw: &str,
    port: &str,
    speed: &str,
    fec: &str,
) -> anyhow::Result<()> {
    let link = format!("{port}/0");
    let present =
        ssh_capture(ip, &zlogin(&format!("{SWADM} link get {link} 2>&1")))
            .map(|o| o.contains(port))
            .unwrap_or(false);
    if !present {
        let create = zlogin(&format!(
            "{SWADM} link create -s {speed} --fec {fec} {port} 2>&1 && echo CREATE_OK"
        ));
        let out = ssh_output(ip, &create).unwrap_or_default();
        if !out.contains("CREATE_OK") {
            bail!("link create {link} on {sw} failed: {}", out.trim());
        }
    }
    let en = ssh_output(
        ip,
        &zlogin(&format!("{SWADM} link enable {link} 2>&1 && echo ENABLE_OK")),
    )
    .unwrap_or_default();
    if !en.contains("ENABLE_OK") {
        bail!("link enable {link} on {sw} failed: {}", en.trim());
    }
    Ok(())
}

/// Whether the switch zone at `ip` is ready to configure: installed,
/// zlogin-able, and dendrite answering its API.
pub(crate) fn switch_ready(ip: &str) -> bool {
    ssh_capture(
        ip,
        &zlogin(&format!("{SWADM} link ls >/dev/null 2>&1 && echo DPD_OK")),
    )
    .map(|o| o.contains("DPD_OK"))
    .unwrap_or(false)
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
    eprintln!(
        "[voxel] {sw} ({ip}): bringing up link {link} ({speed}, fec {fec})"
    );
    enable_link(&ip, &sw, port, speed, fec)?;
    // Brief settle, then show the link state (Down until the peer end is up too).
    std::thread::sleep(std::time::Duration::from_secs(2));
    let st =
        ssh_capture(&ip, &zlogin(&format!("{SWADM} link get {link} 2>&1")))
            .unwrap_or_default();
    print!("{st}");
    eprintln!(
        "[voxel] {sw}: {link} created + enabled (reaches Up once the peer switch's {port} is also up)"
    );
    eprintln!(
        "[voxel] ⚠️  transient: Nexus's reconciler will reap this manual link in ~30s - debug only; use the Oxide API to persist"
    );
    Ok(())
}

/// `voxel network link-down <switch> <port>` - disable + delete a switch port's
/// link in the live switch zone.
pub(crate) async fn link_down(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
    port: &str,
) -> anyhow::Result<()> {
    let (sw, ip) = switch_ip(cfg, name, switch).await?;
    let link = format!("{port}/0");
    eprintln!("[voxel] {sw} ({ip}): taking down link {link}");
    let _ =
        ssh_output(&ip, &zlogin(&format!("{SWADM} link disable {link} 2>&1")));
    let out = ssh_output(
        &ip,
        &zlogin(&format!("{SWADM} link delete {link} 2>&1 && echo DELETE_OK")),
    )
    .unwrap_or_default();
    if out.contains("DELETE_OK") {
        println!("{sw}: link {link} disabled + deleted");
        Ok(())
    } else {
        bail!("link delete {link} on {sw} failed: {}", out.trim())
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

pub(crate) async fn validate(
    cfg: &VoxelConfig,
    name: &str,
    detail: bool,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let racks = cfg.topology.racks();
    println!(
        "validating live network for '{name}'{} ...",
        if detail { " (--detail)" } else { "" }
    );

    for (s, n) in topo.sleds.iter().filter(|(s, _)| s.scrimlet) {
        let ip =
            match resolve_external_ip(cfg, &topo.runner, &s.name, *n, false)
                .await
            {
                Ok(ip) => ip,
                Err(e) => {
                    println!("  switch {} : UNREACHABLE ({e})", s.name);
                    continue;
                }
            };
        let asn = cfg.network.for_rack(s.rack).bgp_asn;
        println!("  switch {} ({ip}, rack{}):", s.name, s.rack + 1);
        let zl = |c: String| {
            ssh_capture(&ip, &format!("{} 2>&1", zlogin(&c)))
                .unwrap_or_default()
        };
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
            let (up, down) =
                (count_lines(&links, "Up"), count_lines(&links, "Down"));
            let nports = ports.lines().filter(|l| !l.trim().is_empty()).count();
            println!(
                "    links  : {up} up, {down} down  ({nports} switch ports)"
            );
            println!(
                "    bgp    : {} established (asn {asn})",
                count_lines(&bgp, "Established")
            );
            let xrack =
                routes.lines().filter(|l| l.contains("198.51.10")).count();
            println!(
                "    routes : {} entries{}",
                routes.lines().count(),
                if racks > 1 {
                    format!(", {xrack} customer /24")
                } else {
                    String::new()
                }
            );
        }
    }

    // Host route per rack (the box's path to each customer prefix).
    println!("  host routes:");
    for rack in 0..racks {
        let net = cfg.network.for_rack(rack);
        let dest = net
            .infra_prefix
            .split('/')
            .next()
            .unwrap_or(&net.infra_prefix)
            .to_string();
        let gw = std::process::Command::new(crate::net::ROUTE)
            .args(["-n", "get", &dest])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .and_then(|s| {
                s.lines().find_map(|l| {
                    l.trim()
                        .strip_prefix("gateway:")
                        .map(|g| g.trim().to_string())
                })
            })
            .unwrap_or_else(|| "(none)".into());
        println!("    rack{} {} -> {gw}", rack + 1, net.infra_prefix);
    }
    Ok(())
}
