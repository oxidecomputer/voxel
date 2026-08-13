//! Host-LAN networking: discover a node's external IPv4 and (re)point the host
//! route at the rack's external network.

use anyhow::{Context, bail};
use libfalcon::{NodeRef, Runner};
use slog::info;
use std::future::Future;
use std::io::Read;
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

/// Bound on a serial-console IP resolution. The exec itself completes in a few
/// seconds, so this only matters when the console is wedged, where callers
/// should fail fast instead of hanging.
pub(crate) const SERIAL_RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

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
    if cfg.external.isolated()
        && let Some((_, ip)) =
            cfg.static_external_ips().into_iter().find(|(name, _)| name == node)
    {
        return Ok(ip);
    }
    node_external_ip(d, n, is_router).await
}

/// ce's stable nexthop, when one is known without touching the guest. An
/// explicit `[topology].ce_external_ip` wins, otherwise isolated mode's static
/// numbering supplies it.
pub(crate) fn ce_static_ip(cfg: &voxel_config::VoxelConfig) -> Option<String> {
    if let Some(ip) = &cfg.topology.ce_external_ip {
        return Some(ip.clone());
    }
    if !cfg.external.isolated() {
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
    let raw = serial_exec_with_timeout(SERIAL_RESOLVE_TIMEOUT, async {
        d.exec(n, cmd).await.map_err(anyhow::Error::from)
    })
    .await?;
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

async fn serial_exec_with_timeout<F>(
    timeout: Duration,
    exec: F,
) -> anyhow::Result<String>
where
    F: Future<Output = anyhow::Result<String>>,
{
    tokio::time::timeout(timeout, exec)
        .await
        .with_context(|| {
            format!("read external IP timed out after {timeout:?}")
        })?
        .context("read external IP")
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
/// written / made executable. Shared by `ssh_exec` and `scp_to`.
fn ensure_askpass() -> Option<camino::Utf8PathBuf> {
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

/// Like [`ssh_capture`], but returns the remote command's combined output even
/// when it exits non-zero - for callers (e.g. `sp exec`) that want the remote
/// tool's OWN error text (faux-mgs prints `Error: ...`, which the caller folds
/// into stdout via `2>&1`) instead of a generic "is the rack up?". Returns None
/// only when ssh itself couldn't run or couldn't connect/authenticate (exit 255),
/// i.e. the node really is unreachable - a non-255 exit means the command ran and
/// its output (success or error) is meaningful.
pub(crate) fn ssh_output_timeout(
    ip: &str,
    remote: &str,
    timeout: Duration,
) -> Option<String> {
    command_output_timeout(ssh_command(ip, remote)?, timeout).and_then(|out| {
        if out.status.code() == Some(255) {
            None
        } else {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
    })
}

fn ssh_command(ip: &str, remote: &str) -> Option<std::process::Command> {
    let askpass = ensure_askpass()?;
    let mut command = std::process::Command::new("ssh");
    command
        .env("SSH_ASKPASS", &askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .stdin(std::process::Stdio::null())
        .args(EPHEMERAL_HOST_OPTS)
        .args(PASSWORD_AUTH_OPTS)
        .args(["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2"])
        .arg(format!("root@{ip}"))
        .arg(remote);
    Some(command)
}

pub(crate) fn command_output_timeout(
    mut command: std::process::Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::os::unix::process::CommandExt;
    command
        .process_group(0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().ok()?;
    let deadline = std::time::Instant::now() + timeout;
    let (Some(mut stdout), Some(mut stderr)) =
        (child.stdout.take(), child.stderr.take())
    else {
        kill_process_group(&child);
        let _ = child.wait();
        return None;
    };
    let stdout_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output).map(|_| output)
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Ok(None) | Err(_) => {
                kill_process_group(&child);
                let _ = child.wait();
                break None;
            }
        }
    };
    kill_process_group(&child);
    Some(std::process::Output {
        status: status?,
        stdout: stdout_reader.join().ok()?.ok()?,
        stderr: stderr_reader.join().ok()?.ok()?,
    })
}

fn kill_process_group(child: &std::process::Child) {
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
}

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
/// transit. Probes every configured external DNS server (a `dig` SOA query,
/// UDP/53) from the host and waits, bounded, until one answers. With the shared
/// transit, the second rack joining can briefly flap the first rack's path while
/// BGP reconverges; this waits that out *here*. Missing host prerequisites remain
/// fatal. The caller decides whether exhausting the bounded probe is fatal.
pub(crate) fn wait_external_reachable(
    log: &slog::Logger,
    dns_ips: &[String],
    dns_zone: &str,
    label: &str,
) -> anyhow::Result<bool> {
    const TIMEOUT: Duration = Duration::from_secs(90);
    const SPACING: Duration = Duration::from_secs(3);
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut first_attempt = true;
    loop {
        if let Some(dns_ip) =
            probe_external_dns(dns_ips, |dns_ip| dig_soa(dns_ip, dns_zone))?
        {
            info!(log, "{label}: external network reachable (dns {dns_ip})");
            return Ok(true);
        }
        if first_attempt {
            info!(
                log,
                "{label}: waiting for external network to converge (dns {}) ...",
                dns_ips.join(", ")
            );
            first_attempt = false;
        }
        let remaining =
            deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(SPACING.min(remaining));
    }
}

fn probe_external_dns(
    dns_ips: &[String],
    mut probe: impl FnMut(&str) -> Option<bool>,
) -> anyhow::Result<Option<&str>> {
    for dns_ip in dns_ips {
        match probe(dns_ip) {
            Some(true) => return Ok(Some(dns_ip)),
            Some(false) => {}
            None => {
                bail!(
                    "required host command `dig` could not be executed; install `dig` to validate external DNS reachability"
                )
            }
        }
    }
    Ok(None)
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

fn route_readback_confirms_nexthop(output: &str, nexthop: &str) -> bool {
    output.lines().any(|line| {
        line.trim()
            .strip_prefix("gateway:")
            .is_some_and(|gateway| gateway.trim() == nexthop)
    })
}

fn external_route_targets(prefix: &str, dns_ips: &[String]) -> Vec<String> {
    let mut targets = vec![prefix.to_string()];
    for ip in dns_ips {
        let host = format!("{ip}/32");
        if !targets.contains(&host) {
            targets.push(host);
        }
    }
    targets
}

pub(crate) async fn set_external_route(
    d: &Runner,
    ce: NodeRef,
    prefix: &str,
    dns_ips: &[String],
    apply: bool,
    static_ip: Option<&str>,
) -> anyhow::Result<()> {
    // A configured static ce address (`[topology].ce_external_ip`) is a stable
    // nexthop: use it directly and skip the slow, volatile serial-console lease
    // lookup. Otherwise read ce's DHCP lease as before.
    let ip = match static_ip {
        Some(s) => s.to_string(),
        None => node_external_ip(d, ce, true).await.context("ce")?,
    };

    for target in external_route_targets(prefix, dns_ips) {
        if !apply {
            info!(
                d.log,
                "external route (dry-run): route add {} {}", target, ip
            );
            continue;
        }

        // DNS host routes override dynamic /32s that illumos may have learned
        // through the host's default gateway.
        let dest = target.split('/').next().unwrap_or(&target);
        for gw in route_gateways(dest) {
            let _ = std::process::Command::new("route")
                .args(["delete", &target, &gw])
                .output();
        }
        for _ in 0..8 {
            let out = std::process::Command::new("route")
                .args(["delete", &target])
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
            .args(["add", &target, &ip])
            .output()
            .with_context(|| format!("route add {target}"))?;
        let readback = std::process::Command::new("route")
            .args(["-n", "get", dest])
            .output()
            .with_context(|| format!("route -n get {dest}"))?;
        let readback_stdout = String::from_utf8_lossy(&readback.stdout);
        if !route_readback_confirms_nexthop(&readback_stdout, &ip) {
            bail!(
                "route {target} -> {ip} was not confirmed by `route -n get {dest}`; route add output: {}{}; route readback: {}{}; run: route add {target} {ip}",
                String::from_utf8_lossy(&add.stdout).trim(),
                String::from_utf8_lossy(&add.stderr).trim(),
                readback_stdout.trim(),
                String::from_utf8_lossy(&readback.stderr).trim(),
            );
        }
        info!(d.log, "external route set: {} -> {} (ce)", target, ip);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::future::pending;

    #[tokio::test]
    async fn serial_exec_times_out_when_pending() {
        let error = serial_exec_with_timeout(
            Duration::from_millis(1),
            pending::<anyhow::Result<String>>(),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("timed out"), "{error:#}");
    }

    #[tokio::test]
    async fn serial_exec_returns_completed_output() {
        let output = serial_exec_with_timeout(
            Duration::from_millis(1),
            std::future::ready(Ok("198.51.100.1".to_string())),
        )
        .await
        .unwrap();

        assert_eq!(output, "198.51.100.1");
    }

    #[tokio::test]
    async fn serial_exec_errors_retain_context() {
        let error = serial_exec_with_timeout(
            Duration::from_millis(1),
            std::future::ready(Err(anyhow!("serial transport failed"))),
        )
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "read external IP");
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string() == "serial transport failed"),
            "{error:#}"
        );
    }

    #[test]
    fn external_routes_include_dns_hosts() {
        let dns =
            vec!["198.51.100.20".to_string(), "198.51.100.21".to_string()];

        assert_eq!(
            external_route_targets("198.51.100.0/24", &dns),
            ["198.51.100.0/24", "198.51.100.20/32", "198.51.100.21/32"]
        );
    }

    #[test]
    fn external_routes_deduplicate_dns_hosts() {
        let dns =
            vec!["198.51.100.20".to_string(), "198.51.100.20".to_string()];

        assert_eq!(
            external_route_targets("198.51.100.0/24", &dns),
            ["198.51.100.0/24", "198.51.100.20/32"]
        );
    }

    #[test]
    fn external_dns_probe_accepts_the_second_server() {
        let dns =
            vec!["198.51.100.20".to_string(), "198.51.100.21".to_string()];
        let mut probed = Vec::new();

        let reachable = probe_external_dns(&dns, |ip| {
            probed.push(ip.to_string());
            Some(ip == "198.51.100.21")
        })
        .unwrap();

        assert_eq!(reachable, Some("198.51.100.21"));
        assert_eq!(probed, dns);
    }

    #[test]
    fn external_dns_probe_reports_all_servers_unavailable() {
        let dns =
            vec!["198.51.100.20".to_string(), "198.51.100.21".to_string()];

        assert_eq!(probe_external_dns(&dns, |_| Some(false)).unwrap(), None);
    }
}
