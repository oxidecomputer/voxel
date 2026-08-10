//! Fail-closed router/edge bring-up for the Debian FRR guest.

use crate::sys::{
    ExternalNet, capture_required, note, read_external_net, run, run_required,
    run_status,
};
use anyhow::{Context, Result, anyhow};
use indoc::formatdoc;
use std::{fs, io::Write, path::Path, time::Duration};

const ROUTER_COMPLETE_SENTINEL: &str = "router bring-up complete";
const STATIC_EDGE_IP: &str = "/opt/cargo-bay/ce-external-ip";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExternalMode {
    Lan,
    Isolated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RouterStep {
    Ssh,
    UniqueDhcpLease,
    Forwarding,
    AptTimers,
    StaticExternal,
    Nat,
    StaticEdgeIp,
    Frr,
}

fn router_plan(mode: ExternalMode) -> Vec<RouterStep> {
    let mut plan = vec![RouterStep::Ssh];
    if mode == ExternalMode::Lan {
        plan.push(RouterStep::UniqueDhcpLease);
    }
    plan.extend([RouterStep::Forwarding, RouterStep::AptTimers]);
    if mode == ExternalMode::Isolated {
        plan.push(RouterStep::StaticExternal);
    }
    plan.extend([RouterStep::Nat, RouterStep::StaticEdgeIp, RouterStep::Frr]);
    plan
}

fn execute_plan(
    plan: &[RouterStep],
    mut execute: impl FnMut(RouterStep) -> Result<()>,
) -> Result<()> {
    for step in plan {
        execute(*step)?;
    }
    Ok(())
}

pub fn bring_up() -> Result<()> {
    let external = read_external_net()?;
    let mode = if external.is_some() {
        ExternalMode::Isolated
    } else {
        ExternalMode::Lan
    };
    let result = execute_plan(&router_plan(mode), |step| match step {
        RouterStep::Ssh => setup_ssh(),
        RouterStep::UniqueDhcpLease => ensure_unique_uplink_lease(),
        RouterStep::Forwarding => configure_forwarding(),
        RouterStep::AptTimers => {
            run(
                "systemctl",
                &[
                    "disable",
                    "--now",
                    "apt-daily-upgrade.timer",
                    "apt-daily.timer",
                ],
            );
            Ok(())
        }
        RouterStep::StaticExternal => apply_static_external(
            external.as_ref().expect("isolated plan has config"),
        ),
        RouterStep::Nat => nat_rack_egress(external.as_ref()),
        RouterStep::StaticEdgeIp => apply_static_edge_ip(external.as_ref()),
        RouterStep::Frr => apply_frr(),
    });
    finish_bring_up(result, |line| note(line))
}

fn finish_bring_up(
    result: Result<()>,
    mut emit: impl FnMut(&str),
) -> Result<()> {
    result?;
    emit(ROUTER_COMPLETE_SENTINEL);
    Ok(())
}

fn setup_ssh() -> Result<()> {
    let authorized = "/opt/cargo-bay/root_authorized_keys";
    match fs::read(authorized) {
        Ok(keys) => {
            fs::create_dir_all("/root/.ssh")
                .context("create root SSH directory")?;
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/root/.ssh/authorized_keys")
                .context("open root authorized_keys")?
                .write_all(&keys)
                .context("append root authorized_keys")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).context("read staged root authorized_keys");
        }
    }
    let path = "/etc/ssh/sshd_config";
    let mut config = fs::read_to_string(path).context("read sshd_config")?;
    for (from, to) in [
        ("#PasswordAuthentication yes", "PasswordAuthentication yes"),
        ("#PermitEmptyPasswords no", "PermitEmptyPasswords yes"),
        ("#PermitRootLogin prohibit-password", "PermitRootLogin yes"),
        ("PermitRootLogin prohibit-password", "PermitRootLogin yes"),
    ] {
        config = config.replace(from, to);
    }
    fs::write(path, config).context("write sshd_config")?;
    run_required("systemctl", &["enable", "--now", "ssh"])?;
    run_required("systemctl", &["restart", "ssh"]).context("restart SSH")
}

fn uplink_network_config(ifc: &str) -> String {
    formatdoc!(
        "[Match]\nName={ifc}\n\n[Network]\nDHCP=yes\n\n[DHCPv4]\nClientIdentifier=mac\nRouteMetric=100\n"
    )
}

fn ensure_unique_uplink_lease() -> Result<()> {
    let ifc = uplink_iface(None)?;
    fs::write(
        "/etc/systemd/network/00-voxel-uplink.network",
        uplink_network_config(&ifc),
    )
    .context("write unique uplink lease configuration")?;
    run_required("systemctl", &["restart", "systemd-networkd"])
}

fn configure_forwarding() -> Result<()> {
    for (key, value) in [
        ("net.ipv4.ip_forward", "1"),
        ("net.ipv6.conf.all.forwarding", "1"),
        ("net.ipv6.conf.all.accept_ra", "0"),
        ("net.ipv4.conf.all.rp_filter", "0"),
        ("net.ipv4.conf.default.rp_filter", "0"),
    ] {
        run_required("sysctl", &["-w", &format!("{key}={value}")])
            .with_context(|| format!("set {key}"))?;
    }
    Ok(())
}

fn apply_static_external(ext: &ExternalNet) -> Result<()> {
    let ifc = ext
        .iface
        .as_deref()
        .ok_or_else(|| anyhow!("external-net requires iface for router"))?;
    capture_required("ip", &["-o", "link", "show", "dev", ifc])
        .context("verify staged external interface")?;
    run_required("ip", &["link", "set", ifc, "up"])?;
    let addresses =
        capture_required("ip", &["-o", "-4", "addr", "show", "dev", ifc])?;
    if !addresses.split_whitespace().any(|token| token == ext.ip_cidr) {
        run_required("ip", &["addr", "add", &ext.ip_cidr, "dev", ifc])?;
    }
    run_required(
        "ip",
        &["route", "replace", "default", "via", &ext.gateway, "dev", ifc],
    )?;
    let resolv: String =
        ext.dns.iter().map(|dns| format!("nameserver {dns}\n")).collect();
    match fs::remove_file("/etc/resolv.conf") {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("remove resolv.conf"),
    }
    fs::write("/etc/resolv.conf", resolv).context("write isolated DNS")
}

fn select_uplink<'a>(
    override_ifc: Option<&'a str>,
    staged: Option<&'a str>,
    default: Option<&'a str>,
) -> Option<&'a str> {
    override_ifc.filter(|s| !s.is_empty()).or(staged).or(default)
}

fn uplink_iface(ext: Option<&ExternalNet>) -> Result<String> {
    if let Some(ifc) = select_uplink(
        std::env::var("UPSTREAM_IFACE").ok().as_deref(),
        ext.and_then(|e| e.iface.as_deref()),
        None,
    ) {
        capture_required("ip", &["-o", "link", "show", "dev", ifc])
            .context("verify selected uplink")?;
        return Ok(ifc.to_string());
    }
    for attempt in 1..=30 {
        let routes =
            capture_required("ip", &["-o", "-4", "route", "show", "default"])?;
        if let Some(ifc) = parse_default_uplink(&routes) {
            return Ok(ifc);
        }
        if attempt < 30 {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    Err(anyhow!("no default-route uplink found after 30 attempts"))
}

fn parse_default_uplink(routes: &str) -> Option<String> {
    routes.lines().find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        (fields.first() == Some(&"default"))
            .then(|| {
                fields
                    .windows(2)
                    .find(|p| p[0] == "dev")
                    .map(|p| p[1].to_string())
            })
            .flatten()
    })
}

fn uplink_subnet(ifc: &str) -> Result<String> {
    let routes = capture_required(
        "ip",
        &["-o", "-4", "route", "show", "dev", ifc, "scope", "link"],
    )?;
    routes
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .find(|cidr| cidr.contains('.') && cidr.contains('/'))
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no directly connected IPv4 subnet on {ifc}"))
}

fn nat_rules<'a>(ifc: &'a str, subnet: &'a str) -> [Vec<&'a str>; 2] {
    [
        vec![
            "-t",
            "nat",
            "POSTROUTING",
            "-o",
            ifc,
            "-d",
            subnet,
            "-j",
            "RETURN",
        ],
        vec!["-t", "nat", "POSTROUTING", "-o", ifc, "-j", "MASQUERADE"],
    ]
}

fn nat_rack_egress(ext: Option<&ExternalNet>) -> Result<()> {
    let ifc = uplink_iface(ext)?;
    let subnet = uplink_subnet(&ifc)?;
    for (index, rule) in nat_rules(&ifc, &subnet).iter().enumerate() {
        let mut check = rule.clone();
        check.insert(2, "-C");
        let status = run_status("iptables", &check)?.code();
        if iptables_check_needs_add(status)? {
            let mut add = rule.clone();
            add.insert(2, if index == 0 { "-I" } else { "-A" });
            if index == 0 {
                add.insert(4, "1");
            }
            run_required("iptables", &add)?;
            run_required("iptables", &check)?;
        }
    }
    note(format!("NAT rack egress via {ifc}"));
    Ok(())
}

fn iptables_check_needs_add(status: Option<i32>) -> Result<bool> {
    match status {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        Some(code) => Err(anyhow!("iptables check failed with status {code}")),
        None => Err(anyhow!("iptables check terminated without status")),
    }
}

fn apply_static_edge_ip(ext: Option<&ExternalNet>) -> Result<()> {
    let contents = match fs::read_to_string(STATIC_EDGE_IP) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("read staged static edge IP"),
    };
    let Some(ip) = staged_static_edge_ip(contents.as_deref())? else {
        return Ok(());
    };
    let ifc = uplink_iface(ext)?;
    let addresses =
        capture_required("ip", &["-o", "-4", "addr", "show", "dev", &ifc])?;
    if addresses
        .split_whitespace()
        .any(|t| t == ip || t.starts_with(&format!("{ip}/")))
    {
        return Ok(());
    }
    let prefix = match ext {
        Some(ext) => uplink_ipv4_prefix(&format!("inet {}", ext.ip_cidr))?,
        None => uplink_ipv4_prefix(&addresses)?,
    };
    run_required("ip", &["addr", "add", &format!("{ip}/{prefix}"), "dev", &ifc])
}

fn staged_static_edge_ip(contents: Option<&str>) -> Result<Option<String>> {
    contents
        .map(|s| {
            let ip = s.trim();
            if ip.is_empty() {
                Err(anyhow!("staged static edge IP is empty"))
            } else {
                Ok(ip.to_string())
            }
        })
        .transpose()
}

fn uplink_ipv4_prefix(addresses: &str) -> Result<u8> {
    for pair in addresses.split_whitespace().collect::<Vec<_>>().windows(2) {
        if pair[0] == "inet"
            && let Some((ip, prefix)) = pair[1].split_once('/')
            && ip.parse::<std::net::Ipv4Addr>().is_ok()
            && let Ok(prefix) = prefix.parse::<u8>()
            && prefix <= 32
        {
            return Ok(prefix);
        }
    }
    Err(anyhow!("uplink address output contains no valid IPv4 prefix"))
}

fn apply_frr() -> Result<()> {
    let src = "/opt/cargo-bay/frr.conf";
    if !Path::new(src).try_exists().context("check staged FRR config")? {
        return Err(anyhow!("{src} not staged"));
    }
    fs::copy(src, "/etc/frr/frr.conf").context("apply frr.conf")?;
    run_required("systemctl", &["restart", "frr"])
        .context("restart required FRR")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn isolated_static_precedes_nat() {
        let p = router_plan(ExternalMode::Isolated);
        assert!(
            p.iter().position(|s| *s == RouterStep::StaticExternal)
                < p.iter().position(|s| *s == RouterStep::Nat)
        );
        assert!(!p.contains(&RouterStep::UniqueDhcpLease));
    }
    #[test]
    fn lan_pins_client_id() {
        let p = router_plan(ExternalMode::Lan);
        assert_eq!(p[1], RouterStep::UniqueDhcpLease);
        assert!(uplink_network_config("e0").contains("ClientIdentifier=mac"));
    }
    #[test]
    fn staged_and_override_need_no_route() {
        assert_eq!(
            select_uplink(Some("override"), Some("staged"), None),
            Some("override")
        );
        assert_eq!(select_uplink(None, Some("staged"), None), Some("staged"));
    }
    #[test]
    fn return_precedes_masquerade() {
        let r = nat_rules("e0", "192.0.2.0/24");
        assert_eq!(r[0].last(), Some(&"RETURN"));
        assert_eq!(r[1].last(), Some(&"MASQUERADE"));
    }
    #[test]
    fn iptables_status_is_strict() {
        assert!(!iptables_check_needs_add(Some(0)).unwrap());
        assert!(iptables_check_needs_add(Some(1)).unwrap());
        assert!(iptables_check_needs_add(Some(2)).is_err());
        assert!(iptables_check_needs_add(None).is_err());
    }
    #[test]
    fn sentinel_only_after_success() {
        let mut out = vec![];
        assert!(
            finish_bring_up(Err(anyhow!("fail")), |s| out.push(s.to_string()))
                .is_err()
        );
        assert!(out.is_empty());
        finish_bring_up(Ok(()), |s| out.push(s.to_string())).unwrap();
        assert_eq!(out, [ROUTER_COMPLETE_SENTINEL]);
    }
    #[test]
    fn required_error_stops_plan() {
        let mut seen = vec![];
        let r = execute_plan(&router_plan(ExternalMode::Lan), |s| {
            seen.push(s);
            if s == RouterStep::Nat { Err(anyhow!("nat")) } else { Ok(()) }
        });
        assert!(r.is_err());
        assert_eq!(seen.last(), Some(&RouterStep::Nat));
        assert!(!seen.contains(&RouterStep::Frr));
    }
    #[test]
    fn parses_prefix_and_default() {
        assert_eq!(uplink_ipv4_prefix("inet 192.0.2.2/22").unwrap(), 22);
        assert_eq!(
            parse_default_uplink("default via 1.1.1.1 dev e0"),
            Some("e0".into())
        );
    }
}
