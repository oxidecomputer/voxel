// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Host-LAN networking: discover a node's external IPv4 and (re)point the host
//! route at the rack's external network.

use anyhow::Context;
use libfalcon::{NodeRef, Runner};
use slog::{info, warn};
use std::future::Future;
use std::time::{Duration, Instant};

use crate::rss::strip_ansi;

/// The switch zone's root as seen from the sled global zone.
pub(crate) const SWITCH_ZONE_ROOT: &str = "/zone/oxz_switch/root";

/// Absolute path to route: commtest re-executes voxel under a login shell
/// that lacks /usr/sbin on PATH.
pub(crate) const ROUTE: &str = "/usr/sbin/route";

/// Bare zlogin prefix for the switch zone, for the interactive login.
pub(crate) const ZLOGIN: &str = "zlogin oxz_switch";

/// Wrap cmd to run inside the switch zone.
pub(crate) fn zlogin(cmd: &str) -> String {
    format!("{ZLOGIN} {cmd}")
}

/// Soft bound on a serial-console exec: serial_bounded warns here and keeps
/// waiting, since cancelling the exec is what wedges the console.
pub(crate) const SERIAL_RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

/// Hard bound on a serial-console exec, past which it is abandoned.
pub(crate) const SERIAL_RESOLVE_HARD_TIMEOUT: Duration =
    Duration::from_secs(60);

/// Run a serial-console exec under the two-stage deadline: warn at the soft
/// bound, abandon at the hard bound. what names the operation in messages.
pub(crate) async fn serial_bounded<T>(
    what: &str,
    fut: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    serial_bounded_caps(
        what,
        SERIAL_RESOLVE_TIMEOUT,
        SERIAL_RESOLVE_HARD_TIMEOUT,
        fut,
    )
    .await
}

/// Like serial_bounded, but never waits past deadline, so one slow attempt
/// cannot stretch a retry loop's window.
pub(crate) async fn serial_bounded_within<T>(
    what: &str,
    deadline: Instant,
    fut: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    serial_bounded_caps(
        what,
        SERIAL_RESOLVE_TIMEOUT.min(remaining),
        SERIAL_RESOLVE_HARD_TIMEOUT.min(remaining),
        fut,
    )
    .await
}

/// Shared two-stage implementation: warn at soft, abandon at hard.
async fn serial_bounded_caps<T>(
    what: &str,
    soft: Duration,
    hard: Duration,
    fut: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::pin!(fut);
    if let Ok(res) = tokio::time::timeout(soft, &mut fut).await {
        return res;
    }
    if hard > soft {
        eprintln!(
            "[voxel] {what}: no answer from the serial console after {}s. Waiting up to {}s \
             rather than cancelling, since a cancelled exec wedges the console.",
            soft.as_secs(),
            hard.as_secs()
        );
        if let Ok(res) =
            tokio::time::timeout(hard.saturating_sub(soft), &mut fut).await
        {
            return res;
        }
    }
    anyhow::bail!(
        "{what}: serial console unresponsive after {}s",
        hard.as_secs()
    )
}

/// A node's external IPv4: the staged static address in isolated mode, else
/// node_external_ip over the serial console.
pub(crate) async fn resolve_external_ip(
    cfg: &voxel_config::VoxelConfig,
    d: &Runner,
    node: &str,
    n: NodeRef,
    is_router: bool,
) -> anyhow::Result<String> {
    if cfg.external.isolated()
        && let Some(ip) = static_external_ip(cfg, node)
    {
        return Ok(ip);
    }
    node_external_ip(d, n, is_router).await
}

/// A node's address in isolated mode's static numbering, None if it has none.
pub(crate) fn static_external_ip(
    cfg: &voxel_config::VoxelConfig,
    node: &str,
) -> Option<String> {
    cfg.static_external_ips()
        .into_iter()
        .find_map(|(name, ip)| (name == node).then_some(ip))
}

/// ce's nexthop when known without entering the guest: ce_external_ip, else
/// isolated mode's static address.
pub(crate) fn ce_static_ip(cfg: &voxel_config::VoxelConfig) -> Option<String> {
    if let Some(ip) = &cfg.topology.ce_external_ip {
        return Some(ip.clone());
    }
    if !cfg.external.isolated() {
        return None;
    }
    static_external_ip(cfg, "ce")
}

/// A node's host-LAN IPv4, its only non-loopback IPv4 address. Routers report
/// via ip, sleds via ipadm.
pub(crate) async fn node_external_ip(
    d: &Runner,
    n: NodeRef,
    is_router: bool,
) -> anyhow::Result<String> {
    let cmd = if is_router {
        "ip -4 -br addr show scope global 2>/dev/null"
    } else {
        "ipadm show-addr -p -o addr 2>/dev/null"
    };
    let raw = d.exec(n, cmd).await.context("read external IP")?;
    let out = strip_ansi(&raw);
    out.split_whitespace()
        .filter_map(|t| t.split('/').next()) // drop any CIDR suffix
        .find(|t| {
            t.split('.').count() == 4
                && t.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                && !t.starts_with("127.")
        })
        .map(str::to_string)
        .with_context(|| format!("no external IPv4 found (got {out:?})"))
}

/// ssh options for a rack that is re-created constantly: no host-key checking,
/// ephemeral known-hosts.
pub(crate) const EPHEMERAL_HOST_OPTS: &[&str] = &[
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "LogLevel=ERROR",
];

/// Empty-root-password auth options shared by every ssh and scp invocation.
pub(crate) const PASSWORD_AUTH_OPTS: &[&str] = &[
    "-o",
    "PreferredAuthentications=password",
    "-o",
    "PubkeyAuthentication=no",
    "-o",
    "NumberOfPasswordPrompts=1",
    "-o",
    "ConnectTimeout=8",
];

/// Write the SSH_ASKPASS helper that supplies the empty root password and
/// return its path, None if it cannot be written or made executable.
pub(crate) fn ensure_askpass() -> Option<camino::Utf8PathBuf> {
    let askpass = crate::util::temp_dir().join("voxel-empty-askpass.sh");
    if !askpass.exists() {
        std::fs::write(&askpass, "#!/bin/sh\necho\n").ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &askpass,
                std::fs::Permissions::from_mode(0o755),
            )
            .ok()?;
        }
    }
    Some(askpass)
}

/// Run ssh root@<ip> <remote> with the rack's empty root password and return
/// stdout, None on any failure. Unlike the serial console, ssh survives RSS load.
pub(crate) fn ssh_capture(ip: &str, remote: &str) -> Option<String> {
    let out = ssh_exec(ip, remote)?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Like ssh_capture, but returns combined output even on a non-zero exit, so
/// the remote tool's own error text comes through. None only on ssh exit 255.
pub(crate) fn ssh_output(ip: &str, remote: &str) -> Option<String> {
    let out = ssh_exec(ip, remote)?;
    if out.status.code() == Some(255) {
        return None; // exit 255 is an ssh transport failure, not a remote error
    }
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(s)
}

/// Run a non-interactive ssh command with the empty root password and return
/// its raw Output.
fn ssh_exec(ip: &str, remote: &str) -> Option<std::process::Output> {
    // SSH_ASKPASS supplies the empty password; force its use.
    let askpass = ensure_askpass()?;
    std::process::Command::new("ssh")
        .env("SSH_ASKPASS", &askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .stdin(std::process::Stdio::null())
        .args(EPHEMERAL_HOST_OPTS)
        .args(PASSWORD_AUTH_OPTS)
        .args(["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2"])
        .arg(format!("root@{ip}"))
        .arg(remote)
        .output()
        .ok()
}

/// scp local to root@<ip>:<remote> non-interactively; whether it succeeded.
pub(crate) fn scp_to(ip: &str, local: &str, remote: &str) -> bool {
    let askpass = match ensure_askpass() {
        Some(p) => p,
        None => return false,
    };
    std::process::Command::new("scp")
        .env("SSH_ASKPASS", &askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .stdin(std::process::Stdio::null())
        .args(EPHEMERAL_HOST_OPTS)
        .args(PASSWORD_AUTH_OPTS)
        .arg("-q") // no progress meter
        .arg(local)
        .arg(format!("root@{ip}:{remote}"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Wait, bounded, until the rack's external DNS answers a SOA query from the
/// host. Best effort: logs the outcome, never fails the launch.
pub(crate) fn wait_external_reachable(
    log: &slog::Logger,
    dns_ip: &str,
    dns_zone: &str,
    label: &str,
) {
    const ATTEMPTS: u32 = 30; // ~90s at 3s spacing
    const SPACING: Duration = Duration::from_secs(3);
    for attempt in 1..=ATTEMPTS {
        match dig_soa(dns_ip, dns_zone) {
            None => {
                info!(
                    log,
                    "{label}: skipping external reachability check (dig unavailable)"
                );
                return;
            }
            Some(true) => {
                info!(
                    log,
                    "{label}: external network reachable (dns {dns_ip})"
                );
                return;
            }
            Some(false) => {
                if attempt == 1 {
                    info!(
                        log,
                        "{label}: waiting for external network to converge (dns {dns_ip}) ..."
                    );
                }
                std::thread::sleep(SPACING);
            }
        }
    }
    warn!(
        log,
        "{label}: external network not reachable after ~{}s (dns {dns_ip}); the rack is up but \
         its external path may still be converging; retry `voxel route` or `dig {dns_zone} SOA @{dns_ip}`",
        ATTEMPTS * SPACING.as_secs() as u32
    );
}

/// dig <zone> SOA @<dns_ip>: Some(true) on an answer, Some(false) on none,
/// None if dig is not installed.
fn dig_soa(dns_ip: &str, zone: &str) -> Option<bool> {
    match std::process::Command::new("dig")
        .args([
            "+short",
            "+timeout=3",
            "+tries=1",
            &format!("@{dns_ip}"),
            zone,
            "SOA",
        ])
        .output()
    {
        Ok(o) => Some(
            o.status.success() && !o.stdout.iter().all(u8::is_ascii_whitespace),
        ),
        Err(_) => None,
    }
}

/// Gateways currently routing dest, from netstat -rn -f inet.
pub(crate) fn route_gateways(dest: &str) -> Vec<String> {
    let out = match std::process::Command::new("netstat")
        .args(["-rn", "-f", "inet"])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => return Vec::new(),
    };
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let d = it.next()?;
            let gw = it.next()?;
            (d == dest).then(|| gw.to_string())
        })
        .collect()
}

/// Point the host route for prefix at ce's current external IP, which changes
/// every launch. With apply false, only print the command.
pub(crate) async fn set_external_route(
    d: &Runner,
    ce: NodeRef,
    prefix: &str,
    apply: bool,
    static_ip: Option<&str>,
) -> anyhow::Result<()> {
    // A static ce address is a stable nexthop; otherwise read ce's DHCP lease.
    let ip = match static_ip {
        Some(s) => s.to_string(),
        None => serial_bounded(
            "ce: reading its DHCP lease",
            node_external_ip(d, ce, true),
        )
        .await
        .context("ce")?,
    };

    if !apply {
        info!(d.log, "external route (dry-run): route add {} {}", prefix, ip);
        return Ok(());
    }
    // Delete each stale gateway for the prefix explicitly, then sweep with bare
    // deletes; illumos route's exit code is unreliable, so re-read the table.
    let dest = prefix.split('/').next().unwrap_or(prefix);
    for gw in route_gateways(dest) {
        let _ = std::process::Command::new(ROUTE)
            .args(["delete", prefix, &gw])
            .output();
    }
    for _ in 0..8 {
        let out =
            std::process::Command::new(ROUTE).args(["delete", prefix]).output();
        let gone = match out {
            Ok(o) => {
                String::from_utf8_lossy(&o.stdout).contains("not in table")
            }
            Err(_) => true,
        };
        if gone {
            break;
        }
    }
    let add = std::process::Command::new(ROUTE)
        .args(["add", prefix, &ip])
        .output()
        .context("route add")?;
    let resolves = std::process::Command::new(ROUTE)
        .args(["-n", "get", dest])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&ip))
        .unwrap_or(false);
    if resolves {
        info!(d.log, "external route set: {} -> {} (ce)", prefix, ip);
    } else {
        warn!(
            d.log,
            "route {} -> {} not confirmed: {}{}; run: route add {} {}",
            prefix,
            ip,
            String::from_utf8_lossy(&add.stdout).trim(),
            String::from_utf8_lossy(&add.stderr).trim(),
            prefix,
            ip
        );
    }
    Ok(())
}
