// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Voxel-managed isolated external network.
//!
//! This stands up option 2 of omicron's [how-to-run external networking]
//! guide (an "external" network that exists only on the test machine) so a
//! box with no routable LAN can still run a rack.
//!
//! The recipe begins with a host etherstub carrying the segment, a host VNIC
//! holding `host_ip` (the nodes' default gateway), and IPv4 forwarding + ipnat
//! rule NAT'ing the subnet out a physical uplink. Node addresses are static:
//! voxel assigns each sled and router a deterministic address from `ip_start`
//! and stages it into that node's cargo-bay (`external-net`). `voxel-init`
//! applies the staged address in the guest, so no DHCP server runs on the
//! segment.
//!
//! The nodes' addresses stay in use after bring-up (RSS is polled over SSH to
//! them, each router NATs rack egress out its own external address, and the
//! host route to the rack points at ce's), so the segment must exist before
//! boot. This is why the launch owns it.
//!
//! Everything here is idempotent (guarded by the matching `show-*` probe) and
//! transient (`-t` flags). `down` removes the NAT rules with `ipnat -r`,
//! which deletes only the rules matching the piped text, so unrelated rules
//! survive. IPv4 forwarding stays enabled, as it is a host-global setting.
//!
//! [how-to-run external networking]: https://github.com/oxidecomputer/omicron/blob/main/docs/how-to-run.adoc#external-networking

use anyhow::{Context, bail};
use oxnet::Ipv4Net;
use std::io::Write;
use std::process::{Command, Stdio};
use voxel_config::External;

/// Etherstub carrying the isolated segment. Distinct from the how-to-run
/// `fake_external_stub0` name so a manually plumbed fake network can coexist.
pub(crate) const STUB: &str = "voxel_ext_stub0";
/// Host VNIC on the stub; owns the gateway address.
const VNIC: &str = "voxel_ext0";
/// ipadm address object on the VNIC.
const ADDROBJ: &str = "voxel_ext0/external";

/// MTU threshold for voxel-init's underlay classification. A sled NIC is
/// underlay iff it accepts jumbo frames (mtu=9000). An etherstub comes up at
/// 9000, so without a cap below this, the sleds' external NICs pass the jumbo
/// probe, get misclassified as underlay, and never come up. The cap itself
/// comes from `external.mtu` (default 1500).
const JUMBO_MTU: u32 = 9000;

/// Refuse an `external.mtu` the jumbo probe can't distinguish from the
/// underlay.
fn assert_mtu_classifiable(mtu: u32) -> anyhow::Result<()> {
    if mtu >= JUMBO_MTU {
        bail!(
            "external.mtu {mtu} must stay below {JUMBO_MTU}: voxel-init classifies a sled \
             NIC as underlay iff it accepts mtu={JUMBO_MTU}, so the external link has to \
             reject jumbo (voxel config set external.mtu 8900)"
        );
    }
    Ok(())
}

/// Run a read-only probe (true when it exits 0).
fn probe(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Capture a read-only probe's stdout (`None` on spawn failure or non-zero
/// exit).
fn probe_out(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a mutating host command under pfexec, or print it under `--dry-run`.
/// Whether `up`/`down` apply their host changes or only print them. A bare
/// `bool` at these call sites reads as an unexplained `false`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DryRun {
    /// Print the `pfexec` commands without running them.
    Yes,
    /// Apply the changes.
    No,
}

impl DryRun {
    /// True when commands should only be printed.
    fn applies(self) -> bool {
        matches!(self, DryRun::Yes)
    }

    pub(crate) fn from_flag(dry_run: bool) -> Self {
        if dry_run { DryRun::Yes } else { DryRun::No }
    }
}

fn run(dry_run: bool, args: &[&str]) -> anyhow::Result<()> {
    if dry_run {
        eprintln!("+ pfexec {}", args.join(" "));
        return Ok(());
    }
    let st = Command::new("pfexec")
        .args(args)
        .status()
        .with_context(|| format!("spawning pfexec {}", args.join(" ")))?;
    if !st.success() {
        bail!("pfexec {} failed ({st})", args.join(" "));
    }
    Ok(())
}

/// Create a temporary static address `addr` on `addrobj` via `ipadm`.
fn create_addr(dry_run: bool, addrobj: &str, addr: &str) -> anyhow::Result<()> {
    run(
        dry_run,
        &[
            "ipadm",
            "create-addr",
            "-t",
            "-T",
            "static",
            "--address",
            addr,
            addrobj,
        ],
    )
}

/// The uplink's `dladm show-phys` state, or `None` when the link is absent.
fn uplink_state(link: &str) -> Option<String> {
    probe_out("dladm", &["show-phys", "-p", "-o", "state", link])
        .map(|s| s.trim().to_string())
}

/// A link's current MTU.
///
/// Returns `None` when the link is absent.
pub(crate) fn link_mtu(link: &str) -> Option<String> {
    probe_out(
        "dladm",
        &["show-linkprop", "-c", "-p", "mtu", "-o", "value", link],
    )
    .map(|s| s.trim().to_string())
}

/// Every host IPv4 address currently plumbed, minus the segment's own VNIC
/// (so `up` stays idempotent), loopback, and link-local. `ipadm show-addr -p
/// -o addrobj,addr` prints one entry per line, e.g. `igb0/dhcp:172.20.0.5/24`,
/// or `tun0/v4:100.121.38.79->100.121.38.79` for point-to-point interfaces.
fn host_v4_addrs() -> Vec<Ipv4Net> {
    let Some(out) =
        probe_out("ipadm", &["show-addr", "-p", "-o", "addrobj,addr"])
    else {
        return Vec::new();
    };
    let own = format!("{VNIC}/");
    out.lines()
        .filter_map(|l| l.split_once(':'))
        .filter(|(addrobj, _)| !addrobj.starts_with(&own))
        .filter_map(|(_, addr)| parse_host_addr(addr))
        .filter(|net| !net.addr().is_loopback() && !net.addr().is_link_local())
        .collect()
}

/// Parse one `ipadm` address column entry. Point-to-point entries (VPN and
/// tunnel interfaces) print as `local->peer` with no prefix length and would
/// otherwise fail the CIDR parse and silently vanish from the overlap check.
/// The local side is what the host owns, so treat it as a /32.
fn parse_host_addr(addr: &str) -> Option<Ipv4Net> {
    match addr.split_once("->") {
        Some((local, _)) => {
            let local = local.split_once('/').map_or(local, |(ip, _)| ip);
            Ipv4Net::new(local.parse().ok()?, 32).ok()
        }
        None => addr.parse().ok(),
    }
}

/// Refuse an `external.subnet` that overlaps an address the host already owns.
/// A collision would either steal traffic from an existing network or make
/// the segment unreachable via the wrong route. Either way, no automatic
/// recovery.
fn assert_subnet_disjoint(subnet: &str) -> anyhow::Result<()> {
    let cfg: Ipv4Net = subnet.parse().with_context(|| {
        format!("external.subnet '{subnet}' must be CIDR (a.b.c.d/len)")
    })?;
    for host in host_v4_addrs() {
        if cfg.overlaps(&host) {
            bail!(
                "external.subnet '{subnet}' overlaps host address {host}; \
                 pick a subnet off the host's LANs (voxel config set external.subnet <cidr>)"
            );
        }
    }
    Ok(())
}

/// Refuse to NAT out a link that isn't up.
// A typo'd uplink would otherwise wire the segment to a dead link and fail
// silently.
fn assert_uplink_up(link: &str) -> anyhow::Result<()> {
    match uplink_state(link).as_deref() {
        Some("up") => Ok(()),
        state => bail!(
            "external.uplink '{link}' is '{}', not 'up'; pick an up link from \
             `dladm show-phys` (voxel config set external.uplink <link>)",
            state.unwrap_or("absent")
        ),
    }
}

/// The two how-to-run NAT rules (portmap for tcp/udp, bare map for the rest).
fn nat_rules(uplink: &str, subnet: &str) -> [String; 2] {
    [
        format!("map {uplink} {subnet} -> 0/32 portmap tcp/udp auto"),
        format!("map {uplink} {subnet} -> 0/32"),
    ]
}

/// Whether the subnet's map rules are already loaded (`ipnat -l` needs privs).
fn nat_loaded(uplink: &str, subnet: &str) -> bool {
    // Match on the rule prefix only. `ipnat -l` prints the target normalized
    // (`0/32` becomes `0.0.0.0/32`), so the full rule text would never match.
    probe_out("pfexec", &["ipnat", "-l"])
        .is_some_and(|l| l.contains(&format!("map {uplink} {subnet}")))
}

/// Append the NAT rules via `ipnat -f -`. This is append-only and never
/// flushes, so unrelated rules survive.
fn load_nat(uplink: &str, subnet: &str, dry_run: bool) -> anyhow::Result<()> {
    pipe_nat(uplink, subnet, &["ipnat", "-f", "-"], dry_run)
}

/// Remove the NAT rules via `ipnat -r -f -`. The `-r` flag deletes exactly
/// the rules matching the piped text, so unrelated rules survive. Removing
/// an absent rule prints a warning but exits 0, which keeps this idempotent.
fn unload_nat(uplink: &str, subnet: &str, dry_run: bool) -> anyhow::Result<()> {
    pipe_nat(uplink, subnet, &["ipnat", "-r", "-f", "-"], dry_run)
}

/// Pipe the subnet's rule text into `pfexec <args>` on stdin.
fn pipe_nat(
    uplink: &str,
    subnet: &str,
    args: &[&str],
    dry_run: bool,
) -> anyhow::Result<()> {
    let rules = nat_rules(uplink, subnet).join("\n");
    let cmd = args.join(" ");
    if dry_run {
        eprintln!(
            "+ printf '{}\\n' | pfexec {cmd}",
            rules.replace('\n', "\\n")
        );
        return Ok(());
    }
    let mut child = Command::new("pfexec")
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning pfexec {cmd}"))?;
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(format!("{rules}\n").as_bytes())
        .context("writing NAT rules to ipnat")?;
    let st = child.wait().context("waiting for ipnat")?;
    if !st.success() {
        bail!("{cmd} failed ({st})");
    }
    Ok(())
}

/// Stand the isolated segment up. This is safe to call on every launch,
/// as each step is guarded by its `show-*` probe and skipped once satisfied.
///
/// # Errors
///
/// Fails when `uplink` is unset or not up, when `mtu` reaches the jumbo
/// threshold, when `subnet` is not CIDR or overlaps a host-owned address, or
/// when one of the underlying `dladm`/`ipadm`/`routeadm`/`ipnat` commands
/// fails.
pub(crate) fn up(x: &External, dry_run: DryRun) -> anyhow::Result<()> {
    let dry_run = dry_run.applies();
    let uplink = x.uplink.as_deref().context(
        "external.uplink must be set in isolated mode (voxel config set external.uplink <link>)",
    )?;
    assert_uplink_up(uplink)?;
    assert_mtu_classifiable(x.mtu)?;
    assert_subnet_disjoint(&x.subnet)?;
    if !x.host_ip_is_usable() {
        bail!(
            "external.host_ip '{}' must be a usable address within external.subnet '{}'",
            x.host_ip,
            x.subnet
        );
    }
    let prefix = x.prefix_length().with_context(|| {
        format!("external.subnet '{}' must be CIDR (a.b.c.d/len)", x.subnet)
    })?;

    eprintln!(
        "[voxel] external: bringing up isolated segment ({} via {uplink})",
        x.subnet
    );

    if !probe("dladm", &["show-etherstub", STUB]) {
        run(dry_run, &["dladm", "create-etherstub", "-t", STUB])?;
    }
    if link_mtu(STUB).as_deref() != Some(&x.mtu.to_string()) {
        let mtu = format!("mtu={}", x.mtu);
        run(dry_run, &["dladm", "set-linkprop", "-t", "-p", &mtu, STUB]).with_context(|| {
            format!(
                "setting {STUB} to mtu {} (VNICs attached at a higher MTU block \
                 this: destroy the rack, `voxel network external down`, and retry)",
                x.mtu
            )
        })?;
    }
    if !probe("dladm", &["show-vnic", VNIC]) {
        run(dry_run, &["dladm", "create-vnic", "-t", "-l", STUB, VNIC])?;
    }
    if !probe("ipadm", &["show-if", VNIC]) {
        run(dry_run, &["ipadm", "create-if", "-t", VNIC])?;
    }
    let desired_addr = format!("{}/{prefix}", x.host_ip);
    let live_addr =
        probe_out("ipadm", &["show-addr", "-p", "-o", "addr", ADDROBJ])
            .map(|s| s.trim().to_string());
    match live_addr.as_deref() {
        Some(a) if a == desired_addr => {}
        Some(_) => {
            // Same addrobj, different address. Falls out when `host_ip` or the
            // subnet prefix changes across `up` invocations. Delete and
            // re-create so nodes staged with the new gateway can reach us.
            run(dry_run, &["ipadm", "delete-addr", ADDROBJ])?;
            create_addr(dry_run, ADDROBJ, &desired_addr)?;
        }
        None => {
            create_addr(dry_run, ADDROBJ, &desired_addr).with_context(|| {
                format!(
                    "creating {ADDROBJ} ({desired_addr}): another link already holding \
                     the address blocks this"
                )
            })?;
        }
    }
    if !probe_out("routeadm", &["-p", "ipv4-forwarding"])
        .is_some_and(|s| s.contains("current=enabled"))
    {
        run(dry_run, &["routeadm", "-e", "ipv4-forwarding", "-u"])?;
    }
    if probe_out("svcs", &["-Ho", "state", "svc:/network/ipfilter:default"])
        .map(|s| s.trim() != "online")
        .unwrap_or(true)
    {
        run(dry_run, &["svcadm", "enable", "-s", "ipfilter"])?;
    }
    if !nat_loaded(uplink, &x.subnet) {
        load_nat(uplink, &x.subnet, dry_run)?;
    }
    eprintln!(
        "[voxel] external: up ({VNIC} = {}; nodes get static addresses from {})",
        x.host_ip, x.ip_start
    );
    Ok(())
}

/// Tear the segment down, including address, interface, VNIC, etherstub, and
/// the NAT rules. IPv4 forwarding stays enabled (see module doc).
///
/// # Errors
///
/// Fails when a delete command fails, e.g. the etherstub still carries node
/// VNICs from a running rack.
pub(crate) fn down(x: &External, dry_run: DryRun) -> anyhow::Result<()> {
    let dry_run = dry_run.applies();
    eprintln!("[voxel] external: taking down isolated segment");
    if probe("ipadm", &["show-addr", ADDROBJ]) {
        run(dry_run, &["ipadm", "delete-addr", ADDROBJ])?;
    }
    if probe("ipadm", &["show-if", VNIC]) {
        run(dry_run, &["ipadm", "delete-if", VNIC])?;
    }
    if probe("dladm", &["show-vnic", VNIC]) {
        run(dry_run, &["dladm", "delete-vnic", VNIC])?;
    }
    if probe("dladm", &["show-etherstub", STUB]) {
        run(dry_run, &["dladm", "delete-etherstub", STUB]).with_context(|| {
            format!(
                "deleting {STUB} (node VNICs still attached block this: destroy the rack and retry)"
            )
        })?;
    }
    if let Some(uplink) = x.uplink.as_deref()
        && nat_loaded(uplink, &x.subnet)
    {
        unload_nat(uplink, &x.subnet, dry_run)?;
        eprintln!(
            "[voxel] external: leaving ipv4-forwarding enabled (host-global setting)"
        );
    }
    Ok(())
}

/// Assert the whole path is live, printing one PASS/FAIL line per item.
///
/// # Errors
///
/// Fails when any item is missing, so the CLI exit code reflects the result.
pub(crate) fn check(x: &External) -> anyhow::Result<()> {
    let mut ok = true;
    let mut item = |good: bool, what: &str| {
        println!("{} {what}", if good { "ok:     " } else { "MISSING:" });
        ok &= good;
    };
    match x.uplink.as_deref() {
        Some(l) => {
            let up = uplink_state(l).as_deref() == Some("up");
            item(up, &format!("uplink {l} is up"));
            item(
                nat_loaded(l, &x.subnet),
                &format!("ipnat map rules for {l} {}", x.subnet),
            );
        }
        None => item(false, "external.uplink set"),
    }
    item(
        probe("dladm", &["show-etherstub", STUB]),
        &format!("etherstub {STUB}"),
    );
    item(
        link_mtu(STUB).as_deref() == Some(&x.mtu.to_string()),
        &format!(
            "etherstub mtu {} (external NICs must fail the jumbo probe)",
            x.mtu
        ),
    );
    item(probe("dladm", &["show-vnic", VNIC]), &format!("vnic {VNIC}"));
    // Match on the exact address, not just addrobj existence: a stale addr
    // from a prior host_ip / subnet-prefix config would otherwise pass.
    let expected = x
        .prefix_length()
        .map(|p| format!("{}/{p}", x.host_ip))
        .unwrap_or_else(|| x.host_ip.clone());
    let live_ok =
        probe_out("ipadm", &["show-addr", "-p", "-o", "addr", ADDROBJ])
            .map(|s| s.trim().to_string())
            == Some(expected.clone());
    item(live_ok, &format!("addr {ADDROBJ} ({expected})"));
    item(
        probe_out("routeadm", &["-p", "ipv4-forwarding"])
            .is_some_and(|s| s.contains("current=enabled")),
        "ipv4-forwarding enabled",
    );
    item(
        probe_out("svcs", &["-Ho", "state", "svc:/network/ipfilter:default"])
            .is_some_and(|s| s.trim() == "online"),
        "ipfilter online",
    );
    if ok {
        println!("check: PASS");
        Ok(())
    } else {
        bail!("check: FAIL")
    }
}
