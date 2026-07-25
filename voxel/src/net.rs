//! Host-LAN networking: discover a node's external IPv4 and (re)point the host
//! route at the rack's external network.

use anyhow::anyhow;
use libfalcon::{NodeRef, Runner};
use slog::{info, warn};
use std::time::Duration;

use crate::rss::strip_ansi;

/// The switch zone's root as seen from the sled global zone - prepend it to an
/// in-zone path to reach the same file from the GZ (e.g. `{SWITCH_ZONE_ROOT}{p}`).
/// Single source for the handful of GZ-rooted switch-zone paths voxel touches.
pub(crate) const SWITCH_ZONE_ROOT: &str = "/zone/oxz_switch/root";

/// The `zlogin` invocation prefix for the switch zone. Use [`zlogin`] to build a
/// full command; this bare form is for the interactive login (no command).
pub(crate) const ZLOGIN: &str = "zlogin oxz_switch";

/// Wrap `cmd` to run inside the switch zone: `zlogin oxz_switch <cmd>`. Single
/// source for the ~dozen switch-zone command sites (each still adds its own
/// redirections / quoting around the result).
pub(crate) fn zlogin(cmd: &str) -> String {
    format!("{ZLOGIN} {cmd}")
}

/// A node's external (host-LAN) IPv4 - the address `voxel route` points at and
/// `voxel host`/`tp` SSH to. Every node's only non-loopback IPv4 is its host-LAN
/// DHCP lease (the underlay/cr links are IPv6), so we just take the first one.
/// Routers (Debian) report addresses via `ip`; sleds (Helios) via `ipadm`.
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
    let raw =
        d.exec(n, cmd).await.map_err(|e| anyhow!("read external IP: {e}"))?;
    let out = strip_ansi(&raw);
    out.split_whitespace()
        .filter_map(|t| t.split('/').next()) // drop any CIDR suffix
        .find(|t| {
            t.split('.').count() == 4
                && t.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                && !t.starts_with("127.")
        })
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no external IPv4 found (got {out:?})"))
}

/// Run `ssh root@<ip> <remote>` non-interactively and capture stdout, using the
/// rack's empty root password (`setup_ssh` enables `PermitEmptyPasswords`). This
/// is how [`crate::rss::watch_rss`] polls the bootstrap-agent: the serial console
/// wedges under RSS load - a stalled/cancelled exec leaves a shell on the
/// single-user console and poisons every later poll - but SSH to the node's LAN
/// IP is unaffected. Returns None on any failure (ssh error / non-zero exit); the
/// caller just retries. Bounded by ssh's own connect + keepalive timeouts so a
/// poll can't hang.
/// The ssh options shared by every voxel ssh invocation. The rack is re-created
/// constantly, so host-key checking is off and known-hosts is ephemeral; this is
/// the pilot/captain access pattern.
pub(crate) const EPHEMERAL_HOST_OPTS: &[&str] = &[
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "LogLevel=ERROR",
];

/// The empty-root-password auth options shared by every voxel ssh/scp invocation
/// (force password auth, one prompt, fail fast). ssh adds keepalive options on top.
const PASSWORD_AUTH_OPTS: &[&str] = &[
    "-o",
    "PreferredAuthentications=password",
    "-o",
    "PubkeyAuthentication=no",
    "-o",
    "NumberOfPasswordPrompts=1",
    "-o",
    "ConnectTimeout=8",
];

/// Materialize the SSH_ASKPASS helper that supplies the rack's empty root password
/// (a script that prints a blank line), returning its path. `None` if it can't be
/// written / made executable. Shared by [`ssh_exec`] and [`scp_to`].
fn ensure_askpass() -> Option<std::path::PathBuf> {
    let askpass = std::env::temp_dir().join("voxel-empty-askpass.sh");
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

pub(crate) fn ssh_capture(ip: &str, remote: &str) -> Option<String> {
    let out = ssh_exec(ip, remote)?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Like [`ssh_capture`], but returns the remote command's combined output even
/// when it exits non-zero - for callers (e.g. `sp exec`) that want the remote
/// tool's OWN error text (faux-mgs prints `Error: ...`, which the caller folds
/// into stdout via `2>&1`) instead of a generic "is the rack up?". Returns None
/// only when ssh itself couldn't run or couldn't connect/authenticate (exit 255),
/// i.e. the node really is unreachable - a non-255 exit means the command ran and
/// its output (success or error) is meaningful.
pub(crate) fn ssh_output(ip: &str, remote: &str) -> Option<String> {
    let out = ssh_exec(ip, remote)?;
    if out.status.code() == Some(255) {
        return None; // ssh transport failure (connect/auth), not a remote error
    }
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(s)
}

/// Run a non-interactive ssh command (empty root password via a forced
/// SSH_ASKPASS) and return its raw [`Output`]. Shared by [`ssh_capture`] (gates
/// on exit status) and [`ssh_output`] (keeps output regardless of status).
fn ssh_exec(ip: &str, remote: &str) -> Option<std::process::Output> {
    // ssh needs a non-interactive way to supply the (empty) password: point
    // SSH_ASKPASS at a script that prints an empty line, and force its use.
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

/// `scp <local> root@<ip>:<remote>` non-interactively (empty root password, same
/// pattern as [`ssh_capture`]). Returns whether it succeeded. Used to deliver
/// `faux-mgs` into a switch zone for the `sp` operator commands.
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

/// `scp root@<ip>:<remote> <local>` - the reverse of [`scp_to`], to pull an
/// artifact (e.g. an sp-emu crash dump) out of the switch zone onto the host.
pub(crate) fn scp_from(ip: &str, remote: &str, local: &str) -> bool {
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
        .arg(format!("root@{ip}:{remote}"))
        .arg(local)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Confirm a rack's external network is actually reachable end-to-end after the
/// host route is set - a route in the table isn't the same as a converged
/// transit. Probes the rack's external DNS (a `dig` SOA query, UDP/53) from the
/// host and waits, bounded, until it answers. With the shared transit, the second
/// rack joining can briefly flap the first rack's path while BGP reconverges; this
/// waits that out *here* instead of letting it surface to the operator as a dead
/// DNS. Best-effort: logs the outcome, never fails the launch. No-op (with a note)
/// if `dig` isn't installed.
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
        "{label}: external network not reachable after ~{}s (dns {dns_ip}) - the rack is up but \
         its external path may still be converging; retry `voxel route` or `dig {dns_zone} SOA @{dns_ip}`",
        ATTEMPTS * SPACING.as_secs() as u32
    );
}

/// `dig <zone> SOA @<dns_ip>`: `Some(true)` if the server answered, `Some(false)`
/// if it didn't (unreachable / timeout), `None` if `dig` isn't installed. The SOA
/// of the external zone is authoritative, so a positive answer needs no silo
/// knowledge - it just proves the rack's external DNS is reachable.
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

/// (Re)point the host route for the rack's external network (`prefix`) at ce's
/// current external IP. ce's host-facing NIC gets a fresh random MAC - and thus
/// a fresh DHCP IP - every launch, so any static route goes stale; discovering
/// it here keeps the external services reachable without a manual hunt. The
/// route is keyed by `prefix`, so racks with distinct external prefixes don't
/// collide. With `apply == false` it just prints the command.
/// The gateways currently routing `dest` (an IPv4 network address like
/// `198.51.100.0`), read from `netstat -rn -f inet`. Used to purge every stale
/// route for a prefix - dead-ce gateways from prior launches pile up otherwise.
fn route_gateways(dest: &str) -> Vec<String> {
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

pub(crate) async fn set_external_route(
    d: &Runner,
    ce: NodeRef,
    prefix: &str,
    apply: bool,
    static_ip: Option<&str>,
) -> anyhow::Result<()> {
    // A configured static ce address (`[topology].ce_external_ip`) is a stable
    // nexthop: use it directly and skip the slow, volatile serial-console lease
    // lookup. Otherwise read ce's DHCP lease as before.
    let ip = match static_ip {
        Some(s) => s.to_string(),
        None => node_external_ip(d, ce, true)
            .await
            .map_err(|e| anyhow!("ce: {e}"))?,
    };

    if !apply {
        info!(d.log, "external route (dry-run): route add {} {}", prefix, ip);
        return Ok(());
    }
    // Drop ALL stale routes for this prefix, then point it at the live ce.
    // Dead-ce gateways from prior launches accumulate, and a bare
    // `route delete <prefix>` doesn't reliably clear multiple same-prefix routes -
    // so first enumerate the live gateways for this prefix from the routing table
    // and delete each explicitly, then a few unqualified deletes to catch any
    // remainder. illumos `route`'s exit code is unreliable (non-zero even on a
    // successful add), so we key off printed output and re-read the table to
    // confirm the final state.
    let dest = prefix.split('/').next().unwrap_or(prefix);
    for gw in route_gateways(dest) {
        let _ = std::process::Command::new("route")
            .args(["delete", prefix, &gw])
            .output();
    }
    for _ in 0..8 {
        let out = std::process::Command::new("route")
            .args(["delete", prefix])
            .output();
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
    let add = std::process::Command::new("route")
        .args(["add", prefix, &ip])
        .output()
        .map_err(|e| anyhow!("route add: {e}"))?;
    let resolves = std::process::Command::new("route")
        .args(["-n", "get", dest])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&ip))
        .unwrap_or(false);
    if resolves {
        info!(d.log, "external route set: {} -> {} (ce)", prefix, ip);
    } else {
        warn!(
            d.log,
            "route {} -> {} not confirmed: {}{} - run: route add {} {}",
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
