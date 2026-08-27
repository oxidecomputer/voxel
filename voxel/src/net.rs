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

/// The switch zone's root as seen from the sled global zone - prepend it to an
/// in-zone path to reach the same file from the GZ (e.g. `{SWITCH_ZONE_ROOT}{p}`).
/// Single source for the handful of GZ-rooted switch-zone paths voxel touches.
pub(crate) const SWITCH_ZONE_ROOT: &str = "/zone/oxz_switch/root";

/// Absolute path to `route`. Not all invoking shells carry /usr/sbin on PATH
/// (commtest re-executes voxel under a fresh login), so spawn it absolutely.
pub(crate) const ROUTE: &str = "/usr/sbin/route";

/// The `zlogin` invocation prefix for the switch zone. Use [`zlogin`] to build a
/// full command; this bare form is for the interactive login (no command).
pub(crate) const ZLOGIN: &str = "zlogin oxz_switch";

/// Wrap `cmd` to run inside the switch zone: `zlogin oxz_switch <cmd>`. Single
/// source for the ~dozen switch-zone command sites (each still adds its own
/// redirections / quoting around the result).
pub(crate) fn zlogin(cmd: &str) -> String {
    format!("{ZLOGIN} {cmd}")
}

/// Soft bound on a serial-console resolution. The exec itself completes in a
/// few seconds, so blowing this means the console is slow or wedged.
/// [`serial_bounded`] warns here and keeps waiting rather than cancelling,
/// because cancelling the exec is what wedges the console.
pub(crate) const SERIAL_RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

/// Hard bound on a serial-console resolution. Giving up here abandons the exec
/// mid-flight, which can wedge the console, but a console this far past the
/// few-second norm is already unusable.
pub(crate) const SERIAL_RESOLVE_HARD_TIMEOUT: Duration =
    Duration::from_secs(60);

/// Run a serial-console exec under the two-stage deadline. Cancelling an
/// in-flight falcon exec leaves the console wedged for every later exec (see
/// [`resolve_external_ip`]), so a slow exec is not cancelled at
/// [`SERIAL_RESOLVE_TIMEOUT`]. It gets a warning and keeps running to
/// [`SERIAL_RESOLVE_HARD_TIMEOUT`], where only a console that is already
/// unusable is abandoned. `what` names the operation in both messages.
///
/// The hard deadline is a deliberate trade-off: it still drops the exec
/// mid-flight, and truly never cancelling would need a detached exec that
/// falcon's serial API does not offer. These execs answer in a few seconds
/// on a healthy console, so 60s of silence means the console is already
/// wedged and there is nothing left for cancellation to break.
///
/// # Errors
///
/// Fails when the exec itself fails, or with a timeout error past the hard
/// deadline.
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

/// Like [`serial_bounded`], but never waits past `deadline`. Retry loops use
/// this so one slow attempt cannot stretch their overall window: the hard
/// deadline shrinks to the window's remainder. Abandoning at the window's
/// edge carries the same wedge risk as the hard deadline, and these callers
/// stop using the console once the window closes anyway.
///
/// # Errors
///
/// As [`serial_bounded`], with the timeout landing at `deadline` when that
/// comes first.
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

/// Shared two-stage implementation: warn at `soft`, abandon at `hard`.
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

/// Resolve a node's external IPv4 without entering the guest when possible.
/// Isolated mode numbers every node deterministically
/// ([`VoxelConfig::static_external_ips`]), so we return the staged address
/// directly. The fallback, [`node_external_ip`], execs over the falcon serial
/// console, which wedges permanently if a prior exec was cancelled mid-flight
/// (see [`ssh_output`]). Prefer this resolver wherever the config and node
/// name are in hand.
///
/// [`VoxelConfig::static_external_ips`]: voxel_config::VoxelConfig::static_external_ips
pub(crate) async fn resolve_external_ip(
    cfg: &voxel_config::VoxelConfig,
    d: &Runner,
    node: &str,
    n: NodeRef,
    is_router: bool,
) -> anyhow::Result<String> {
    if cfg.external.static_addressing()
        && let Some((_, ip)) =
            cfg.static_external_ips().into_iter().find(|(name, _)| name == node)
    {
        return Ok(ip);
    }
    node_external_ip(d, n, is_router).await
}

/// ce's stable nexthop, when one is known without touching the guest. An
/// explicit `[topology].ce_external_ip` wins, otherwise static addressing's
/// numbering supplies it.
pub(crate) fn ce_static_ip(cfg: &voxel_config::VoxelConfig) -> Option<String> {
    if let Some(ip) = &cfg.topology.ce_external_ip {
        return Some(ip.clone());
    }
    if !cfg.external.static_addressing() {
        return None;
    }
    cfg.static_external_ips()
        .into_iter()
        .find_map(|(name, ip)| (name == "ce").then_some(ip))
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

/// Materialize the SSH_ASKPASS helper that supplies the rack's empty root password
/// (a script that prints a blank line), returning its path. `None` if it can't be
/// written / made executable. Shared by `ssh_exec` and `scp_to`.
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

pub(crate) fn ssh_capture(ip: &str, remote: &str) -> Option<String> {
    let out = ssh_exec(ip, remote)?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// How a remote command could fail: the ssh transport never reached the node,
/// or the node ran the command and it exited non-zero.
///
/// This split lets callers take an "unreachable node" path (a destroyed rack)
/// without also swallowing real command failures on a live one.
#[derive(Debug)]
pub(crate) enum SshFailure {
    /// ssh could not connect or authenticate (exit 255), so the node itself
    /// is unreachable.
    Unreachable,
    /// The command ran remotely and failed, or ssh could not be run locally;
    /// holds the failure text.
    Failed(String),
}

/// Like [`ssh_capture`], but keeps the failure mode instead of folding every
/// failure into `None`.
pub(crate) fn ssh_try_capture(
    ip: &str,
    remote: &str,
) -> Result<String, SshFailure> {
    let Some(out) = ssh_exec(ip, remote) else {
        return Err(SshFailure::Failed("ssh could not be run locally".into()));
    };
    if out.status.code() == Some(255) {
        return Err(SshFailure::Unreachable);
    }
    if !out.status.success() {
        return Err(SshFailure::Failed(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
/// SSH_ASKPASS) and return its raw `Output`. Shared by `ssh_capture` (gates
/// on exit status) and `ssh_output` (keeps output regardless of status).
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

/// A routing-table entry as `netstat -rn -f inet` prints it, reduced to the
/// destination, gateway, and flags columns.
pub(crate) struct RouteEntry {
    pub dest: String,
    pub gateway: String,
    pub flags: String,
}

/// The IPv4 routing table, one entry per line with at least a destination and
/// gateway column. Headers and separators come through as unparseable
/// destinations, so consumers matching on an address never see them.
pub(crate) fn route_entries() -> Vec<RouteEntry> {
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
            Some(RouteEntry {
                dest: it.next()?.to_string(),
                gateway: it.next()?.to_string(),
                flags: it.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// The gateways currently routing `dest` (an IPv4 network address like
/// `198.51.100.0`). Used to purge every stale route for a prefix - dead-ce
/// gateways from prior launches pile up otherwise.
pub(crate) fn route_gateways(dest: &str) -> Vec<String> {
    route_entries()
        .into_iter()
        .filter_map(|e| (e.dest == dest).then_some(e.gateway))
        .collect()
}

/// (Re)point the host route for the rack's external network (`prefix`) at ce's
/// current external IP. ce's host-facing NIC gets a fresh random MAC, and thus
/// a fresh DHCP IP, every launch, so any static route goes stale.
///
/// Discovering it here keeps the external services reachable without manual hunting.
/// The route is keyed by `prefix`, so racks with distinct external prefixes don't
/// collide. With `apply == false` it just prints the command.
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
