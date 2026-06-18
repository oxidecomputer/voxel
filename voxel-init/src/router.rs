//! Router/edge bring-up - replaces `router-launch.sh`. Runs in the voxel-frr
//! debian guest. FRR + bgpd are pre-installed; this applies the generated
//! unnumbered `frr.conf` and NATs rack egress out to the host LAN (the RSS
//! time-sync path - the boundary NTP zone must reach its upstream).

use crate::sys::{capture, note, run, run_quiet, warn};
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::Path;
use std::time::Duration;

pub fn bring_up() -> Result<()> {
    // Forwarding is baked, but re-assert + stop lab-net RAs from clobbering us.
    sysctl("net.ipv4.ip_forward", "1");
    sysctl("net.ipv6.conf.all.forwarding", "1");
    sysctl("net.ipv6.conf.all.accept_ra", "0");

    // apt-daily timers can wipe FRR state (disabled at bake; belt + braces).
    run("systemctl", &["disable", "--now", "apt-daily-upgrade.timer", "apt-daily.timer"]);

    // rp_filter drops the rack's asymmetric / unnumbered transit traffic.
    sysctl("net.ipv4.conf.all.rp_filter", "0");
    sysctl("net.ipv4.conf.default.rp_filter", "0");

    nat_rack_egress();
    apply_frr()?;

    note("router bring-up complete");
    Ok(())
}

fn sysctl(key: &str, val: &str) {
    run("sysctl", &["-w", &format!("{key}={val}")]);
}

/// NAT rack-sourced traffic out this node's host-LAN uplink (the interface
/// carrying its own default route), so the boundary NTP zone can reach its
/// upstream. Makes every router a valid egress regardless of NIC naming - wait
/// for the DHCP default to appear first.
fn nat_rack_egress() {
    match uplink_iface() {
        Some(ifc) => {
            let present = run_quiet(
                "iptables",
                &["-t", "nat", "-C", "POSTROUTING", "-o", &ifc, "-j", "MASQUERADE"],
            );
            if !present {
                run("iptables", &["-t", "nat", "-A", "POSTROUTING", "-o", &ifc, "-j", "MASQUERADE"]);
            }
            note(format!("NAT rack egress via {ifc}"));
        }
        None => warn("no default-route uplink found; rack egress/NTP may fail"),
    }
}

fn uplink_iface() -> Option<String> {
    if let Ok(v) = std::env::var("UPSTREAM_IFACE") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    for _ in 0..30 {
        if let Some(line) = capture("ip", &["-o", "-4", "route", "show", "default"]) {
            // "default via <gw> dev <iface> ..." - the iface is whitespace field 5.
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
