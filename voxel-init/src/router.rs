//! Router/edge bring-up - replaces `router-launch.sh`. Runs in the voxel-frr
//! debian guest. FRR (bgpd + bfdd) is pre-installed; this applies the generated
//! `frr.conf` (unnumbered eBGP or static, per router_mode) and NATs rack egress
//! out to the host LAN (the RSS time-sync path - the boundary NTP zone must reach
//! its upstream).

use crate::sys::{
    ExternalNet, capture, note, read_external_net, replace_in_file, run,
    run_quiet, warn,
};
use anyhow::{Context, Result, bail};
use camino::Utf8Path;
use indoc::formatdoc;
use std::fs;
use std::time::Duration;

pub fn bring_up() -> Result<()> {
    setup_ssh();

    // Give the host-LAN uplink a UNIQUE DHCP lease before NAT depends on it.
    ensure_unique_uplink_lease();

    // Forwarding is baked, but re-assert + stop lab-net RAs from clobbering us.
    sysctl("net.ipv4.ip_forward", "1");
    sysctl("net.ipv6.conf.all.forwarding", "1");
    sysctl("net.ipv6.conf.all.accept_ra", "0");

    // apt-daily timers can wipe FRR state (disabled at bake; belt + braces).
    run(
        "systemctl",
        &["disable", "--now", "apt-daily-upgrade.timer", "apt-daily.timer"],
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

/// SSH convenience for `voxel host login <router>`, the router-role counterpart
/// of the gimlet agent's `setup_ssh`. `openssh-server` is already in the image
/// (install-frr.sh), so this only appends any staged operator key and relaxes
/// sshd_config: voxel authenticates as root with the rack's empty password, and
/// Debian's stock `PermitRootLogin prohibit-password` refuses that.
fn setup_ssh() {
    let authorized = "/opt/cargo-bay/root_authorized_keys";
    if Utf8Path::new(authorized).exists() {
        let _ = fs::create_dir_all("/root/.ssh");
        if let Ok(keys) = fs::read(authorized) {
            use std::io::Write;
            match fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/root/.ssh/authorized_keys")
            {
                Ok(mut f) => {
                    if let Err(e) = f.write_all(&keys) {
                        warn(format!("authorized_keys: {e}"));
                    }
                }
                Err(e) => warn(format!("authorized_keys: {e}")),
            }
        }
    }

    // Debian's stock sshd_config keeps PasswordAuthentication yes but defaults
    // PermitRootLogin to prohibit-password. Flip it so serial-first debugging
    // (blank root password) still lets you in over SSH.
    replace_in_file(
        "/etc/ssh/sshd_config",
        &[
            ("#PasswordAuthentication yes", "PasswordAuthentication yes"),
            ("#PermitEmptyPasswords no", "PermitEmptyPasswords yes"),
            ("#PermitRootLogin prohibit-password", "PermitRootLogin yes"),
            ("PermitRootLogin prohibit-password", "PermitRootLogin yes"),
        ],
    );
    run("systemctl", &["enable", "--now", "ssh"]);
    run("systemctl", &["restart", "ssh"]);
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
    let cur = capture("ip", &["-o", "-4", "addr", "show", "dev", &ifc])
        .unwrap_or_default();
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

/// Every router boots from the same frr image with the same baked machine-id,
/// so systemd-networkd derives an identical DHCP client-id (DUID) and the
/// upstream server leases them all the SAME host-LAN IP. NAT return traffic then
/// races to whichever router owns that IP's ARP entry, starving the others' rack
/// egress: RSS time-sync in static mode took 18-28min or stalled. Pin the
/// uplink's DHCP client-id to its (always-unique) MAC and re-DHCP, so each router
/// gets a distinct lease. Scoped to the uplink interface only, leaving the
/// FRR-managed transit links untouched.
fn ensure_unique_uplink_lease() {
    // Isolated mode stages a static address and runs no DHCP server, so there
    // is no lease to dedupe. Keep networkd's DHCP client off the uplink that
    // `apply_static_external` configures by hand.
    if read_external_net().is_some() {
        note(
            "isolated mode: static external address, skipping DHCP lease pinning",
        );
        return;
    }
    let Some(ifc) = uplink_iface() else {
        warn("unique-lease: no host-LAN uplink found; skipping");
        return;
    };
    let cfg = formatdoc! {"
        [Match]
        Name={ifc}

        [Network]
        DHCP=yes

        [DHCPv4]
        ClientIdentifier=mac
        RouteMetric=100
    "};
    if let Err(e) =
        fs::write("/etc/systemd/network/00-voxel-uplink.network", &cfg)
    {
        warn(format!("unique-lease: write networkd config: {e}"));
        return;
    }
    run("systemctl", &["restart", "systemd-networkd"]);
    note(format!(
        "pinned {ifc} DHCP client-id to MAC; re-DHCPing host-LAN uplink"
    ));
}

fn sysctl(key: &str, val: &str) {
    run("sysctl", &["-w", &format!("{key}={val}")]);
}

/// NAT rack egress out this node's host-LAN uplink (the interface carrying its
/// own default route) so the boundary NTP zone can reach its internet upstream.
/// Excludes the directly-connected customer LAN: traffic there is the racks'
/// external-service replies (Nexus/DNS/console) sourced from public service IPs,
/// which must reach the host unchanged. Masquerading those rewrites the source to
/// this router's uplink IP so the host drops the reply. Waits for the DHCP
/// default to appear first.
fn nat_rack_egress() {
    match uplink_iface() {
        Some(ifc) => {
            // Skip NAT for the connected customer subnet (the external-service
            // reply path) before masquerading the internet-bound rest (NTP).
            if let Some(subnet) = uplink_subnet(&ifc)
                && !run_quiet(
                    "iptables",
                    &[
                        "-t",
                        "nat",
                        "-C",
                        "POSTROUTING",
                        "-o",
                        &ifc,
                        "-d",
                        &subnet,
                        "-j",
                        "RETURN",
                    ],
                )
            {
                run(
                    "iptables",
                    &[
                        "-t",
                        "nat",
                        "-I",
                        "POSTROUTING",
                        "1",
                        "-o",
                        &ifc,
                        "-d",
                        &subnet,
                        "-j",
                        "RETURN",
                    ],
                );
            }
            if !run_quiet(
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
            ) {
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
    let Some(ExternalNet { ip_cidr, gateway, dns, iface }) =
        read_external_net()
    else {
        return;
    };
    let Some(ifc) = iface else {
        warn(
            "external-net staged without an iface line; router bring-up needs it",
        );
        return;
    };

    if !link_exists(&ifc) {
        warn(format!(
            "external-net names {ifc}, which this router does not have; present links: {}",
            link_names().join(" ")
        ));
        return;
    }
    run("ip", &["link", "set", &ifc, "up"]);
    let cur = capture("ip", &["-o", "-4", "addr", "show", "dev", &ifc])
        .unwrap_or_default();
    let already = cur.split_whitespace().any(|t| t == ip_cidr);
    if already {
        note(format!("static external {ip_cidr} already on {ifc}"));
    } else {
        run("ip", &["addr", "add", &ip_cidr, "dev", &ifc]);
    }
    run("ip", &["route", "replace", "default", "via", &gateway, "dev", &ifc]);

    let resolv: String =
        dns.iter().map(|s| format!("nameserver {s}\n")).collect();
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

/// Whether the kernel has `ifc`.
///
/// The staged external name is derived from falcon's link-creation order
/// (`VoxelConfig::router_ext_iface`), so a change to that order surfaces as a device
/// this router does not have.
fn link_exists(ifc: &str) -> bool {
    capture("ip", &["-o", "link", "show", "dev", ifc]).is_some()
}

/// Every link name the kernel reports, for naming what is present when a staged
/// interface is not.
fn link_names() -> Vec<String> {
    capture("ip", &["-o", "link", "show"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(|n| n.trim_end_matches(':').to_string())
        .collect()
}

fn uplink_iface() -> Option<String> {
    if let Ok(v) = std::env::var("UPSTREAM_IFACE")
        && !v.is_empty()
    {
        return Some(v);
    }
    // Isolated mode dictates the uplink up front (no DHCP to poll for).
    //
    // We handle it before the `lan`-mode default-route poll, yielding nothing when
    // the device is absent: a NAT rule against it would never match, and
    // `apply_static_external` has already reported it.
    if let Some(ext) = read_external_net()
        && let Some(ifc) = ext.iface
    {
        return link_exists(&ifc).then_some(ifc);
    }
    for _ in 0..30 {
        if let Some(line) =
            capture("ip", &["-o", "-4", "route", "show", "default"])
        {
            // "default via <gw> dev <iface> ...", so the iface is whitespace field 5.
            if let Some(dev) = line.split_whitespace().nth(4)
                && !dev.is_empty()
            {
                return Some(dev.to_string());
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    None
}

/// The directly-connected IPv4 subnet on `ifc` (its kernel scope-link route,
/// e.g. "192.168.68.0/24"), the customer LAN the host reaches the rack from.
/// None if it can't be read.
fn uplink_subnet(ifc: &str) -> Option<String> {
    let line = capture(
        "ip",
        &["-o", "-4", "route", "show", "dev", ifc, "scope", "link"],
    )?;
    let cidr = line.split_whitespace().next()?;
    (cidr.contains('/') && cidr.contains('.')).then(|| cidr.to_string())
}

fn apply_frr() -> Result<()> {
    let src = "/opt/cargo-bay/frr.conf";
    if !Utf8Path::new(src).exists() {
        bail!("{src} not staged");
    }
    fs::copy(src, "/etc/frr/frr.conf").context("apply frr.conf")?;
    run("systemctl", &["restart", "frr"]);
    Ok(())
}
