//! Router/edge bring-up—replaces `router-launch.sh`. Runs in the voxel-frr
//! debian guest. FRR + bgpd are pre-installed; this applies the generated
//! unnumbered `frr.conf` and NATs rack egress out to the host LAN (the RSS
//! time-sync path—the boundary NTP zone must reach its upstream).

use crate::sys::{ExternalNet, capture, note, read_external_net, run, run_quiet, warn};
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::Path;
use std::time::Duration;

pub fn bring_up() -> Result<()> {
    // Forwarding is baked, but re-assert + stop lab-net RAs from clobbering us.
    sysctl("net.ipv4.ip_forward", "1");
    sysctl("net.ipv6.conf.all.forwarding", "1");
    sysctl("net.ipv6.conf.all.accept_ra", "0");

    // apt-daily timers can wipe FRR state (disabled at bake; belt + braces).
    run(
        "systemctl",
        &[
            "disable",
            "--now",
            "apt-daily-upgrade.timer",
            "apt-daily.timer",
        ],
    );

    // rp_filter drops the rack's asymmetric / unnumbered transit traffic.
    sysctl("net.ipv4.conf.all.rp_filter", "0");
    sysctl("net.ipv4.conf.default.rp_filter", "0");

    apply_static_external();
    nat_rack_egress();
    apply_static_edge_ip();
    apply_frr()?;

    note("router bring-up complete");
    Ok(())
}

/// If a static customer-edge address is staged (voxel `[topology].ce_external_ip`,
/// written only into ce's cargo-bay), add it as a SECONDARY address on the uplink.
/// DHCP keeps the primary address + default route (egress/NTP), so this is purely
/// an extra fixed address that gives the host route to the rack a STABLE nexthop -
/// no churn across launches, nothing to chase over the serial console. No-op on
/// the cr* routers (only ce's cargo-bay carries the file). The prefix is taken
/// from the uplink's current DHCP address so the secondary lands on the same LAN.
fn apply_static_edge_ip() {
    let ip = match fs::read_to_string("/opt/cargo-bay/ce-external-ip") {
        Ok(s) => s.trim().to_string(),
        Err(_) => return, // not configured / not the ce node
    };
    if ip.is_empty() {
        return;
    }
    let Some(ifc) = uplink_iface() else {
        warn("static edge IP: no uplink found");
        return;
    };
    let cur = capture("ip", &["-o", "-4", "addr", "show", "dev", &ifc]).unwrap_or_default();
    if cur
        .split_whitespace()
        .any(|t| t == ip || t.starts_with(&format!("{ip}/")))
    {
        note(format!("static edge IP {ip} already on {ifc}"));
        return;
    }
    // Prefix length from the uplink's DHCP address (token after "inet"), default /22.
    let prefix = cur
        .split_whitespace()
        .skip_while(|t| *t != "inet")
        .nth(1)
        .and_then(|cidr| cidr.split('/').nth(1))
        .unwrap_or("22");
    let cidr = format!("{ip}/{prefix}");
    run("ip", &["addr", "add", &cidr, "dev", &ifc]);
    note(format!("static edge IP {cidr} added on {ifc}"));
}

fn sysctl(key: &str, val: &str) {
    run("sysctl", &["-w", &format!("{key}={val}")]);
}

/// NAT rack-sourced traffic out this node's host-LAN uplink (the interface
/// carrying its own default route), so the boundary NTP zone can reach its
/// upstream. Makes every router a valid egress regardless of NIC naming—wait
/// for the DHCP default to appear first.
fn nat_rack_egress() {
    match uplink_iface() {
        Some(ifc) => {
            let present = run_quiet(
                "iptables",
                &[
                    "-t",
                    "nat",
                    "-C",
                    "POSTROUTING",
                    "-o",
                    &ifc,
                    "-j",
                    "MASQUERADE",
                ],
            );
            if !present {
                run(
                    "iptables",
                    &[
                        "-t",
                        "nat",
                        "-A",
                        "POSTROUTING",
                        "-o",
                        &ifc,
                        "-j",
                        "MASQUERADE",
                    ],
                );
            }
            note(format!("NAT rack egress via {ifc}"));
        }
        None => warn("no default-route uplink found; rack egress/NTP may fail"),
    }
}

/// Apply the voxel-managed static external address (isolated mode) before any
/// downstream step consults the uplink. This means bringing the staged
/// `iface` up, adding the address (mirroring `apply_static_edge_ip`), replacing
/// the default route via `gateway`, and writing `/etc/resolv.conf` from `dns`.
///
/// No-op in `lan` mode (no file staged).
fn apply_static_external() {
    let Some(ExternalNet {
        ip_cidr,
        gateway,
        dns,
        iface,
    }) = read_external_net()
    else {
        return;
    };
    let Some(ifc) = iface else {
        warn("external-net staged without an iface line; router bring-up needs it");
        return;
    };
    run("ip", &["link", "set", &ifc, "up"]);
    let cur = capture("ip", &["-o", "-4", "addr", "show", "dev", &ifc]).unwrap_or_default();
    let already = cur.split_whitespace().any(|t| t == ip_cidr);
    if already {
        note(format!("static external {ip_cidr} already on {ifc}"));
    } else {
        run("ip", &["addr", "add", &ip_cidr, "dev", &ifc]);
    }
    run(
        "ip",
        &["route", "replace", "default", "via", &gateway, "dev", &ifc],
    );

    let resolv: String = dns.iter().map(|s| format!("nameserver {s}\n")).collect();
    if !resolv.is_empty() {
        // Replace the systemd-resolved symlink with a static file so our
        // nameservers stick (isolated mode has no DHCP to populate resolved).
        let _ = fs::remove_file("/etc/resolv.conf");
        if let Err(e) = fs::write("/etc/resolv.conf", resolv) {
            warn(format!("resolv.conf: {e}"));
        }
    }
    note(format!("static external {ip_cidr} on {ifc} (gw {gateway})"));
}

fn uplink_iface() -> Option<String> {
    if let Ok(v) = std::env::var("UPSTREAM_IFACE") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    // Isolated mode dictates the uplink up front (no DHCP to poll for).
    //
    // We handle it before the `lan`-mode default-route poll.
    if let Some(ext) = read_external_net() {
        if let Some(ifc) = ext.iface {
            return Some(ifc);
        }
    }
    for _ in 0..30 {
        if let Some(line) = capture("ip", &["-o", "-4", "route", "show", "default"]) {
            // "default via <gw> dev <iface> ...", so the iface is whitespace field 5.
            if let Some(dev) = line.split_whitespace().nth(4) {
                if !dev.is_empty() {
                    return Some(dev.to_string());
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    None
}

fn apply_frr() -> Result<()> {
    let src = "/opt/cargo-bay/frr.conf";
    if !Path::new(src).exists() {
        return Err(anyhow!("{src} not staged"));
    }
    fs::copy(src, "/etc/frr/frr.conf").context("apply frr.conf")?;
    run("systemctl", &["restart", "frr"]);
    Ok(())
}
