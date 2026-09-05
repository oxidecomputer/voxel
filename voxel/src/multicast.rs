// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Host-side plumbing for running multicast into a running rack.
//!
//! The vtysh command rendering and parsing live in [`frr`].
//!
//! A host route sends each group's packets out through the external segment
//! (L2 multicast), and once the fabric router receives them, a static
//! [source-specific][RFC 4607] mroute replicates them toward its requesting
//! rack-facing links. The rack does not signal upstream yet (v1), making the
//! complete path explicit.
//!
//! The `--steer` flag selects the delivery paths for each group.
//!
//! Selecting multiple links creates multiple forwarding paths into the rack.
//! Viona's receive-path deduplication only handles duplicate local paths when
//! packets reach the guests.
//!
//! A VIF is the kernel multicast-routing API's virtual interface. pimd
//! registers a per-interface slot (`MRT_ADD_VIF`) so that an interface
//! can appear as an mroute's incoming interface (iif) or outgoing
//! interface (oif). pimd runs no PIM protocol here. Every VIF is set to
//! passive and forwarding state consists of only the static mroutes.
//!
//! PIM needs an IPv4 address to create a VIF. The rack-facing links have no
//! production IPv4 addresses, only IPv6 link-locals (RFC 5549). The generated
//! router configuration assigns [documentation-prefix][RFC 5737 §3]
//! `/32`s for PIM, which is bootstrapped before the multicast setup runs.
//!
//! The host offloads checksumming, forcing a multicast packet looped back to
//! arrive with its IPv4 header checksum unset. The routers validate that
//! header and their Linux stack drops the packet at IP input. Softnpu
//! accepts the frame. Tofino drops it in the parser stage of the pipeline.
//!
//! The host route table lives in the global zone and is shared across falcon
//! environments. Voxel records each environment's groups, gateways, sources,
//! and steering selections under the `.falcon/` directory, leaving routes
//! owned by another environment untouched when setup or teardown take place.
//!
//! See [RFC 4607] for source-specific multicast (SSM), and [RFC 5737 §3]
//! for the documentation prefix the rack-facing links are numbered from.
//!
//! [RFC 4607]: https://www.rfc-editor.org/rfc/rfc4607
//! [RFC 5737 §3]: https://www.rfc-editor.org/rfc/rfc5737#section-3

use std::fs;
use std::io::{ErrorKind, Write};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow, bail, ensure};
use itertools::Itertools;
use tempfile::NamedTempFile;
use voxel_config::VoxelConfig;

use crate::net::{
    ROUTE, RouteEntry, SshFailure, ce_static_ip, host_external_ip,
    resolve_external_ip, route_entries, serial_bounded, ssh_capture,
    ssh_try_capture, static_ip,
};
use crate::topo::build_topo;

mod frr;
use frr::{
    Mroute, Transit, config_errors, parse_pim_ifaces, vtysh_config, vtysh_show,
};

mod record;
mod steering;

use record::{
    MulticastRoute, MulticastState, lock_multicast_routes,
    read_multicast_state, read_multicast_states, write_multicast_state,
};
pub(crate) use steering::{GroupSpec, SteerSpec};
use steering::{Steering, group_addrs};

/// The netstat flag illumos uses for a host route.
const HOST_ROUTE_FLAG: char = 'H';

fn fabric_routers(cfg: &VoxelConfig) -> anyhow::Result<Vec<String>> {
    let routers = cfg.multicast_routers();
    ensure!(
        !routers.is_empty(),
        "topology.routers has no fabric router to forward from"
    );
    Ok(routers)
}

/// A router's external address in either mode:
///
/// - static addressing's assignment comes directly from configuration.
/// - DHCP addressing reads the node's lease over the falcon console
///   under a bounded deadline.
async fn node_addr(
    cfg: &VoxelConfig,
    name: &str,
    node: &str,
) -> anyhow::Result<String> {
    if let Some(ip) = static_ip(cfg, node) {
        return Ok(ip);
    }
    let topo = build_topo(cfg, name)?;
    let n = topo
        .node_ref(node)
        .with_context(|| format!("{node} is not in the topology"))?;
    serial_bounded(
        &format!("reading {node}'s address"),
        resolve_external_ip(cfg, &topo.runner, node, n, true),
    )
    .await
    .with_context(|| {
        format!("cannot resolve {node}'s external address (is the rack up?)")
    })
}

/// `ce`'s external address, i.e., the nexthop every group's host route points
/// at. An explicit `[topology] ce_external_ip` or static addressing's
/// numbering resolves without touching the guest.
///
/// If not configured explicitly, `ce`'s lease is read from the
/// running node.
async fn ce_nexthop(cfg: &VoxelConfig, name: &str) -> anyhow::Result<String> {
    match ce_static_ip(cfg) {
        Some(ip) => Ok(ip),
        None => node_addr(cfg, name, "ce").await,
    }
}

/// Run a command on the transit router, or print it under `--dry-run`.
///
/// Returns its stdout and fails when ssh cannot reach the router or the command
/// exits with a non-zero.
fn router_run(ip: &str, cmd: &str, dry_run: bool) -> anyhow::Result<String> {
    if dry_run {
        eprintln!("+ ssh root@{ip} {cmd}");
        return Ok(String::new());
    }
    ssh_capture(ip, cmd).with_context(|| {
        format!(
            "`{cmd}` on {ip} (is the rack up and its external NIC addressed?)"
        )
    })
}

/// Run a host command under pfexec, or print it under `--dry-run`.
///
/// illumos `route` exits non-zero even on a successful add, so the status is
/// not checked here. `up` and `down` commands re-read the table instead.
fn host_route(args: &[&str], dry_run: bool) -> anyhow::Result<()> {
    if dry_run {
        eprintln!("+ pfexec route {}", args.join(" "));
        return Ok(());
    }
    Command::new("pfexec")
        .arg(ROUTE)
        .args(args)
        .output()
        .map(drop)
        .with_context(|| format!("run pfexec route {}", args.join(" ")))
}

/// The host-route gateways currently listed for `group`.
fn host_route_gateways(
    entries: &[RouteEntry],
    group: &Ipv4Addr,
) -> Vec<String> {
    let group = group.to_string();
    entries
        .iter()
        .filter(|entry| {
            entry.dest == group
                && is_host_route(entry)
                && !entry.gateway.is_empty()
        })
        .map(|entry| entry.gateway.clone())
        .unique()
        .collect()
}

/// Whether a netstat route entry is a host route.
fn is_host_route(entry: &RouteEntry) -> bool {
    entry.flags.contains(HOST_ROUTE_FLAG)
}

/// Drop only the host routes whose gateways this environment owns.
fn purge_route(
    group: &Ipv4Addr,
    entries: &[RouteEntry],
    owned_gateways: &[String],
    dry_run: bool,
) -> anyhow::Result<()> {
    let group_text = group.to_string();
    for gateway in host_route_gateways(entries, group)
        .into_iter()
        .filter(|gateway| owned_gateways.contains(gateway))
    {
        host_route(&["delete", "-host", &group_text, &gateway], dry_run)?;
    }
    Ok(())
}

fn host_route_lines(
    entries: &[RouteEntry],
    addrs: &[Ipv4Addr],
    state: Option<&MulticastState>,
    want_owned: bool,
) -> Vec<String> {
    addrs
        .iter()
        .flat_map(|addr| {
            let owned = state
                .and_then(|state| state.route(addr))
                .map(MulticastRoute::gateways)
                .unwrap_or_default();

            host_route_gateways(entries, addr)
                .into_iter()
                .filter(move |gateway| owned.contains(gateway) == want_owned)
                .map(move |gateway| format!("host route {addr} -> {gateway}"))
        })
        .collect()
}

/// Name the host routes teardown process leaves in place.
fn report_unrecorded_routes(
    entries: &[RouteEntry],
    addrs: &[Ipv4Addr],
    state: Option<&MulticastState>,
    name: &str,
) {
    for route in host_route_lines(entries, addrs, state, false) {
        eprintln!(
            "[voxel] multicast: leaving {route}; it is not owned by falcon \
             environment '{name}'"
        );
    }
}

/// The groups recorded for a falcon environment.
///
/// This is what a groupless `down` or `check` command starts from before a scan
/// of the host route table.
fn state_groups(state: Option<&MulticastState>) -> Vec<Ipv4Addr> {
    state.map(|state| state.groups().collect()).unwrap_or_default()
}

/// Remove the host routes recorded for a falcon environment.
fn purge_state_routes(
    addrs: &[Ipv4Addr],
    state: Option<&MulticastState>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let Some(state) = state else {
        return Ok(());
    };
    let entries = route_entries()?;
    for addr in addrs {
        let Some(route) = state.route(addr) else {
            continue;
        };
        purge_route(addr, &entries, &route.gateways(), dry_run)?;
    }
    Ok(())
}

/// Purge the environment's host routes before falcon destroys it.
///
/// Multicast state stays around until teardown succeeds.
pub(crate) fn prepare_destroy(
    name: &str,
) -> anyhow::Result<record::MulticastLock> {
    let multicast_lock = lock_multicast_routes()?;
    let state = read_multicast_state(name)?;
    let Some(state) = state else {
        return Ok(multicast_lock);
    };
    let addrs = state.groups().collect::<Vec<_>>();
    purge_state_routes(&addrs, Some(&state), false)?;
    let entries = route_entries()?;
    let remaining = host_route_lines(&entries, &addrs, Some(&state), true);
    ensure!(
        remaining.is_empty(),
        "host routes remain before destroy:\n  {}",
        remaining.join("\n  ")
    );
    Ok(multicast_lock)
}

fn multicast_source(group: &Ipv4Addr) -> anyhow::Result<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .context("bind the multicast route probe")?;

    socket
        .connect((*group, 9))
        .with_context(|| format!("select the route to {group}"))?;

    match socket.local_addr().context("read the multicast route source")?.ip() {
        IpAddr::V4(source) => Ok(source),
        IpAddr::V6(source) => {
            bail!("the route to {group} selected IPv6 source {source}")
        }
    }
}

/// Wrap an [`SshFailure`] for `cmd` on `ip`.
fn router_read_err(ip: &str, cmd: &str, e: SshFailure) -> anyhow::Error {
    match e {
        SshFailure::Unreachable => {
            anyhow!("router {ip} unreachable while running `{cmd}`")
        }
        SshFailure::Failed(text) => {
            anyhow!("`{cmd}` on {ip} failed: {}", text.trim())
        }
    }
}

/// The kernel tunable deciding whether the host computes checksums itself,
/// as read through `mdb`.
///
/// Returns `None` when it cannot be read, typically due to the lack of
/// privilege in opening the kernel.
fn dohwcksum() -> Option<u32> {
    let mut child = Command::new("pfexec")
        .args(["mdb", "-k"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(b"dohwcksum/D\n").ok()?;
    let out = child.wait_with_output().ok()?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().last()?.parse().ok()
}

const DOHWCKSUM_SYSTEM_LINE: &str = "set ip:dohwcksum = 0";
const DOHWCKSUM_SYSTEM_DIR: &str = "/etc/system.d";
const DOHWCKSUM_SYSTEM_FILE: &str = "/etc/system.d/voxel";

fn dohwcksum_assignment(line: &str) -> Option<bool> {
    let line = line.trim().to_ascii_lowercase();
    let rest = line.strip_prefix("set")?;
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let setting = rest.trim_start();
    let setting_stripped = setting.strip_prefix("ip:").unwrap_or(setting);
    let rest = setting_stripped.strip_prefix("dohwcksum")?;
    let Some(next) = rest.chars().next() else {
        return Some(false);
    };
    if !next.is_whitespace() && !matches!(next, '=' | '&' | '|') {
        return None;
    }
    let rest = rest.trim_start();
    let Some(value) = rest.strip_prefix('=') else {
        return Some(false);
    };
    Some(matches!(value.trim(), "0" | "0x0"))
}

fn checksum_system_files() -> anyhow::Result<Vec<PathBuf>> {
    let mut files = vec![PathBuf::from("/etc/system")];
    let entries = match fs::read_dir(DOHWCKSUM_SYSTEM_DIR) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(files),
        Err(e) => return Err(e).context("read /etc/system.d"),
    };

    for entry in entries {
        let entry = entry.context("read an /etc/system.d entry")?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.as_path() == Path::new(DOHWCKSUM_SYSTEM_FILE) {
            continue;
        }
        if fs::metadata(&path)
            .with_context(|| format!("read metadata for {}", path.display()))?
            .is_file()
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn ensure_no_dohwcksum_conflicts() -> anyhow::Result<()> {
    for path in checksum_system_files()? {
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("read {}", path.display()));
            }
        };

        for (index, line) in contents.lines().enumerate() {
            if dohwcksum_assignment(line) == Some(false) {
                bail!(
                    "conflicting dohwcksum setting at {}:{}",
                    path.display(),
                    index + 1
                );
            }
        }
    }
    Ok(())
}

fn remove_staged_system_file(path: &Path) {
    let _ = Command::new("pfexec")
        .args(["rm", "-f"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn persist_dohwcksum(dry_run: bool) -> anyhow::Result<()> {
    ensure_no_dohwcksum_conflicts()?;
    let contents = format!("{DOHWCKSUM_SYSTEM_LINE}\n");
    if !dry_run {
        match fs::read_to_string(DOHWCKSUM_SYSTEM_FILE) {
            Ok(current) if current == contents => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).context("read /etc/system.d/voxel");
            }
        }
    }

    let mut source =
        NamedTempFile::new().context("create the system fragment")?;
    source
        .write_all(contents.as_bytes())
        .context("write the system fragment")?;
    source.flush().context("flush the system fragment")?;

    let staged = PathBuf::from(DOHWCKSUM_SYSTEM_DIR)
        .join(format!(".voxel.{}", std::process::id()));

    if dry_run {
        eprintln!(
            "+ pfexec install -D -m 0644 -o root -g bin {} {}",
            source.path().display(),
            staged.display()
        );
        eprintln!(
            "+ pfexec mv -f {} {}",
            staged.display(),
            DOHWCKSUM_SYSTEM_FILE
        );
        return Ok(());
    }

    let status = Command::new("pfexec")
        .args(["install", "-D", "-m", "0644", "-o", "root", "-g", "bin"])
        .arg(source.path())
        .arg(&staged)
        .status()
        .context("stage /etc/system.d/voxel")?;

    if !status.success() {
        remove_staged_system_file(&staged);
        bail!("could not stage /etc/system.d/voxel");
    }

    let status = Command::new("pfexec")
        .args(["mv", "-f"])
        .arg(&staged)
        .arg(DOHWCKSUM_SYSTEM_FILE)
        .status()
        .context("install /etc/system.d/voxel")?;

    if !status.success() {
        remove_staged_system_file(&staged);
        bail!("could not install /etc/system.d/voxel");
    }
    Ok(())
}

/// Disable hardware checksum offload now and across reboots.
///
/// The host-global `dohwcksum` gate covers the IPv4 header checksum, ULP
/// checksums, and LSO, for both address IP families.
///
/// The `/etc/system.d/voxel` path gets applied at boot. The `mdb` write here
/// modifies the running kernel.
///
/// When the `down` command is run, this operation leaves the host setting in
/// place.
///
/// A preview prints both writes without changing the host.
fn checksum_preflight(dry_run: bool) -> anyhow::Result<()> {
    persist_dohwcksum(dry_run)?;

    if dry_run {
        eprintln!("+ printf 'dohwcksum/W 0\\n' | pfexec mdb -kw");
        return Ok(());
    }

    // An unreadable tunable config item must not pass for a disabled one.
    let value = dohwcksum().context(
        "could not read ip:dohwcksum via `pfexec mdb -k` (checksum offload \
         state unverified)",
    )?;

    if value != 0 {
        let mut child = Command::new("pfexec")
            .args(["mdb", "-kw"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("disable checksum offload")?;
        child
            .stdin
            .take()
            .context("open mdb input")?
            .write_all(b"dohwcksum/W 0\n")
            .context("write the dohwcksum setting")?;
        ensure!(
            child.wait().context("wait for mdb")?.success(),
            "could not disable checksum offload"
        );
    }

    ensure!(
        dohwcksum() == Some(0),
        "the host is still offloading checksums, or the setting became \
         unreadable. Offloaded frames are dropped at IP input."
    );
    Ok(())
}

/// The interfaces PIM currently holds a VIF on.
fn router_pim_ifaces(ip: &str) -> anyhow::Result<Vec<String>> {
    let cmd = vtysh_show("show ip pim interface json");
    let out =
        ssh_try_capture(ip, &cmd).map_err(|e| router_read_err(ip, &cmd, e))?;
    parse_pim_ifaces(&out).with_context(|| format!("`{cmd}` on {ip}"))
}

/// Point each group's host route at `ce`, recording ownership first.
///
/// The routes belong to the falcon environment rather than to any one router,
/// so they go in once however many routers forward the groups. A route already
/// owned by another environment is rejected.
///
/// # Errors
///
/// This fails when:
/// - a route is not recorded for this environment
/// - a route does not take on installation
fn install_host_routes(
    addrs: &[Ipv4Addr],
    nexthop: &str,
    state: &MulticastState,
    prior: &MulticastState,
    dry_run: bool,
) -> anyhow::Result<()> {
    let entries = route_entries()?;
    for addr in addrs {
        let group = addr.to_string();
        let route = state
            .route(addr)
            .with_context(|| format!("{group} has no ownership record"))?;
        let owned_gateways = route.gateways();
        let prior_gateways =
            prior.route(addr).map(MulticastRoute::gateways).unwrap_or_default();
        let unrecorded: Vec<String> = host_route_gateways(&entries, addr)
            .into_iter()
            .filter(|gateway| !prior_gateways.contains(gateway))
            .collect();

        ensure!(
            unrecorded.is_empty(),
            "host route {group} is not recorded for falcon environment \
             '{}' (gateway {})",
            state.environment,
            unrecorded.join(", ")
        );
        purge_route(addr, &entries, &owned_gateways, dry_run)?;
        host_route(&["add", "-host", &group, nexthop], dry_run)?;
    }
    if dry_run {
        return Ok(());
    }

    let entries = route_entries()?;
    for addr in addrs {
        let owned_gateways =
            state.route(addr).map(MulticastRoute::gateways).unwrap_or_default();
        let actual = host_route_gateways(&entries, addr)
            .into_iter()
            .filter(|gateway| owned_gateways.contains(gateway))
            .collect::<Vec<_>>();
        ensure!(
            actual == [nexthop],
            "host route {addr} -> {nexthop} did not take (found {})",
            actual.join(", ")
        );
    }
    Ok(())
}

fn ensure_host_route_ownership(
    addrs: &[Ipv4Addr],
    entries: &[RouteEntry],
    state: &MulticastState,
    prior: &MulticastState,
) -> anyhow::Result<()> {
    let current = addrs
        .iter()
        .filter_map(|addr| state.route(addr).map(|route| (*addr, route)))
        .collect::<Vec<_>>();
    for other in read_multicast_states()? {
        if other.environment == state.environment {
            continue;
        }
        for (addr, route) in &current {
            let Some(other_route) = other.route(addr) else { continue };
            ensure!(
                route
                    .gateways()
                    .iter()
                    .all(|gateway| !other_route.gateways().contains(gateway)),
                "host route {addr} is already recorded for falcon environment '{}'",
                other.environment
            );
        }
    }
    for addr in addrs {
        let prior_gateways =
            prior.route(addr).map(MulticastRoute::gateways).unwrap_or_default();
        let unrecorded: Vec<String> = host_route_gateways(entries, addr)
            .into_iter()
            .filter(|gateway| !prior_gateways.contains(gateway))
            .collect();

        ensure!(
            unrecorded.is_empty(),
            "host route {addr} is not recorded for falcon environment \
             '{}' (gateway {})",
            state.environment,
            unrecorded.join(", ")
        );
    }
    Ok(())
}

struct MulticastUpdate<'a> {
    cfg: &'a VoxelConfig,
    name: &'a str,
    addrs: &'a [Ipv4Addr],
    source: &'a Ipv4Addr,
    steering: &'a Steering,
    state: &'a MulticastState,
    dry_run: bool,
}

impl MulticastUpdate<'_> {
    /// Assign every group to one router's forwarding links, bringing up
    /// `pimd` and the VIFs first when that router does not already carry
    /// them.
    ///
    /// # Errors
    ///
    /// This fails when:
    /// - the router is unreachable
    /// - `pimd` will not start
    /// - a configuration command fails
    async fn up_one(&self, router_name: &str) -> anyhow::Result<()> {
        let transit = Transit::new(self.cfg, router_name)?;
        let router_ip = node_addr(self.cfg, self.name, &transit.router).await?;

        eprintln!(
            "[voxel] multicast: {} on {} -> {}",
            transit.router,
            transit.iif,
            transit.ifaces.join(" ")
        );

        // Reapply the static PIM VIFs and SSM range before programming mroutes.
        let cmds: Vec<String> = voxel_config::frr::ssm_range_cmds()
            .into_iter()
            .chain(transit.vif_cmds())
            .chain(self.addrs.iter().flat_map(|addr| {
                let links = self.steering.selected_links(addr, &transit.router);
                self.state
                    .route(addr)
                    .into_iter()
                    .flat_map(|route| route.sources())
                    .flat_map(|previous_source| {
                        transit.no_mroute_cmds(addr, &previous_source, None)
                    })
                    .chain(transit.mroute_cmds(
                        addr,
                        self.source,
                        links.as_deref(),
                    ))
            }))
            .collect();

        let out = router_run(&router_ip, &vtysh_config(&cmds), self.dry_run)
            .with_context(|| {
                format!("program multicast assignments on {}", transit.router)
            })?;
        let errors = config_errors(&out);
        ensure!(
            errors.is_empty(),
            "FRR rejected multicast assignments on {}: {}",
            transit.router,
            errors.join("; ")
        );
        if !self.dry_run {
            let cmd = vtysh_show("show ip mroute json");
            let out = ssh_try_capture(&router_ip, &cmd)
                .map_err(|error| router_read_err(&router_ip, &cmd, error))?;
            for addr in self.addrs {
                let Some(route) = self.state.route(addr) else { continue };
                for source in route.sources() {
                    let expected = if source == *self.source {
                        let links =
                            self.steering.selected_links(addr, &transit.router);
                        transit.selected(links.as_deref())
                    } else {
                        Vec::new()
                    };
                    let live = Mroute::parse(&out, addr, &source)?;
                    ensure!(
                        Mroute::matches(live.as_ref(), &transit.iif, &expected),
                        "multicast assignment ({source}, {addr}) on {} \
                         does not match the requested paths; \
                         update record retained",
                        transit.router
                    );
                }
            }
        }
        Ok(())
    }
}

/// On run, point each group's host route at `ce` and program the selected
/// ingress path.
///
/// This is safe to re-run because this environment's routes are deleted before
/// being re-added.
///
/// # Errors
///
/// This fails when:
/// - a host route is owned by another falcon environment
/// - the host still offloads checksums
/// - `ce`'s or a router's external address cannot be resolved
/// - a router is unreachable
/// - a host route does not take upon run-up
pub(crate) async fn up(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[GroupSpec],
    steer: &[SteerSpec],
    dry_run: bool,
) -> anyhow::Result<()> {
    let _multicast_lock = lock_multicast_routes()?;
    let addrs = group_addrs(groups);
    ensure!(!addrs.is_empty(), "no multicast groups were given");
    let mut steering = Steering::parse(steer, cfg)?;
    let routers = fabric_routers(cfg)?;
    let mut state = read_multicast_state(name)?
        .unwrap_or_else(|| MulticastState::new(name, &routers[0]));

    state.ensure_topology_exists(&addrs, &routers, cfg.scrimlet_count())?;

    for router in &routers {
        state.add_router(router);
    }

    // A path for a group this run is not plumbing would be silently ignored,
    // which reads exactly like a typo taking effect.
    if let Some(stray) = steering.groups().find(|addr| !addrs.contains(addr)) {
        bail!("--steer names {stray}, which is not among the groups to plumb");
    }

    steering.reuse_recorded(Some(&state), &addrs);

    steering.finalize(cfg, &addrs, &routers);
    steering.ensure_single_path_per_router(&routers)?;

    let nexthop = ce_nexthop(cfg, name).await?;

    let prior_state = state.clone();
    for addr in &addrs {
        state.prepare_gateway(*addr, &nexthop);
    }
    let entries = route_entries()?;
    ensure_host_route_ownership(&addrs, &entries, &state, &prior_state)?;
    checksum_preflight(dry_run)?;
    if !dry_run {
        write_multicast_state(name, &state).with_context(|| {
            format!("record host routes for falcon '{name}'")
        })?;
    }
    install_host_routes(&addrs, &nexthop, &state, &prior_state, dry_run)?;

    let source = if dry_run {
        host_external_ip(cfg)?
            .parse()
            .context("the host's external address is not IPv4")?
    } else {
        let sources: Vec<Ipv4Addr> = addrs
            .iter()
            .map(multicast_source)
            .process_results(|sources| sources.unique().collect())?;
        ensure!(
            sources.len() == 1,
            "the groups route from different sources ({}); plumb them in \
             separate runs",
            sources.iter().join(", ")
        );
        sources[0]
    };

    for addr in &addrs {
        let selection = steering.selected_paths(addr).into();
        state.prepare_route(addr, source, selection);
    }

    if !dry_run {
        write_multicast_state(name, &state).with_context(|| {
            format!("record multicast update for falcon '{name}'")
        })?;
    }

    eprintln!(
        "[voxel] multicast: {} group(s) via ce {nexthop} from {source}, \
         forwarded by {}",
        addrs.len(),
        routers.join(" ")
    );

    let update = MulticastUpdate {
        cfg,
        name,
        addrs: &addrs,
        source: &source,
        steering: &steering,
        state: &state,
        dry_run,
    };

    for router in &routers {
        update.up_one(router).await?;
    }

    if !dry_run {
        state.commit_routes(&addrs);
        write_multicast_state(name, &state).with_context(|| {
            format!("commit multicast update for falcon '{name}'")
        })?;
        eprintln!("[voxel] multicast: up");
    }
    Ok(())
}

/// Withdraw each group's assignment on every router and remove its host
/// routes.
///
/// With no groups given, this tears down everything this falcon environment
/// has recorded on its routers, undoing any sequence of `up` runs without
/// scanning other environment's host routes.
///
/// # Errors
///
/// This fails when a router address cannot be resolved, or if anything
/// survives the teardown.
pub(crate) async fn down(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[GroupSpec],
    dry_run: bool,
) -> anyhow::Result<()> {
    let _multicast_lock = lock_multicast_routes()?;
    let state = read_multicast_state(name)?;
    let addrs = if groups.is_empty() {
        state_groups(state.as_ref())
    } else {
        group_addrs(groups)
    };
    if state.is_none() && addrs.is_empty() {
        eprintln!(
            "[voxel] multicast: no recorded state for {name}; nothing to do"
        );
        return Ok(());
    }
    eprintln!("[voxel] multicast: removing group assignments and host routes");

    // Host routes first. They are the one piece that outlives `voxel destroy`,
    // and resolving a router's address needs the rack up in lan mode, so a
    // destroyed rack must not abort teardown before they are gone.
    purge_state_routes(&addrs, state.as_ref(), dry_run)?;

    let entries = route_entries()?;
    report_unrecorded_routes(&entries, &addrs, state.as_ref(), name);
    if !dry_run && let Some(state) = state.as_ref() {
        let remaining = host_route_lines(&entries, &addrs, Some(state), true);
        ensure!(
            remaining.is_empty(),
            "host routes remain after teardown:\n  {}",
            remaining.join("\n  ")
        );
    }

    let Some(mut state) = state else { return Ok(()) };
    let addrs: Vec<Ipv4Addr> = addrs
        .iter()
        .copied()
        .filter(|addr| state.route(addr).is_some())
        .collect();

    if addrs.is_empty() {
        return Ok(());
    }

    let routers = fabric_routers(cfg)?;
    state.ensure_topology_exists(&addrs, &routers, cfg.scrimlet_count())?;

    // Then the routers. The record supplies the assignments to withdraw, so
    // there is no need to query FRR before changing them.
    let mut reachable = Vec::new();
    for router in &routers {
        let transit = Transit::new(cfg, router)?;
        let router_ip = match node_addr(cfg, name, &transit.router).await {
            Ok(ip) => ip,
            Err(e) if dry_run => {
                eprintln!(
                    "[voxel] multicast: cannot resolve {} ({e:#}); \
                     skipping router preview",
                    transit.router
                );
                continue;
            }
            Err(e) => return Err(e),
        };
        reachable.push((transit, router_ip));
    }

    for (transit, router_ip) in &reachable {
        let mut cmds = Vec::new();
        for addr in &addrs {
            let Some(route) = state.route(addr) else {
                continue;
            };
            let sources = route.sources();
            if sources.is_empty() {
                eprintln!(
                    "[voxel] multicast: leaving {addr}'s router assignments; \
                     its state record has no source ownership"
                );
                continue;
            }
            for source in sources {
                cmds.extend(transit.no_mroute_cmds(addr, &source, None));
            }
        }
        if cmds.is_empty() {
            continue;
        }
        let out = router_run(router_ip, &vtysh_config(&cmds), dry_run)
            .with_context(|| {
                format!("withdraw multicast assignments on {}", transit.router)
            })?;
        let errors = config_errors(&out);
        ensure!(
            errors.is_empty(),
            "FRR rejected multicast withdrawals on {}: {}",
            transit.router,
            errors.join("; ")
        );
        if !dry_run {
            let cmd = vtysh_show("show ip mroute json");
            let out = ssh_try_capture(router_ip, &cmd)
                .map_err(|error| router_read_err(router_ip, &cmd, error))?;
            for addr in &addrs {
                let Some(route) = state.route(addr) else { continue };
                for source in route.sources() {
                    let live = Mroute::parse(&out, addr, &source)?;
                    ensure!(
                        Mroute::matches(live.as_ref(), &transit.iif, &[]),
                        "multicast assignment ({source}, {addr}) remains \
                         on {}; ownership record retained",
                        transit.router
                    );
                }
            }
        }
    }

    if !dry_run {
        state.remove_groups(&addrs);
        write_multicast_state(name, &state)?;
    }
    Ok(())
}

/// The PIM state on the interface where groups arrive.
async fn plumbing_status_one(
    cfg: &VoxelConfig,
    name: &str,
    router_name: &str,
    addrs: &[Ipv4Addr],
    steering: &Steering,
    state: Option<&MulticastState>,
) -> anyhow::Result<Vec<(String, bool)>> {
    let transit = Transit::new(cfg, router_name)?;
    let router_ip = node_addr(cfg, name, &transit.router).await?;
    let router = &transit.router;

    // PIM and mroute state are live; the requested paths come from the record.
    let pim = match router_pim_ifaces(&router_ip) {
        Ok(ifaces) => (
            format!("pim on {router}:{}", transit.iif),
            ifaces.contains(&transit.iif),
        ),
        Err(e) => (format!("pim on {router}:{}: {e:#}", transit.iif), false),
    };

    let mroute_cmd = vtysh_show("show ip mroute json");
    let mroutes = ssh_try_capture(&router_ip, &mroute_cmd).map_err(|e| {
        format!("{:#}", router_read_err(&router_ip, &mroute_cmd, e))
    });

    let checks = addrs.iter().map(|addr| {
        let Some(route) = state.and_then(|state| state.route(addr)) else {
            return (
                format!("mroute on {router}:{addr}: no ownership record"),
                false,
            );
        };
        let Some(recorded_source) = route.source else {
            return (
                format!("mroute on {router}:{addr}: no recorded source"),
                false,
            );
        };
        let links = steering.selected_links(addr, router);
        let expected = transit.selected(links.as_deref());
        let text = match &mroutes {
            Ok(text) => text,
            Err(error) => {
                return (format!("mroute on {router}:{addr}: {error}"), false);
            }
        };
        mroute_status(
            text,
            &mroute_cmd,
            router,
            &transit.iif,
            expected,
            addr,
            &recorded_source,
        )
    });
    Ok(std::iter::once(pim).chain(checks).collect())
}

/// Compare one live FRR forwarding entry with the requested path.
fn mroute_status(
    text: &str,
    cmd: &str,
    router: &str,
    iif: &str,
    mut expected: Vec<String>,
    addr: &Ipv4Addr,
    source: &Ipv4Addr,
) -> (String, bool) {
    expected.sort();
    let res = match Mroute::parse(text, addr, source) {
        Ok(route) => route,
        Err(e) => {
            return (
                format!("mroute on {router}:{addr}: `{cmd}`: {e:#}"),
                false,
            );
        }
    };
    let ok = Mroute::matches(res.as_ref(), iif, &expected);
    (
        format!(
            "mroute on {router}:{addr} -> {}",
            if expected.is_empty() {
                "none".to_string()
            } else {
                expected.join(" ")
            }
        ),
        ok,
    )
}

struct PlumbingStatus {
    checks: Vec<(String, bool)>,
    steering: Steering,
    addrs: Vec<Ipv4Addr>,
    routers: Vec<String>,
}

/// Every piece of plumbing for `groups`, each paired with whether it is in
/// place: the per-group host route once, then PIM and the group's assignment
/// on each fabric router.
async fn plumbing_status(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[GroupSpec],
) -> anyhow::Result<PlumbingStatus> {
    let addrs = group_addrs(groups);
    ensure!(!addrs.is_empty(), "no multicast groups were given");
    let nexthop = ce_nexthop(cfg, name).await?;
    let routers = fabric_routers(cfg)?;
    let state = read_multicast_state(name)?;
    if let Some(state) = state.as_ref() {
        state.ensure_topology_exists(&addrs, &routers, cfg.scrimlet_count())?;
    }
    let mut steering = Steering::from_recorded(state.as_ref(), &addrs);
    steering.finalize(cfg, &addrs, &routers);

    let entries = route_entries()?;

    let mut out = vec![(
        "host checksum offload disabled".to_string(),
        dohwcksum() == Some(0),
    )];
    out.extend(addrs.iter().map(|addr| {
        (
            format!("host route {addr} -> {nexthop}"),
            host_route_gateways(&entries, addr).contains(&nexthop),
        )
    }));

    for addr in &addrs {
        let recorded_source = state
            .as_ref()
            .and_then(|state| state.route(addr))
            .and_then(|route| route.source);
        let check = match multicast_source(addr) {
            Ok(source) => (
                format!("host source for {addr} -> {source}"),
                Some(source) == recorded_source,
            ),
            Err(error) => (format!("host source for {addr}: {error:#}"), false),
        };
        out.push(check);
    }

    for router in &routers {
        out.extend(
            plumbing_status_one(
                cfg,
                name,
                router,
                &addrs,
                &steering,
                state.as_ref(),
            )
            .await?,
        );
    }
    Ok(PlumbingStatus { checks: out, steering, addrs, routers })
}

/// When the commtest preflight found missing or unreachable through to the
/// rack.
pub(crate) struct MissingPlumbing {
    /// Plumbing items not in place (one line each).
    pub(crate) missing: Vec<String>,
    /// Groups whose steering names no path into the target rack. These
    /// fail the preflight process, even when their plumbing is complete.
    pub(crate) unreached: Vec<Ipv4Addr>,
}

/// The pieces of plumbing missing for `groups`, as human-readable items.
/// Empty here means the path is complete.
///
/// This is the `commtest` preflight's view. A run with missing plumbing
/// fails here before any traffic is sent.
pub(crate) async fn missing_plumbing(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[GroupSpec],
    rack: usize,
) -> anyhow::Result<MissingPlumbing> {
    let _multicast_lock = lock_multicast_routes()?;
    let PlumbingStatus { checks, steering, addrs, routers } =
        plumbing_status(cfg, name, groups).await?;
    Ok(MissingPlumbing {
        missing: checks
            .into_iter()
            .filter_map(|(item, ok)| (!ok).then_some(item))
            .collect(),
        unreached: steering.unreached_groups(cfg, &addrs, &routers, rack),
    })
}

/// The groups this environment recorded, which is what a groupless `check`
/// asserts. Router-observed groups are left out: a group with no
/// record belongs to whoever put it there, and holding it to this
/// environment's nexthop and sender would fail it for no reason.
fn recorded_groups(name: &str) -> anyhow::Result<Vec<GroupSpec>> {
    Ok(state_groups(read_multicast_state(name)?.as_ref())
        .into_iter()
        .map(|addr| GroupSpec { addr })
        .collect())
}

/// Assert the whole external host path is live, printing one line per item.
///
/// With no groups given, this covers everything voxel has plumbed, the same
/// set a groupless `down` tears down, so any sequence of `up` runs is
/// asserted whole.
///
/// # Errors
///
/// This fails when any item is missing, so the CLI exit code reflects the
/// result.
pub(crate) async fn check(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[GroupSpec],
) -> anyhow::Result<()> {
    let _multicast_lock = lock_multicast_routes()?;
    let groups = if groups.is_empty() {
        // Only what this environment recorded.
        let plumbed = recorded_groups(name)?;
        if plumbed.is_empty() {
            println!("check: nothing plumbed");
            return Ok(());
        }
        plumbed
    } else {
        groups.to_vec()
    };
    let status = plumbing_status(cfg, name, &groups).await?;

    for (item, ok) in &status.checks {
        if *ok {
            println!("ok:      {item}");
        } else {
            println!("MISSING: {item}");
        }
    }

    if status.checks.iter().all(|(_, ok)| *ok) {
        println!("check: PASS");
        Ok(())
    } else {
        bail!("check: FAIL")
    }
}
